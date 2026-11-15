use std::path::{Path, PathBuf};

pub const HISTORY_KEEP: usize = 8;
pub const MAX_CONSECUTIVE_SKIPS: usize = 5;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Run {
    pub at: i64,
    pub status: String,
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct History {
    #[serde(default)]
    pub runs: Vec<Run>,
}

pub fn digest(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            h ^= 0xff;
            h = h.wrapping_mul(0x100000001b3);
        }
        for b in part.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("{h:016x}")
}

fn path(cron_dir: &Path, id: &str) -> Option<PathBuf> {
    let safe = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    safe.then(|| cron_dir.join("history").join(format!("{id}.json")))
}

pub fn load(cron_dir: &Path, id: &str) -> History {
    path(cron_dir, id)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn record(cron_dir: &Path, id: &str, run: Run) {
    let Some(p) = path(cron_dir, id) else { return };
    let mut h = load(cron_dir, id);
    h.runs.push(run);
    let drop = h.runs.len().saturating_sub(HISTORY_KEEP);
    h.runs.drain(..drop);
    if let Err(e) = crate::cron::store::atomic_write_json(&p, &h) {
        log::warn!("cron {id}: cannot write history: {e:#}");
    }
}

pub fn remove(cron_dir: &Path, id: &str) {
    if let Some(p) = path(cron_dir, id) {
        let _ = std::fs::remove_file(p);
    }
}

impl History {
    pub fn last(&self) -> Option<&Run> {
        self.runs.last()
    }

    pub fn input_unchanged(&self, input: &str) -> bool {
        let Some(last) = self.last() else {
            return false;
        };
        let reusable = matches!(
            last.status.as_str(),
            "ok" | "silent" | "unchanged" | "skipped"
        );
        let skips = self
            .runs
            .iter()
            .rev()
            .take_while(|r| r.status == "skipped")
            .count();
        reusable && last.input.as_deref() == Some(input) && skips < MAX_CONSECUTIVE_SKIPS
    }

    pub fn output_unchanged(&self, output: &str, failed: bool) -> bool {
        let Some(last) = self.last() else {
            return false;
        };
        let same_kind = if failed {
            last.status == "error"
        } else {
            matches!(last.status.as_str(), "ok" | "unchanged" | "skipped")
        };
        same_kind && last.output.as_deref() == Some(output)
    }
}
