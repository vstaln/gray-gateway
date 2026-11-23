//! Discord conversation identity and session reset policy.
//!
//! A Discord channel is already a separate session when the message arrives in
//! one of its threads: Discord sends the thread's channel id. Guild messages
//! additionally use a per-user key, a secure group default; DMs
//! stay keyed by their one-to-one channel.

use chrono::{DateTime, Local, TimeZone, Timelike};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResetReason {
    /// A pre-policy session had no activity timestamp. It is reset once when
    /// an enabled policy first sees it, then follows the normal policy.
    Legacy,
    Idle,
    Daily,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetPolicy {
    pub mode: String,
    pub idle_minutes: u64,
    pub at_hour: u32,
}

impl Default for ResetPolicy {
    fn default() -> Self {
        Self {
            mode: "both".into(),
            idle_minutes: 1_440,
            at_hour: 4,
        }
    }
}

impl ResetPolicy {
    pub fn from_config(config: &Value) -> Self {
        let raw = config.get("session_reset");
        let mut out = Self::default();
        if let Some(policy) = raw {
            if let Some(mode) = policy.get("mode").and_then(Value::as_str) {
                out.mode = mode.to_string();
            }
            if let Some(minutes) = policy.get("idle_minutes").and_then(Value::as_u64) {
                out.idle_minutes = minutes;
            }
            if let Some(hour) = policy.get("at_hour").and_then(Value::as_u64) {
                out.at_hour = hour as u32;
            }
        }
        out
    }

    /// Whether the current private home should start fresh before this turn.
    pub fn reset_reason(&self, state: &Value, has_session: bool, now: f64) -> Option<ResetReason> {
        if self.mode == "none" || !has_session {
            return None;
        }
        let Some(last) = state.get("last_activity").and_then(Value::as_f64) else {
            return Some(ResetReason::Legacy);
        };
        if matches!(self.mode.as_str(), "idle" | "both")
            && (now - last).max(0.0) >= self.idle_minutes.saturating_mul(60) as f64
        {
            return Some(ResetReason::Idle);
        }
        if matches!(self.mode.as_str(), "daily" | "both") && self.daily_boundary_passed(last, now) {
            return Some(ResetReason::Daily);
        }
        None
    }

    fn daily_boundary_passed(&self, last: f64, now: f64) -> bool {
        let Some(now_local) = local_at(now) else {
            return false;
        };
        let Some(last_local) = local_at(last) else {
            return false;
        };
        let today = now_local.date_naive();
        let boundary_date = if now_local.hour() >= self.at_hour {
            today
        } else {
            today - chrono::Duration::days(1)
        };
        let Some(boundary) = boundary_date.and_hms_opt(self.at_hour, 0, 0) else {
            return false;
        };
        last_local.naive_local() < boundary
    }
}

fn local_at(timestamp: f64) -> Option<DateTime<Local>> {
    if !timestamp.is_finite() || timestamp < i64::MIN as f64 || timestamp > i64::MAX as f64 {
        return None;
    }
    Local
        .timestamp_opt(timestamp as i64, 0)
        .single()
        .or_else(|| Local.timestamp_opt(timestamp as i64, 0).earliest())
}

/// Build the session key for one inbound surface.
///
/// `channel_id` is the actual Discord message channel. For a message inside a
/// native Discord thread this is the thread id, so the thread is already a
/// distinct conversation. Guild chats add the author id to prevent users in
/// one channel from reading each other's transcript through the agent.
pub fn conversation_key(channel_id: &str, user_id: &str, is_dm: bool) -> String {
    if is_dm || user_id.trim().is_empty() {
        format!("chat:{channel_id}")
    } else {
        format!("chat:{channel_id}:user:{user_id}")
    }
}

/// Forget the transcript in one private conversation home and leave a fresh
/// generation marker behind. The marker prevents a turn that was already in
/// flight during `/new` from restoring its old session pointer when it exits.
pub fn reset_home(home: &Path, now: f64) -> Result<(), String> {
    let sessions = home.join("sessions");
    if sessions.is_dir() {
        let stamp = local_at(now)
            .map(|t| t.format("%Y%m%d-%H%M%S").to_string())
            .unwrap_or_else(|| format!("{}", now as i64));
        let mut archive = home.join("sessions-archive").join(&stamp);
        let mut n = 1;
        while archive.exists() {
            n += 1;
            archive = home.join("sessions-archive").join(format!("{stamp}-{n}"));
        }
        archive_jsonl(&sessions, &archive)?;
    }
    let state = json!({
        "generation": crate::channels::discord::durable::uuid_hex(),
        "last_activity": now
    });
    crate::channels::discord::config::atomic_json(&home.join("session.json"), &state)
}

fn archive_jsonl(dir: &Path, archive: &Path) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|_| "cannot list conversation sessions".to_string())?;
    for entry in entries {
        let entry = entry.map_err(|_| "cannot list conversation sessions".to_string())?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|_| "cannot inspect conversation session".to_string())?;
        if file_type.is_dir() {
            archive_jsonl(&path, &archive.join(entry.file_name()))?;
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            std::fs::create_dir_all(archive)
                .map_err(|_| "cannot create conversation session archive".to_string())?;
            let dest = archive.join(entry.file_name());
            if std::fs::rename(&path, &dest).is_err() {
                std::fs::copy(&path, &dest)
                    .map_err(|_| "cannot archive conversation session".to_string())?;
                std::fs::remove_file(&path)
                    .map_err(|_| "cannot clear conversation session".to_string())?;
            }
        }
    }
    Ok(())
}

/// True when the on-disk state still belongs to the same conversation
/// generation. A reset advances this value while the old child is running.
pub fn generation_is_current(state_path: &Path, generation: &str) -> bool {
    std::fs::read(state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|state| {
            state
                .get("generation")
                .and_then(Value::as_str)
                .map(|current| current == generation)
        })
        .unwrap_or(false)
}
