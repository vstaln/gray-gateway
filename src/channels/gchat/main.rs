//! gchat — Google Chat space webhooks (outbound only: a space webhook is
//! a POST URL; inbound needs a Workspace Chat app — documented in README).
//! Config: ~/.gray/gchat/config.json {"webhook"} or GCHAT_WEBHOOK.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

const NAME: &str = "gchat";

pub fn manifest() -> Value {
    json!({
        "name": NAME, "display_name": "Google Chat",
        "version": env!("CARGO_PKG_VERSION"), "protocol": "1.1",
        "tools": [
            {"name": "gchat_send", "label": "Google Chat", "preview": "{text|firstline}",
             "description": "Send text (Google Chat markdown: *bold* _italic_ `code`) to the configured space webhook.",
             "parameters": {"type": "object", "properties": {
                "text": {"type": "string"},
                "thread": {"type": "string", "description": "thread key to reply in"}},
             "required": ["text"]}},
        ],
        "commands": ["/gchat"], "subcommands": ["gchat"],
        "command_descriptions": {"/gchat": "Google Chat bridge"},
    })
}

/// The user's home dir: `$HOME`, or `%USERPROFILE%` on Windows where
/// HOME is typically unset for a natively-spawned process.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

fn config_path() -> std::path::PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".gray")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join("gchat").join("config.json")
}
fn webhook() -> Option<String> {
    std::env::var("GCHAT_WEBHOOK")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string(config_path())
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|c| c["webhook"].as_str().map(str::to_string))
        })
}

fn send(a: &Value) -> Result<String, String> {
    let mut url =
        webhook().ok_or("no webhook — `gchat setup --webhook https://chat.googleapis.com/…`")?;
    let mut body = json!({"text": a["text"].as_str().ok_or("text required")?});
    if let Some(t) = a["thread"].as_str() {
        body["thread"] = json!({"threadKey": t});
        url = format!("{url}&messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
    }
    match ureq::post(&url)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send_json(body)
    {
        Ok(_) => Ok("sent".into()),
        Err(ureq::Error::Status(c, r)) => {
            Err(format!("HTTP {c}: {}", r.into_string().unwrap_or_default()))
        }
        Err(e) => Err(e.to_string()),
    }
}

pub fn cli(argv: &[String]) -> Result<String, String> {
    match argv.first().map(String::as_str) {
        Some("setup") => {
            let mut it = argv[1..].iter();
            if let Some(f) = it.next() {
                if f == "--webhook" {
                    let w = it.next().ok_or("--webhook needs URL")?;
                    std::fs::create_dir_all(config_path().parent().unwrap())
                        .map_err(|e| e.to_string())?;
                    std::fs::write(
                        config_path(),
                        serde_json::to_string_pretty(&json!({"webhook": w})).unwrap(),
                    )
                    .map_err(|e| e.to_string())?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            config_path(),
                            std::fs::Permissions::from_mode(0o600),
                        );
                    }
                    return Ok("saved".into());
                }
                return Err(format!("unknown flag {f}"));
            }
            Err("setup needs --webhook".into())
        }
        Some("status") | None => Ok(format!(
            "Google Chat\n  webhook: {}",
            if webhook().is_some() {
                "set"
            } else {
                "MISSING"
            }
        )),
        Some("-h" | "--help" | "help") => Ok("usage: gchat setup --webhook URL | status".into()),
        Some(o) => Err(format!("unknown verb {o}")),
    }
}

/// Answer one protocol method the gateway routes to this channel.
pub fn dispatch(method: &str, params: &Value) -> Value {
    if method == "plugin/manifest" {
        return manifest();
    }
    if method == "tool/call" {
        let name = params["name"].as_str().unwrap_or("");
        return match name {
            "gchat_send" => match send(&params["args"]) {
                Ok(c) => json!({"content": c}),
                Err(e) => json!({"content": e, "is_error": true}),
            },
            other => json!({"content": format!("unknown tool {other}"), "is_error": true}),
        };
    }
    if method == "command/run" {
        let argv: Vec<String> = params["argv"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let text = cli(&argv).unwrap_or_else(|e| e);
        return json!({"text": text});
    }
    json!({"error": {"code": -32601, "message": format!("method not found: {method}")}})
}
