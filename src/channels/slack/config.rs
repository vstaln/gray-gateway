//! Private config load/save/validate.
//! Errors never contain config values.
pub use crate::channels::common::config::{atomic_json, bool_opt, int_in};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Default config path: ~/.config/gray-slack/config.json.
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config/gray-slack/config.json")
}

/// Write JSON (pretty + trailing newline) atomically: parents 0700,
/// temp file + rename, replacement mode 0600.

/// A Slack id: a letter prefix followed by uppercase alphanumerics
/// (U…/W… users, C…/D…/G… channels, T… teams, B… bots, F… files).
pub fn slack_id(v: &Value) -> bool {
    match v.as_str() {
        Some(s) => {
            s.len() >= 2
                && s.len() <= 32
                && s.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
                && s.bytes().all(|b| b.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Slack user-id shape (U… or W… enterprise users).
pub fn user_id(v: &Value) -> bool {
    match v.as_str() {
        Some(s) => slack_id(v) && (s.starts_with('U') || s.starts_with('W')),
        None => false,
    }
}

/// Slack conversation-id shape (C… channel, D… DM, G… private/MPIM).
pub fn channel_id(v: &Value) -> bool {
    match v.as_str() {
        Some(s) => slack_id(v) && (s.starts_with('C') || s.starts_with('D') || s.starts_with('G')),
        None => false,
    }
}

/// Validate a config object. Errors never echo config values.
pub fn validate_config(data: &Value) -> Result<(), String> {
    if !data.is_object() {
        return Err("Configuration must be an object".to_string());
    }
    for key in ["bot_token", "app_token"] {
        match data.get(key).and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => {}
            _ => return Err(format!("{key} is missing; run setup")),
        }
    }
    // channel_id is the home channel and always required; owner_id is
    // optional — a token-only config means "nobody admitted yet", and every
    // human DM then gets a pairing reply (the first approval becomes owner).
    if !data.get("channel_id").is_some_and(channel_id) {
        return Err("channel_id must be a Slack channel ID".to_string());
    }
    if let Some(owner) = data.get("owner_id") {
        if !user_id(owner) {
            return Err("owner_id must be a Slack member ID".to_string());
        }
    }
    for (key, check) in [
        ("allowed_users", user_id as fn(&Value) -> bool),
        ("allowed_channels", channel_id as fn(&Value) -> bool),
    ] {
        if let Some(list) = data.get(key) {
            let ok = list.as_array().is_some_and(|a| a.iter().all(check));
            if !ok {
                return Err(format!("{key} must be an array of Slack IDs"));
            }
        }
    }
    if let Some(bot) = data.get("bot_user_id") {
        if !slack_id(bot) {
            return Err("bot_user_id must be a Slack ID".to_string());
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
    bool_opt(data, "restart_notification")?;
    bool_opt(data, "reply_in_thread")?;
    int_in(data, "timeout_seconds", 1, 86400)?;
    int_in(data, "concurrency", 1, 16)?;
    int_in(data, "max_requests", 1, 1000)?;
    int_in(data, "queue_capacity", 1, 100000)?;
    if let Some(policy) = data.get("session_reset") {
        if !policy.is_object() {
            return Err("session_reset must be an object".to_string());
        }
        if let Some(mode) = policy.get("mode").and_then(Value::as_str) {
            if !["none", "idle", "daily", "both"].contains(&mode) {
                return Err("session_reset.mode must be none, idle, daily or both".to_string());
            }
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
