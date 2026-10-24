//! Read-only port of the bits of core's `JsonlSessionStore` the CLI needs:
//! session-id validation, unique-prefix resolution, and `.open` lock probes.

use std::path::{Path, PathBuf};

/// Core's `valid_session_id`.
pub fn valid_session_id(s: &str) -> bool {
    if s.is_empty()
        || s.len() > 128
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if matches!(lower.as_str(), "con" | "prn" | "aux" | "nul") {
        return false;
    }
    if lower.len() == 4
        && (lower.starts_with("com") || lower.starts_with("lpt"))
        && lower.as_bytes()[3].is_ascii_digit()
        && lower.as_bytes()[3] != b'0'
    {
        return false;
    }
    true
}

pub fn transcript_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.jsonl"))
}

fn open_path(root: &Path, id: &str) -> Option<PathBuf> {
    valid_session_id(id).then(|| root.join(format!("{id}.open")))
}

/// Core's `JsonlSessionStore::resolve_unique_prefix`, same error strings.
pub fn resolve_unique_prefix(root: &Path, raw: &str) -> Result<String, String> {
    if !valid_session_id(raw) {
        return Err(format!("invalid session id {raw:?}"));
    }
    if transcript_path(root, raw).is_file() {
        return Ok(raw.to_string());
    }
    let mut matches: Vec<String> = match std::fs::read_dir(root) {
        Ok(read) => read
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .into_string()
                    .ok()
                    .and_then(|n| n.strip_suffix(".jsonl").map(str::to_owned))
            })
            .filter(|stem| stem.starts_with(raw))
            .collect(),
        Err(_) => Vec::new(),
    };
    matches.sort();
    matches.dedup();
    match matches.len() {
        0 => Err(format!("no session {raw:?} in {}", root.display())),
        1 => Ok(matches.remove(0)),
        _ => {
            let shown = matches
                .iter()
                .take(8)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            let more = matches.len().saturating_sub(8);
            Err(format!(
                "ambiguous session id {raw:?} — matches: {shown}{}",
                if more > 0 {
                    format!(" (+{more} more)")
                } else {
                    String::new()
                }
            ))
        }
    }
}

/// Core's `open_lock_held_at`: the session is live while some process holds
/// an exclusive lock on `<root>/<id>.open`. A probe that gets the lock drops
/// it immediately when the file handle closes.
pub fn open_lock_held(root: &Path, id: &str) -> bool {
    let Some(path) = open_path(root, id) else {
        return false;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// Core's `read_open_holder`: `{"pid":N,"started_ms":M}` or a bare pid.
pub fn open_holder_pid(root: &Path, id: &str) -> Option<u32> {
    let path = open_path(root, id)?;
    #[cfg(windows)]
    let path = path.with_extension("open-holder");
    let content = std::fs::read_to_string(path).ok()?;
    let content = content.trim();
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .filter(|v| v.get("started_ms").is_some_and(serde_json::Value::is_u64))
        .and_then(|v| v.get("pid")?.as_u64())
        .and_then(|p| u32::try_from(p).ok())
        .or_else(|| content.parse::<u32>().ok())
}

/// Core's `session_read::live_sessions`: ids whose `.open` lock is held, with
/// the holder pid, sorted.
pub fn live_sessions(root: &Path) -> std::io::Result<Vec<(String, Option<u32>)>> {
    let read = match std::fs::read_dir(root) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in read {
        let Some(name) = entry?.file_name().into_string().ok() else {
            continue;
        };
        let Some(id) = name.strip_suffix(".open") else {
            continue;
        };
        if open_lock_held(root, id) {
            out.push((id.to_string(), open_holder_pid(root, id)));
        }
    }
    out.sort();
    Ok(out)
}
