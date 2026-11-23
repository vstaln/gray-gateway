//! Private config load/save/validate.
//! Errors never contain config values.
pub use crate::channels::common::config::{atomic_json, int_in, user_home};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Default config path: ~/.config/gray-irc/config.json
/// The user's home directory — `$HOME` on unix, `USERPROFILE` on Windows,
/// the same places gray itself resolves.

/// Default config path.
pub fn default_path() -> PathBuf {
    user_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/gray-irc/config.json")
}

/// Write JSON (pretty + trailing newline) atomically: parents 0700,
/// temp file + rename, replacement mode 0600.

/// One IRC target: `#`/`&`/`+`/`!` channel or a bare nick, no spaces/ctrl.
pub fn target_valid(v: &Value) -> bool {
    match v.as_str() {
        Some(s) => {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_graphic() && b != b',' && b != b'\x07')
        }
        None => false,
    }
}

fn str_nonempty(data: &Value, key: &str) -> Result<(), String> {
    match data.get(key).and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(()),
        _ => Err(format!("{key} is missing; run setup")),
    }
}

/// Validate a config object. Error texts never echo config values.
pub fn validate_config(data: &Value) -> Result<(), String> {
    if !data.is_object() {
        return Err("Configuration must be an object".to_string());
    }
    str_nonempty(data, "server")?;
    str_nonempty(data, "nickname")?;
    if data
        .get("nickname")
        .and_then(Value::as_str)
        .is_some_and(|n| n.len() > 30 || !n.bytes().all(|b| b.is_ascii_graphic()))
    {
        return Err("nickname must be 1-30 non-space characters".to_string());
    }
    match data.get("channels").and_then(Value::as_array) {
        Some(a) if !a.is_empty() && a.iter().all(target_valid) => {}
        _ => return Err("channels must be a non-empty array of IRC targets".to_string()),
    }
    // Inbound is allowlisted-only: the array may be empty (nobody admitted
    // yet, every message is ignored) but must be present.
    match data.get("allowed_users").and_then(Value::as_array) {
        Some(a)
            if a.iter()
                .all(|v| v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 64)) => {}
        _ => return Err("allowed_users must be an array of IRC nicks".to_string()),
    }
    int_in(data, "port", 1, 65535)?;
    int_in(data, "timeout_seconds", 1, 86400)?;
    int_in(data, "max_requests", 1, 1000)?;
    int_in(data, "queue_capacity", 1, 100000)?;
    if data
        .get("busy_input_mode")
        .is_some_and(|v| !matches!(v.as_str(), Some("queue" | "steer")))
    {
        return Err("busy_input_mode must be queue or steer".to_string());
    }
    int_in(data, "concurrency", 1, 16)?;
    for key in ["use_tls", "restart_notification"] {
        if let Some(t) = data.get(key) {
            if !t.is_boolean() {
                return Err(format!("{key} must be true or false"));
            }
        }
    }
    for key in ["server_password", "nickserv_password"] {
        if let Some(v) = data.get(key) {
            if !v.is_string() {
                return Err(format!("{key} must be a string"));
            }
        }
    }
    if let Some(h) = data.get("home_channel") {
        if !target_valid(h) {
            return Err("home_channel must be an IRC channel or nick".to_string());
        }
    }
    for key in ["gray_bin", "gray_home", "workdir"] {
        let ok = data
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| Path::new(s).is_absolute());
        if !ok {
            return Err(format!("{key} must be an absolute path"));
        }
    }
    Ok(())
}

/// Load + validate. Any IO/parse failure maps to the setup hint.
pub fn load_config(path: &Path) -> Result<Value, String> {
    let data: Value = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "Configuration missing or invalid; run setup".to_string())?;
    validate_config(&data)?;
    Ok(data)
}

/// Validate + atomic save.
pub fn save_config(path: &Path, data: &Value) -> Result<(), String> {
    validate_config(data)?;
    atomic_json(path, data)
}
