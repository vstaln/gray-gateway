use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredFire {
    pub id: String,
    pub name: String,
    pub path: std::path::PathBuf,
    pub excerpt: String,
    pub to_chat: bool,

    pub reminder: bool,

    pub failed: bool,

    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FireOutput {
    pub transcript: String,
    pub final_text: String,
}

pub struct TickReport {
    pub fired: usize,
    pub errors: usize,
    pub delivered: Vec<DeliveredFire>,
}

#[async_trait::async_trait(?Send)]
pub trait AsyncRunner {
    async fn run(&self, prompt: String, cwd: PathBuf) -> anyhow::Result<String>;

    async fn run_full(&self, prompt: String, cwd: PathBuf) -> anyhow::Result<FireOutput> {
        let text = self.run(prompt, cwd).await?;
        Ok(FireOutput {
            transcript: text.clone(),
            final_text: text,
        })
    }
}

pub struct SubprocessRunner {
    pub gray_bin: PathBuf,
    pub home: PathBuf,
}

#[async_trait::async_trait(?Send)]
impl AsyncRunner for SubprocessRunner {
    async fn run(&self, prompt: String, cwd: PathBuf) -> anyhow::Result<String> {
        Ok(self.run_full(prompt, cwd).await?.transcript)
    }

    async fn run_full(&self, prompt: String, cwd: PathBuf) -> anyhow::Result<FireOutput> {
        let mut cmd = tokio::process::Command::new(&self.gray_bin);
        cmd.arg("-p")
            .arg(&prompt)
            .arg("--json")
            .arg("--max-requests")
            .arg("200")
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .env("GRAY_HOME", &self.home)
            .env("GRAY_TURN_ORIGIN", "cron")
            .env("GRAY_SHOW_REASONING", "0");
        let out = cmd.output().await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let rows: Vec<serde_json::Value> = stdout
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let transcript = crate::cron_fire::transcript_text(&rows);
        let final_text = crate::cron_fire::final_assistant_text(&rows);
        if !out.status.success() && final_text.is_empty() {
            let wire = rows
                .iter()
                .find(|r| r.get("type").and_then(|t| t.as_str()) == Some("error"))
                .and_then(|r| r.get("message").and_then(|m| m.as_str()))
                .map(str::to_string);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let tail = stderr.trim();
            anyhow::bail!(
                "{}",
                wire.filter(|m| !m.is_empty()).unwrap_or_else(|| {
                    if tail.is_empty() {
                        format!("gray exited {}", out.status)
                    } else {
                        tail.chars().take(2000).collect()
                    }
                })
            );
        }
        Ok(FireOutput {
            transcript,
            final_text,
        })
    }
}

pub const FIRE_TIMEOUT_SECS: u64 = 600;

pub struct SaveLocalDeliver {
    pub home: PathBuf,
}

impl SaveLocalDeliver {
    pub async fn deliver_full(
        &self,
        job: &crate::cron::CronJob,
        now: i64,
        out: &FireOutput,
        elapsed_ms: u64,
    ) -> Result<DeliveredFire, String> {
        let transcript = crate::cron_fire::redact_secrets(&out.transcript, &self.home);
        let path = crate::cron_fire::write_local_output(&self.home, job, now, &transcript)
            .map_err(|e| format!("local write failed: {e:#}"))?;
        let shown = if job.reminder {
            out.final_text.clone()
        } else {
            crate::cron_fire::redact_secrets(&out.final_text, &self.home)
        };

        let excerpt = crate::cron_fire::delivery_excerpt(&shown);
        let saved = |to_chat: bool| DeliveredFire {
            id: job.id.clone(),
            name: job.name.clone(),
            path: path.clone(),
            excerpt: excerpt.clone(),
            to_chat,
            reminder: job.reminder,
            failed: false,
            elapsed_ms,
        };
        match &job.deliver {
            crate::cron::Deliver::Local => Ok(saved(false)),
            crate::cron::Deliver::Target(_) => {
                log::warn!(
                    "cron {}: unknown target {:?}, saved locally",
                    job.id,
                    job.deliver
                );
                Ok(saved(false))
            }
            crate::cron::Deliver::Origin => {
                let Some(origin) = &job.origin else {
                    log::warn!(
                        "cron {}: origin delivery without origin session, saved locally",
                        job.id
                    );
                    return Ok(saved(false));
                };

                // A job made by the schedule tool inside a chat-hosted home
                // before it knew about chats is bound to a one-turn REPL
                // session; send it to the home's chat instead.
                let rerouted = (origin.platform == SESSION_PLATFORM)
                    .then(|| crate::cron::chat_route::from_home(&self.home))
                    .flatten();
                let origin = rerouted.as_ref().unwrap_or(origin);
                if origin.platform == SESSION_PLATFORM {
                    post_to_session_inbox(&self.home, &origin.chat, &saved(true))
                        .map_err(|e| format!("session inbox write failed: {e:#}"))?;
                    return Ok(saved(false));
                }

                let note = crate::cron_fire::mirror_message(&job.name, &excerpt);
                let (home, chat) = (self.home.clone(), origin.chat.clone());
                let appended = tokio::task::spawn_blocking(move || {
                    crate::vendor::append_injected_user(&home, &chat, &note)
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("append task failed: {e}")));
                if let Err(e) = appended {
                    log::warn!(
                        "cron {}: no session {:?} to mirror into ({e:#}); delivering anyway",
                        job.id,
                        origin.chat
                    );
                }
                if is_hosted(origin) {
                    post_to_outbox(&self.home, &saved(true), origin)
                        .map_err(|e| format!("outbox write failed: {e:#}"))?;
                }
                Ok(saved(true))
            }
        }
    }
}

pub fn owner_stamp() -> String {
    format!("{}:{}", std::process::id(), uuid::Uuid::new_v4())
}

pub enum Prepared {
    Done(crate::cron::RunStatus, Option<DeliveredFire>),
    Run(ReadyFire),
}

pub struct ReadyFire {
    pub job: crate::cron::CronJob,
    pub prompt: String,
    pub workdir: PathBuf,
    pub input: Option<String>,
}

fn mark_failed(store: &crate::cron::CronStore, job: &crate::cron::CronJob, msg: &str) {
    let _ = store.mark_done(
        &job.id,
        job.fire_claim.as_ref(),
        crate::cron::RunStatus::Error,
        Some(msg),
    );
}

pub async fn fire_one(
    store: &crate::cron::CronStore,
    runner: &dyn AsyncRunner,
    job: crate::cron::CronJob,
    now: i64,
    deliver: &SaveLocalDeliver,
) -> (crate::cron::RunStatus, Option<DeliveredFire>) {
    match prepare(store, job, now, deliver).await {
        Prepared::Done(status, saved) => (status, saved),
        Prepared::Run(ready) => {
            let started = std::time::Instant::now();
            let out = run_guarded(
                runner,
                ready.prompt.clone(),
                ready.workdir.clone(),
                FIRE_TIMEOUT_SECS,
            )
            .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            finish(store, deliver, ready, now, out, elapsed_ms).await
        }
    }
}

pub async fn prepare(
    store: &crate::cron::CronStore,
    job: crate::cron::CronJob,
    now: i64,
    deliver: &SaveLocalDeliver,
) -> Prepared {
    use crate::cron::RunStatus;
    let fail = |msg: String| {
        mark_failed(store, &job, &msg);
        Prepared::Done(RunStatus::Error, None)
    };
    if job.reminder {
        let out = FireOutput {
            transcript: job.prompt.clone(),
            final_text: job.prompt.clone(),
        };
        return match deliver.deliver_full(&job, now, &out, 0).await {
            Ok(saved) => {
                let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
                Prepared::Done(RunStatus::Ok, Some(saved))
            }
            Err(msg) => {
                let _ = store.mark_done(
                    &job.id,
                    job.fire_claim.as_ref(),
                    RunStatus::DeliveryFailed,
                    Some(&msg),
                );
                Prepared::Done(RunStatus::DeliveryFailed, None)
            }
        };
    }
    let workdir: PathBuf = match &job.workdir {
        Some(w) => w.clone(),
        None => match std::env::current_dir() {
            Ok(c) => c,
            Err(e) => return fail(format!("cannot resolve workdir: {e:#}")),
        },
    };
    if let Some(s) = &job.script
        && (!s.is_absolute() || !s.is_file())
    {
        return fail(format!("pre-script missing: {}", s.display()));
    }
    let mut skill_paths = Vec::new();
    for name in &job.skills {
        match crate::vendor::resolve_skill_name(&workdir, name) {
            Some(p) => skill_paths.push(p),
            None => return fail(format!("skill not found: {name}")),
        }
    }
    let mut script_stdout: Option<String> = None;
    let mut input = None;
    if let Some(s) = &job.script {
        let outcome = crate::cron_fire::run_pre_script(s, &workdir).await;
        if !outcome.ok {
            return fail(format!("pre-script failed: {}", outcome.stderr_tail));
        }
        if !crate::cron_fire::parse_wake_gate(&outcome.stdout) {
            let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
            return Prepared::Done(RunStatus::Ok, None);
        }
        let digest =
            crate::cron_digest::digest(&[&job.prompt, &job.skills.join("\n"), &outcome.stdout]);
        let history = crate::cron_digest::load(store.dir(), &job.id);
        if history.input_unchanged(&digest) {
            let output = history.last().and_then(|r| r.output.clone());
            crate::cron_digest::record(
                store.dir(),
                &job.id,
                crate::cron_digest::Run {
                    at: now,
                    status: "skipped".to_string(),
                    input: Some(digest),
                    output,
                },
            );
            let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
            return Prepared::Done(RunStatus::Ok, None);
        }
        input = Some(digest);
        script_stdout = Some(outcome.stdout);
    }
    let prompt =
        crate::cron_fire::assemble_fire_prompt(&job.prompt, &skill_paths, script_stdout.as_deref());
    if let Some(session) = repl_session(&deliver.home, &job) {
        let fired = DeliveredFire {
            id: job.id.clone(),
            name: job.name.clone(),
            path: PathBuf::new(),
            excerpt: prompt,
            to_chat: false,
            reminder: false,
            failed: false,
            elapsed_ms: 0,
        };
        return match post_to_session_inbox(&deliver.home, &session, &fired) {
            Ok(()) => {
                let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
                Prepared::Done(RunStatus::Ok, Some(fired))
            }
            Err(e) => fail(format!("session inbox write failed: {e:#}")),
        };
    }
    Prepared::Run(ReadyFire {
        job,
        prompt,
        workdir,
        input,
    })
}

pub async fn run_guarded(
    runner: &dyn AsyncRunner,
    prompt: String,
    workdir: PathBuf,
    secs: u64,
) -> Result<FireOutput, String> {
    let run_fut = std::panic::AssertUnwindSafe(runner.run_full(prompt, workdir));
    match tokio::time::timeout(
        std::time::Duration::from_secs(secs),
        futures::FutureExt::catch_unwind(run_fut),
    )
    .await
    {
        Err(_) => Err(format!("fire exceeded {secs}s")),
        Ok(Err(_)) => Err("agent run panicked".to_string()),
        Ok(Ok(Err(e))) => Err(format!("agent run failed: {e:#}")),
        Ok(Ok(Ok(out))) => Ok(out),
    }
}

pub async fn finish(
    store: &crate::cron::CronStore,
    deliver: &SaveLocalDeliver,
    ready: ReadyFire,
    now: i64,
    out: Result<FireOutput, String>,
    elapsed_ms: u64,
) -> (crate::cron::RunStatus, Option<DeliveredFire>) {
    use crate::cron::RunStatus;
    let ReadyFire { job, input, .. } = ready;
    let history = crate::cron_digest::load(store.dir(), &job.id);
    let note = |status: &str, output: Option<String>| {
        crate::cron_digest::record(
            store.dir(),
            &job.id,
            crate::cron_digest::Run {
                at: now,
                status: status.to_string(),
                input: input.clone(),
                output,
            },
        )
    };
    let out = match out {
        Err(msg) => {
            let digest = crate::cron_digest::digest(&[&msg]);
            let repeat = history.output_unchanged(&digest, true);
            note("error", Some(digest));
            mark_failed(store, &job, &msg);
            let shown = if repeat {
                None
            } else {
                failure_delivery(deliver, &job, now, &msg)
            };
            return (RunStatus::Error, shown);
        }
        Ok(out) => out,
    };
    if crate::cron_fire::is_silent_response(&out.final_text) {
        note("silent", None);
        let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
        return (RunStatus::Ok, None);
    }
    let digest = crate::cron_digest::digest(&[&out.final_text]);
    if history.output_unchanged(&digest, false) {
        note("unchanged", Some(digest));
        let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
        return (RunStatus::Ok, None);
    }
    match deliver.deliver_full(&job, now, &out, elapsed_ms).await {
        Ok(saved) => {
            note("ok", Some(digest));
            let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
            (RunStatus::Ok, Some(saved))
        }
        Err(msg) => {
            note("delivery_failed", Some(digest));
            let _ = store.mark_done(
                &job.id,
                job.fire_claim.as_ref(),
                RunStatus::DeliveryFailed,
                Some(&msg),
            );
            (RunStatus::DeliveryFailed, None)
        }
    }
}

pub const MAX_BATCH: usize = 4;

pub async fn run_group(
    store: &crate::cron::CronStore,
    runner: &dyn AsyncRunner,
    deliver: &SaveLocalDeliver,
    group: Vec<ReadyFire>,
    now: i64,
) -> Vec<(crate::cron::RunStatus, Option<DeliveredFire>)> {
    let started = std::time::Instant::now();
    if group.len() == 1 {
        let ready = group.into_iter().next().expect("one fire");
        let out = run_guarded(
            runner,
            ready.prompt.clone(),
            ready.workdir.clone(),
            FIRE_TIMEOUT_SECS,
        )
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        return vec![finish(store, deliver, ready, now, out, elapsed_ms).await];
    }
    let tasks: Vec<(String, String)> = group
        .iter()
        .map(|r| (r.job.id.clone(), r.prompt.clone()))
        .collect();
    let ids: Vec<String> = tasks.iter().map(|(id, _)| id.clone()).collect();
    let prompt = crate::cron_fire::assemble_batch_prompt(&tasks);
    let cap = (crate::cron::store::FIRE_CLAIM_TTL_SECS - 60).max(FIRE_TIMEOUT_SECS as i64) as u64;
    let secs = (FIRE_TIMEOUT_SECS * group.len() as u64).min(cap);
    let out = run_guarded(runner, prompt, group[0].workdir.clone(), secs).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let parts: Vec<Result<FireOutput, String>> = match out {
        Err(msg) => vec![Err(msg); group.len()],
        Ok(out) => crate::cron_fire::split_batch_reply(&out.final_text, &ids)
            .into_iter()
            .map(|section| {
                let final_text = section.unwrap_or_else(|| out.final_text.clone());
                Ok(FireOutput {
                    transcript: out.transcript.clone(),
                    final_text,
                })
            })
            .collect(),
    };
    let mut results = Vec::new();
    for (ready, part) in group.into_iter().zip(parts) {
        results.push(finish(store, deliver, ready, now, part, elapsed_ms).await);
    }
    results
}

fn failure_delivery(
    deliver: &SaveLocalDeliver,
    job: &crate::cron::CronJob,
    now: i64,
    msg: &str,
) -> Option<DeliveredFire> {
    if !matches!(job.deliver, crate::cron::Deliver::Origin) || job.origin.is_none() {
        return None;
    }
    let msg = crate::cron_fire::redact_secrets(msg, &deliver.home);
    let path = crate::cron_fire::write_local_output(&deliver.home, job, now, &msg).ok()?;
    let fired = DeliveredFire {
        id: job.id.clone(),
        name: job.name.clone(),
        path,
        excerpt: msg.lines().next().unwrap_or("").chars().take(300).collect(),
        to_chat: true,
        reminder: job.reminder,
        failed: true,
        elapsed_ms: 0,
    };

    if let Some(origin) = job
        .origin
        .as_ref()
        .filter(|o| o.platform == SESSION_PLATFORM)
    {
        if let Err(e) = post_to_session_inbox(&deliver.home, &origin.chat, &fired) {
            log::warn!("cron {}: session inbox write failed: {e:#}", job.id);
        }
        return None;
    }
    if let Some(origin) = job.origin.as_ref().filter(|o| is_hosted(o))
        && let Err(e) = post_to_outbox(&deliver.home, &fired, origin)
    {
        log::warn!("cron {}: outbox write failed: {e:#}", job.id);
    }
    Some(fired)
}

pub const SESSION_PLATFORM: &str = "repl";

fn repl_session(home: &std::path::Path, job: &crate::cron::CronJob) -> Option<String> {
    let origin = job.origin.as_ref()?;
    (matches!(job.deliver, crate::cron::Deliver::Origin)
        && origin.platform == SESSION_PLATFORM
        && crate::cron::chat_route::from_home(home).is_none())
    .then(|| origin.chat.clone())
}

pub fn is_hosted(origin: &crate::cron::store::Origin) -> bool {
    origin.platform != SESSION_PLATFORM && origin.platform != "local"
}

fn post_to_outbox(
    home: &std::path::Path,
    fired: &DeliveredFire,
    origin: &crate::cron::store::Origin,
) -> anyhow::Result<()> {
    write_entry(
        &home.join("cron").join("outbox"),
        &delivery_json(fired, Some(origin)),
    )
}

pub fn drain_outbox(home: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(home.join("cron").join("outbox")) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path);
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("cron outbox: cannot remove {}: {e}", path.display());
            continue;
        }
        match text {
            Ok(t) if !t.trim().is_empty() => out.push(t.trim().to_string()),
            _ => log::warn!("cron outbox: dropped unreadable {}", path.display()),
        }
    }
    out
}

pub(crate) fn session_inbox(home: &std::path::Path, session: &str) -> Option<PathBuf> {
    let safe = !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    safe.then(|| home.join("cron").join("inbox").join(session))
}

fn post_to_session_inbox(
    home: &std::path::Path,
    session: &str,
    fired: &DeliveredFire,
) -> anyhow::Result<()> {
    let dir = session_inbox(home, session)
        .ok_or_else(|| anyhow::anyhow!("unsafe session id {session:?}"))?;
    let body = if fired.failed {
        format!("(failed) {}", fired.excerpt)
    } else {
        fired.excerpt.clone()
    };

    drop_stale_entries(&dir, &fired.id, &fired.name);
    let entry = serde_json::json!({
        "job_id": fired.id,
        "card": format!("cron: {}", format_fire_chat(fired)),
        "prompt": crate::cron_fire::mirror_message(&fired.name, &body),
        "name": fired.name,
        "body": fired.excerpt,
        "status": if fired.failed { "failed" } else { "ok" },
        "reminder": fired.reminder,
        "elapsed_ms": fired.elapsed_ms,
    });
    write_entry(&dir, &entry.to_string())
}

fn drop_stale_entries(dir: &std::path::Path, id: &str, name: &str) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(Result::ok) {
        let path = e.path();
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Some(v) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        else {
            continue;
        };
        let field = |k: &str| v.get(k).and_then(|x| x.as_str());
        let is_cron = field("card").is_some_and(|c| c.starts_with("cron: "));
        let same = match field("job_id") {
            Some(j) => j == id,
            None => field("name") == Some(name),
        };
        if is_cron && same {
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub(crate) fn write_entry(dir: &std::path::Path, body: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{stamp:020}-{}.json", uuid::Uuid::new_v4());
    let tmp = dir.join(format!(".{name}.tmp"));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(&tmp)?, body.as_bytes())?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

pub const MAX_CONCURRENT_FIRES: usize = 4;

pub const MAX_DUE_PER_TICK: usize = 8;

pub const BATCH_WINDOW_SECS: i64 = 120;

pub async fn tick_once(
    store: &crate::cron::CronStore,
    runner: &dyn AsyncRunner,
    deliver: &SaveLocalDeliver,
    kind: &str,
) -> anyhow::Result<TickReport> {
    tick_once_with(
        store,
        runner,
        deliver,
        kind,
        crate::vendor::cron_auto_enabled(),
    )
    .await
}

pub(crate) async fn tick_once_with(
    store: &crate::cron::CronStore,
    runner: &dyn AsyncRunner,
    deliver: &SaveLocalDeliver,
    kind: &str,
    auto: bool,
) -> anyhow::Result<TickReport> {
    if let Err(e) = store.record_tick(kind) {
        log::warn!("cron: cannot record tick heartbeat: {e:#}");
    }

    if !auto {
        return Ok(TickReport {
            fired: 0,
            errors: 0,
            delivered: Vec::new(),
        });
    }
    let owner = owner_stamp();
    let mut report = TickReport {
        fired: 0,
        errors: 0,
        delivered: Vec::new(),
    };
    let now = crate::cron::now_secs();
    let due = store.claim_due_window(now, &owner, MAX_DUE_PER_TICK, &[], BATCH_WINDOW_SECS)?;
    let prepared =
        futures::future::join_all(due.into_iter().map(|job| prepare(store, job, now, deliver)))
            .await;

    let mut ordered: Vec<Option<(crate::cron::RunStatus, Option<DeliveredFire>)>> = Vec::new();
    ordered.resize_with(prepared.len(), || None);
    let mut groups: Vec<Vec<(usize, ReadyFire)>> = Vec::new();
    for (idx, p) in prepared.into_iter().enumerate() {
        match p {
            Prepared::Done(status, saved) => ordered[idx] = Some((status, saved)),
            Prepared::Run(ready) => match groups
                .iter_mut()
                .find(|g| g.len() < MAX_BATCH && g[0].1.workdir == ready.workdir)
            {
                Some(g) => g.push((idx, ready)),
                None => groups.push(vec![(idx, ready)]),
            },
        }
    }
    let runs = futures::StreamExt::collect::<Vec<_>>(futures::StreamExt::buffer_unordered(
        futures::stream::iter(groups.into_iter().map(|g| async move {
            let (idxs, fires): (Vec<usize>, Vec<ReadyFire>) = g.into_iter().unzip();
            let outs = run_group(store, runner, deliver, fires, now).await;
            idxs.into_iter().zip(outs).collect::<Vec<_>>()
        })),
        MAX_CONCURRENT_FIRES,
    ))
    .await;
    for (idx, out) in runs.into_iter().flatten() {
        ordered[idx] = Some(out);
    }
    for (status, saved) in ordered.into_iter().flatten() {
        report.fired += 1;
        if !matches!(status, crate::cron::RunStatus::Ok) {
            report.errors += 1;
        }
        if let Some(saved) = saved {
            report.delivered.push(saved);
        }
    }
    Ok(report)
}

pub fn format_fire_chat(saved: &DeliveredFire) -> String {
    crate::cron_fire::format_delivery_plain(
        &saved.name,
        &saved.excerpt,
        saved.reminder,
        saved.failed,
    )
}

pub fn delivery_json(saved: &DeliveredFire, origin: Option<&crate::cron::store::Origin>) -> String {
    let (platform, chat, thread, route) = match origin {
        Some(o) => (
            o.platform.clone(),
            o.chat.clone(),
            o.thread.clone(),
            o.route.clone(),
        ),
        None => (String::new(), String::new(), None, None),
    };
    serde_json::json!({
        "type": "cron_delivery",
        "job_id": saved.id,
        "name": saved.name,
        "platform": platform,
        "chat": chat,
        "thread": thread,
        "route": route,
        "kind": if saved.reminder { "reminder" } else { "task" },
        "status": if saved.failed { "failed" } else { "ok" },
        "elapsed_ms": saved.elapsed_ms,
        "final_text": saved.excerpt,
        "text": format_fire_chat(saved),
        "path": saved.path.display().to_string(),
    })
    .to_string()
}

pub async fn serve_loop(
    store: crate::cron::CronStore,
    deliver: SaveLocalDeliver,
    runner: impl AsyncRunner + 'static,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = interval.tick() => {
                match tick_once(&store, &runner, &deliver, "serve").await {
                    Ok(rep) => log::info!("cron tick: fired={} errors={}", rep.fired, rep.errors),
                    Err(e) => log::warn!("cron tick failed: {e:#}"),
                }
            }
        }
    }
    Ok(())
}

#[path = "cron_serve_tests.rs"]
#[cfg(test)]
mod tests;
