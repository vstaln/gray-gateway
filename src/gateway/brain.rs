use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::deliver;
use super::event::{self, Event, Kind};
use super::sessions::Sessions;
use super::settings::Settings;
use super::turn::{TurnOutcome, TurnRequest, TurnRunner};
use crate::cron::store::Origin as Route;

pub const MAX_RECOVERY: u32 = 3;

const ORPHAN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Running {
    pub turn: String,
    pub key: String,
    pub events: Vec<Event>,
    pub started_at: i64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child: Option<ChildProc>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildProc {
    pub pid: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgid: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<u64>,
}

impl ChildProc {
    pub fn leader(pid: u32) -> Self {
        Self {
            pid,
            pgid: cfg!(unix).then_some(pid),
            start_time: super::pid::proc_start_time(pid),
        }
    }

    pub fn alive(&self) -> bool {
        if !super::pid::pid_alive(self.pid) || zombie(self.pid) {
            return false;
        }
        match self.start_time {
            Some(then) => super::pid::proc_start_time(self.pid) == Some(then),
            None => true,
        }
    }
}

pub fn stamp_child(record: &Path, child: ChildProc) {
    let Some(mut running) = std::fs::read(record)
        .ok()
        .and_then(|b| serde_json::from_slice::<Running>(&b).ok())
    else {
        return;
    };
    running.child = Some(child);
    if let Err(e) = crate::cron::store::atomic_write_json(record, &running) {
        log::warn!(
            "gateway: cannot stamp turn child into {}: {e:#}",
            record.display()
        );
    }
}

#[derive(Debug)]
pub struct Done {
    pub turn: String,
    pub key: String,
    pub kind: Kind,
    pub route: Option<Route>,
    pub outcome: TurnOutcome,
}

pub struct Brain {
    pub dir: PathBuf,
    pub settings: Settings,
    sessions: Sessions,
    runner: Arc<dyn TurnRunner>,

    busy: HashMap<String, (String, tokio::task::JoinHandle<()>)>,
    done_tx: tokio::sync::mpsc::UnboundedSender<Done>,
    done_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Done>>,
}

impl Brain {
    pub fn new(home: &Path, runner: Arc<dyn TurnRunner>) -> Self {
        let dir = super::state_dir(home);
        for sub in ["events", "running", "outbox"] {
            let _ = std::fs::create_dir_all(dir.join(sub));
        }
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            settings: Settings::load(&dir),
            sessions: Sessions::load(&dir),
            dir,
            runner,
            busy: HashMap::new(),
            done_tx,
            done_rx: Some(done_rx),
        }
    }

    pub fn take_done(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<Done> {
        self.done_rx.take().expect("take_done called once")
    }

    pub fn busy_keys(&self) -> usize {
        self.busy.len()
    }

    pub fn recover(&mut self) {
        for path in event::spool_files(&self.dir.join("running")) {
            let record = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Running>(&b).ok());
            let _ = std::fs::remove_file(&path);
            let Some(record) = record else { continue };
            reap_orphan(&self.dir, &record);
            for mut ev in record.events {
                ev.attempt += 1;
                if ev.attempt > MAX_RECOVERY {
                    super::activity::log(
                        &self.dir,
                        "abandoned",
                        serde_json::json!({"key": ev.key, "kind": ev.kind.as_str(),
                            "attempts": ev.attempt - 1, "text": excerpt(&ev.text, 200)}),
                    );
                    if !ev.kind.autonomous() {
                        let note = format!(
                            "I was interrupted {} times while working on your message and stopped retrying it:\n> {}",
                            ev.attempt - 1,
                            excerpt(&ev.text, 300)
                        );
                        let last = self.sessions.last_route(&ev.key);
                        deliver::decide(
                            &self.dir,
                            &self.settings,
                            &ev.key,
                            Kind::System,
                            ev.route.clone(),
                            last,
                            &note,
                        );
                    }
                    continue;
                }
                super::activity::log(
                    &self.dir,
                    "recovered",
                    serde_json::json!({"key": ev.key, "kind": ev.kind.as_str(), "attempt": ev.attempt}),
                );
                if let Err(e) = event::admit(&self.dir, &ev) {
                    log::warn!("gateway: cannot re-admit interrupted event: {e:#}");
                }
            }
        }
    }

    pub fn step(&mut self, now: i64) {
        self.settings = Settings::load(&self.dir);
        self.dispatch(now);
    }

    fn dispatch(&mut self, now: i64) {
        let paused = super::paused(&self.dir);

        let mut by_key: BTreeMap<(usize, String), Vec<(PathBuf, Event)>> = BTreeMap::new();
        let mut first_seen: HashMap<String, usize> = HashMap::new();
        for (path, ev) in event::pending(&self.dir) {
            if paused && ev.kind.autonomous() {
                continue;
            }
            let next = first_seen.len();
            let order = *first_seen.entry(ev.key.clone()).or_insert(next);
            by_key
                .entry((order, ev.key.clone()))
                .or_default()
                .push((path, ev));
        }
        for ((_, key), batch) in by_key {
            if self.busy.len() >= self.settings.max_turns.max(1) {
                break;
            }
            if self.busy.contains_key(&key) {
                continue;
            }
            self.start_turn(&key, batch, now);
        }
    }

    fn start_turn(&mut self, key: &str, batch: Vec<(PathBuf, Event)>, now: i64) {
        let turn = uuid::Uuid::new_v4().simple().to_string();
        let events: Vec<Event> = batch.iter().map(|(_, e)| e.clone()).collect();
        let record = Running {
            turn: turn.clone(),
            key: key.to_string(),
            events: events.clone(),
            started_at: now,
            child: None,
        };
        let record_path = self.dir.join("running").join(format!("{turn}.json"));
        if let Err(e) = crate::cron::store::atomic_write_json(&record_path, &record) {
            log::warn!("gateway: cannot write admission record, turn not started: {e:#}");
            return;
        }
        for (path, _) in &batch {
            let _ = std::fs::remove_file(path);
        }
        let kind = turn_kind(&events);

        let route = events.iter().rev().find_map(|e| e.route.clone());

        if let Some(r) = route
            .as_ref()
            .filter(|r| r.platform != super::outbox::LOCAL)
        {
            self.sessions.note_route(key, r);
        }
        let request = TurnRequest {
            key: key.to_string(),

            session_id: (!kind.autonomous())
                .then(|| self.sessions.session_id(key))
                .flatten(),
            prompt: compose_prompt(&events, now),
            route: route.clone().or_else(|| self.sessions.last_route(key)),
            kind,
            cwd: self
                .settings
                .workdir
                .clone()
                .or_else(dirs_home)
                .unwrap_or_else(|| PathBuf::from("/")),
            timeout: Duration::from_secs(self.settings.turn_timeout_secs.max(30)),
            running: Some(record_path),
        };
        super::activity::log(
            &self.dir,
            "turn_start",
            serde_json::json!({"key": key, "turn": turn, "kind": kind.as_str(),
                "events": events.len(), "text": excerpt(&events[events.len() - 1].text, 160)}),
        );
        let runner = self.runner.clone();
        let tx = self.done_tx.clone();
        let (t, k) = (turn.clone(), key.to_string());
        let task = tokio::spawn(async move {
            let outcome = runner.run(request).await;
            let _ = tx.send(Done {
                turn: t,
                key: k,
                kind,
                route,
                outcome,
            });
        });
        self.busy.insert(key.to_string(), (turn, task));
    }

    pub fn finish(&mut self, done: Done) {
        if self
            .busy
            .get(&done.key)
            .is_some_and(|(turn, _)| *turn == done.turn)
        {
            self.busy.remove(&done.key);
        }
        if let Some(sid) = done
            .outcome
            .session_id
            .as_ref()
            .filter(|_| !done.kind.autonomous())
        {
            self.sessions.set_session_id(&done.key, sid);
        }
        let last = self.sessions.last_route(&done.key);
        match &done.outcome.error {
            Some(err) => {
                super::activity::log(
                    &self.dir,
                    "turn_failed",
                    serde_json::json!({"key": done.key, "turn": done.turn,
                        "kind": done.kind.as_str(), "error": excerpt(err, 400)}),
                );
                if !done.kind.autonomous() {
                    let text = format!("⚠ I couldn't finish that: {}", excerpt(err, 400));
                    deliver::decide(
                        &self.dir,
                        &self.settings,
                        &done.key,
                        Kind::User,
                        done.route.clone(),
                        last,
                        &text,
                    );
                }
            }
            None => {
                super::activity::log(
                    &self.dir,
                    "turn_end",
                    serde_json::json!({"key": done.key, "turn": done.turn,
                        "kind": done.kind.as_str(), "chars": done.outcome.text.len()}),
                );
                deliver::decide(
                    &self.dir,
                    &self.settings,
                    &done.key,
                    done.kind,
                    done.route.clone(),
                    last,
                    &done.outcome.text,
                );
            }
        }
        let _ = std::fs::remove_file(self.dir.join("running").join(format!("{}.json", done.turn)));
    }

    pub fn abort_all(&mut self) {
        for (key, (turn, task)) in self.busy.drain() {
            log::info!("gateway: interrupting turn {turn} ({key}); it resumes on next start");
            task.abort();
        }
    }
}

fn reap_orphan(dir: &Path, record: &Running) {
    let Some(child) = record.child.as_ref().filter(|c| c.alive()) else {
        return;
    };
    let killed = kill_child(child);
    let deadline = std::time::Instant::now() + ORPHAN_GRACE;
    while killed && child.alive() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if !killed {
        log::warn!(
            "gateway: turn {} child {} outlived the last run and could not be killed",
            record.turn,
            child.pid
        );
    }
    super::activity::log(
        dir,
        "orphan_killed",
        serde_json::json!({"key": record.key, "turn": record.turn,
            "pid": child.pid, "killed": killed, "gone": !child.alive()}),
    );
}

#[cfg(unix)]
fn kill_child(child: &ChildProc) -> bool {
    if child.pid <= 1 || child.pid > i32::MAX as u32 || child.pid == std::process::id() {
        return false;
    }
    let pid = child.pid as libc::pid_t;

    unsafe {
        let group = child
            .pgid
            .and_then(|g| libc::pid_t::try_from(g).ok())
            .filter(|&g| g > 1 && libc::getpgid(pid) == g && g != libc::getpgrp());
        match group {
            Some(g) => libc::kill(-g, libc::SIGKILL) == 0,
            None => libc::kill(pid, libc::SIGKILL) == 0,
        }
    }
}

#[cfg(not(unix))]
fn kill_child(_child: &ChildProc) -> bool {
    false
}

fn zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat.rsplit_once(')')?.1.split_whitespace().next()?;
            Some(state == "Z" || state == "X")
        })
        .unwrap_or(false)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn turn_kind(events: &[Event]) -> Kind {
    if events.iter().any(|e| e.kind == Kind::User) {
        Kind::User
    } else {
        events.first().map(|e| e.kind).unwrap_or(Kind::System)
    }
}

pub fn compose_prompt(events: &[Event], now: i64) -> String {
    let interrupted = events.iter().any(|e| e.attempt > 0);
    let mut parts: Vec<String> = Vec::new();
    if interrupted {
        parts.push(
            "[gateway] The previous attempt at this was cut short by a restart. Check what was \
             already done (files, commands, messages) before redoing anything."
                .to_string(),
        );
    }
    if let [only] = events
        && only.kind == Kind::User
    {
        parts.push(only.text.clone());
        return parts.join("\n\n");
    }
    let users = events.iter().filter(|e| e.kind == Kind::User).count();
    if users > 1 {
        parts.push(format!(
            "[gateway] {users} messages arrived while you were busy; answer them together."
        ));
    }
    let when = chrono::DateTime::from_timestamp(now, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%a %Y-%m-%d %H:%M %:z")
                .to_string()
        })
        .unwrap_or_default();
    for e in events {
        parts.push(match e.kind {
            Kind::User => e.text.clone(),
            Kind::Trigger => format!(
                "[gateway · trigger · {when}] Not a message from the owner: an external \
                 trigger fired. Act on it if it needs action. If the owner does not need to \
                 hear about it, reply exactly NO_REPLY.\n\n{}",
                e.text
            ),
            other => format!("[gateway · {} · {when}]\n{}", other.as_str(), e.text),
        });
    }
    parts.join("\n\n---\n\n")
}

pub fn excerpt(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}
