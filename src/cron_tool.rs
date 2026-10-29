use serde_json::Value;

use crate::cron::schedule::{Schedule, next_run, parse_schedule};
use crate::cron::store::{JobState, Origin};
use crate::cron::{CronJob, CronStore, Deliver, Repeat, now_secs};

pub fn tool_def() -> Value {
    serde_json::json!({
        "name": "schedule",
        "description": concat!(
            "Schedule work for later; it comes back to this session. Create or replace a ",
            "job by `name`. `when` is a schedule (\"in 30m\", \"every 2h\", \"daily 9am\", ",
            "\"weekdays 9:00\", \"every monday 6pm\", 5-field cron, RFC3339) — or ",
            "\"off\"/\"on\" to pause/resume, \"delete\" to remove. At fire time `prompt` runs ",
            "as a fresh turn (`reminder: true` delivers it as literal text instead). Call with no ",
            "args to list jobs."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Job name (the upsert key)."},
                "when": {"type": "string", "description": "Schedule, or off/on/delete."},
                "prompt": {"type": "string", "description": "What to do at fire time. Required to create."},
                "reminder": {"type": "boolean", "description": "Deliver `prompt` as literal text, no turn."},
                "times": {"type": "integer", "minimum": 1, "description": "Stop after this many runs."}
            }
        },
        "preview": "name"
    })
}

pub struct ScheduleTool {
    pub home: std::path::PathBuf,
}

impl ScheduleTool {
    pub fn apply(&self, sid: Option<&str>, args: &Value) -> anyhow::Result<String> {
        let store = CronStore::open(self.home.join("cron"))?;
        let s = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let note = match (s("name"), s("when")) {
            (None, _) => String::new(),
            (Some(_), None) => {
                anyhow::bail!("`when` is required with `name`: a schedule, off, on or delete")
            }
            (Some(name), Some("delete")) => {
                anyhow::ensure!(store.remove(name)?, "no job named {name:?}");
                format!("deleted {name}")
            }
            (Some(name), Some(w @ ("off" | "on"))) => {
                anyhow::ensure!(store.set_paused(name, w == "off")?, "no job named {name:?}");
                format!("{name} {}", if w == "off" { "paused" } else { "resumed" })
            }
            (Some(name), Some(when)) => {
                upsert(&store, &self.home, sid, name, when, s("prompt"), args)?
            }
        };
        let jobs = store.list()?;
        let list = if jobs.is_empty() {
            "no jobs".to_string()
        } else {
            jobs.iter().map(job_line).collect::<Vec<_>>().join("\n")
        };
        Ok(if note.is_empty() {
            list
        } else {
            format!("{note}\n\n{list}")
        })
    }
}

fn upsert(
    store: &CronStore,
    home: &std::path::Path,
    sid: Option<&str>,
    name: &str,
    when: &str,
    prompt: Option<&str>,
    args: &Value,
) -> anyhow::Result<String> {
    let schedule = parse_schedule(when)?;
    let times = match args.get("times").and_then(Value::as_u64) {
        Some(t) => Some(u32::try_from(t.max(1))?),
        None => None,
    };
    let reminder = args.get("reminder").and_then(Value::as_bool);
    let verb = if store.get(name)?.is_some() {
        store.update(name, |j| {
            j.schedule = schedule;
            if let Some(p) = prompt {
                j.prompt = p.to_string();
            }
            if let Some(r) = reminder {
                j.reminder = r;
            }
            if times.is_some() {
                j.repeat = Repeat {
                    times,
                    completed: 0,
                };
            }

            (j.state, j.next_run_at) = (JobState::Active, next_run(now_secs(), &j.schedule));
            Ok(())
        })?;
        "updated"
    } else {
        let prompt =
            prompt.ok_or_else(|| anyhow::anyhow!("`prompt` is required to create {name:?}"))?;
        // Inside a chat-hosted home the job belongs to the chat; the REPL
        // session that created it is one turn long and nobody reads its inbox.
        let origin = crate::cron::chat_route::current(home).or_else(|| {
            sid.map(|sid| Origin {
                platform: crate::cron_serve::SESSION_PLATFORM.to_string(),
                chat: sid.to_string(),
                thread: None,
                route: None,
            })
        });
        let deliver = if origin.is_some() {
            Deliver::Origin
        } else {
            Deliver::Local
        };
        let reminder = reminder.unwrap_or(false);
        let id = store.add_full(
            name,
            when,
            prompt,
            deliver,
            origin,
            None,
            vec![],
            None,
            reminder,
        )?;
        if times.is_some() {
            store.update(&id, |j| {
                j.repeat.times = times;
                Ok(())
            })?;
        }
        "created"
    };
    Ok(format!("{verb} {name}"))
}

fn job_line(j: &CronJob) -> String {
    let state = match j.state {
        JobState::Active if j.enabled => "active",
        JobState::Active => "disabled",
        JobState::Paused => "paused",
        JobState::Done => "done",
    };
    let next = j
        .next_run_at
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| {
            format!(
                " · next {}",
                t.with_timezone(&chrono::Local).format("%a %b %-d %H:%M")
            )
        })
        .unwrap_or_default();
    let runs = match j.repeat.times {
        Some(t) => format!(" · {}/{t} runs", j.repeat.completed),
        None => String::new(),
    };
    format!(
        "{} · {} · {state}{next}{runs}",
        j.name,
        schedule_text(&j.schedule)
    )
}

pub fn schedule_text(s: &Schedule) -> String {
    match s {
        Schedule::Cron { expr } => format!("cron {expr}"),
        Schedule::Interval { secs } => match secs {
            s if s % 86_400 == 0 => format!("every {}d", s / 86_400),
            s if s % 3600 == 0 => format!("every {}h", s / 3600),
            s => format!("every {}m", s / 60),
        },
        Schedule::Once { .. } => "once".to_string(),
    }
}
