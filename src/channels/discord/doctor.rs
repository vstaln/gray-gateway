//! Doctor preflight checks with safe errors.
//! Every failure is a category string; transport/config bodies never surface.
use serde_json::Value;

use crate::channels::discord::transport::{HOME_PERMS, Rest, THREAD_PERMS};

/// Production entry point: checks `config` against the live Discord API.
pub async fn doctor(config: &Value) -> Result<(), String> {
    let token = config.get("token").and_then(Value::as_str).unwrap_or("");
    let rest = Rest::production(token);
    check(config, &rest).await
}

/// All checks, with the REST client injected so tests can use the loopback
/// stub. Mirrors the Python order: gray binary → provider model → login →
/// message-content intent → home channel (+ guild permissions).
pub async fn check(config: &Value, rest: &Rest) -> Result<(), String> {
    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if !is_executable(gray_bin) {
        return Err("gray executable is missing or not executable".to_string());
    }
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let provider: Value = std::fs::read(std::path::Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "gray provider configuration is missing or invalid".to_string())?;
    if provider
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("gray has no configured model".to_string());
    }
    let bot_id = rest.login().await.map_err(|e| e.to_string())?;
    let (_app_id, intent) = rest.application().await.map_err(|e| e.to_string())?;
    if !intent {
        return Err("Enable Message Content Intent in the Discord developer portal".to_string());
    }
    let channel_id = config
        .get("channel_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let id: u64 = channel_id
        .parse()
        .map_err(|_| "channel_id must be a Discord ID".to_string())?;
    let channel = rest.fetch_channel(id).await.map_err(|e| e.to_string())?;
    // Resolve the effective channel permissions, including ordered parent and
    // channel overwrites; Discord's thread rule needs its own send bit.
    if channel.guild_id.is_some() {
        let perms = rest
            .effective_channel_permissions(&channel, &bot_id)
            .await
            .map_err(|e| e.to_string())?;
        let required = if channel.is_thread() {
            THREAD_PERMS
        } else {
            HOME_PERMS
        };
        if perms & required != required {
            return Err(if channel.is_thread() {
                "Thread requires View, Read History and Send Messages in Threads permissions"
                    .to_string()
            } else {
                "Home channel requires View, Send and Read History permissions".to_string()
            });
        }
    }
    println!("Bot token, intent, channel access and local gray configuration verified.");
    println!("Provider generation and live gateway connection were not tested.");
    Ok(())
}

/// `os.access(path, X_OK)` equivalent: a real file with any exec bit set.
fn is_executable(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
