#![allow(dead_code)]

pub mod common;

pub mod discord;
pub mod email;
pub mod gchat;
pub mod irc;
pub mod mastodon;
pub mod matrix;
pub mod mattermost;
pub mod ntfy;
pub mod signal;
pub mod slack;
pub mod teams;
pub mod telegram;
pub mod twilio;
pub mod webchat;
pub mod whatsapp;

use serde_json::Value;

use crate::parts::{Part, Parts};

pub const PARTS: &[Part] = &[
    Part {
        name: "discord",
        default: false,
        description: "Discord bridge: send, rich components, voice-note transcripts",
    },
    Part {
        name: "telegram",
        default: false,
        description: "Telegram bridge: send, files, voice-note transcripts",
    },
    Part {
        name: "slack",
        default: false,
        description: "Slack bridge over Socket Mode",
    },
    Part {
        name: "signal",
        default: false,
        description: "Signal bridge through signal-cli",
    },
    Part {
        name: "matrix",
        default: false,
        description: "Matrix bridge over /sync",
    },
    Part {
        name: "irc",
        default: false,
        description: "IRC bridge with TLS",
    },
    Part {
        name: "email",
        default: false,
        description: "Email bridge over IMAP and SMTP",
    },
    Part {
        name: "ntfy",
        default: false,
        description: "ntfy subscribe/publish bridge",
    },
    Part {
        name: "webchat",
        default: false,
        description: "Local browser chat on 127.0.0.1",
    },
    Part {
        name: "whatsapp",
        default: false,
        description: "WhatsApp Business Cloud API",
    },
    Part {
        name: "teams",
        default: false,
        description: "Microsoft Teams webhook channel",
    },
    Part {
        name: "twilio",
        default: false,
        description: "Twilio SMS",
    },
    Part {
        name: "gchat",
        default: false,
        description: "Google Chat webhook channel",
    },
    Part {
        name: "mastodon",
        default: false,
        description: "Mastodon post, timeline and notifications",
    },
    Part {
        name: "mattermost",
        default: false,
        description: "Mattermost REST channel",
    },
    Part {
        name: "send-message",
        default: false,
        description: "queue /tell messages for the next turn",
    },
    Part {
        name: "webhooks",
        default: false,
        description: "local HTTP webhooks that start turns",
    },
    Part {
        name: "file-trigger",
        default: false,
        description: "per-path reminders appended to write results",
    },
    Part {
        name: "control",
        default: false,
        description: "unix-socket remote control and send_to_session",
    },
];

fn channel_manifest(name: &str) -> Value {
    match name {
        "discord" => discord::sidecar::MANIFEST.clone(),
        "telegram" => telegram::sidecar::MANIFEST.clone(),
        "slack" => slack::sidecar::MANIFEST.clone(),
        "signal" => signal::sidecar::MANIFEST.clone(),
        "matrix" => matrix::sidecar::MANIFEST.clone(),
        "irc" => irc::sidecar::MANIFEST.clone(),
        "email" => email::sidecar::MANIFEST.clone(),
        "ntfy" => ntfy::sidecar::MANIFEST.clone(),
        "webchat" => webchat::main::manifest(),
        "whatsapp" => whatsapp::main::manifest(),
        "teams" => teams::main::manifest(),
        "twilio" => twilio::main::manifest(),
        "gchat" => gchat::main::manifest(),
        "mastodon" => mastodon::main::manifest(),
        "mattermost" => mattermost::main::manifest(),
        _ if crate::inbound::is_part(name) => crate::inbound::part_manifest(name),
        _ => Value::Null,
    }
}

pub fn config_path(name: &str) -> std::path::PathBuf {
    let path = match name {
        "discord" => discord::config::default_path(),
        "telegram" => telegram::config::default_path(),
        "slack" => slack::config::default_path(),
        "signal" => signal::config::default_path(),
        "matrix" => matrix::config::default_path(),
        "irc" => irc::config::default_path(),
        "email" => email::config::default_path(),
        "ntfy" => ntfy::config::default_path(),
        other => {
            let base = common::config::user_home()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".gray")
                .join(other);
            return base.join("config.json");
        }
    };
    std::path::absolute(&path).unwrap_or(path)
}

fn list(v: &Value, key: &str) -> Vec<Value> {
    v.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    list(v, key)
        .iter()
        .filter_map(|e| e.as_str().map(str::to_string))
        .collect()
}

pub fn manifest(parts: &Parts) -> Value {
    use serde_json::json;
    let mut tools = vec![crate::cron_tool::tool_def()];
    let mut commands = vec![json!("/cron"), json!("/gateway")];
    let mut command_descriptions = json!({});
    let mut turn_local = json!({"/gateway": ["*"], "/cron": ["*"]});
    let mut hooks: Vec<String> = Vec::new();
    let mut capabilities: Vec<String> = Vec::new();
    let mut requires: Vec<String> = vec!["memory".to_string()];
    for part in PARTS {
        if !parts.on(part.name) {
            continue;
        }
        let m = channel_manifest(part.name);
        tools.extend(list(&m, "tools"));
        for c in list(&m, "commands") {
            if !commands.contains(&c) {
                commands.push(c);
            }
        }
        if let Some(d) = m.get("command_descriptions").and_then(Value::as_object) {
            for (k, v) in d {
                command_descriptions[k] = v.clone();
            }
        }
        if let Some(t) = m.get("turn_local").and_then(Value::as_object) {
            for (k, v) in t {
                turn_local[k] = v.clone();
            }
        }
        for h in strings(&m, "hooks") {
            if !hooks.contains(&h) {
                hooks.push(h);
            }
        }
        for c in strings(&m, "capabilities") {
            if !capabilities.contains(&c) {
                capabilities.push(c);
            }
        }
        for r in strings(&m, "requires_plugins") {
            if r != "gateway" && !requires.contains(&r) {
                requires.push(r);
            }
        }
    }
    let mut out = json!({
        "name": "gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.3",
        "description": "always-on gateway: chat channels, cron scheduling, control socket",
        "tools": tools,
        "commands": commands,
        "command_descriptions": command_descriptions,
        "turn_local": turn_local,
        "autostart": true,
        "requires_plugins": requires,
        "parts": parts.manifest(PARTS),
        "completion": [
            "setup", "run", "status", "start", "stop", "restart", "install",
            "uninstall", "on", "off", "lifecycle", "send", "wake", "activity",
            "pause", "resume", "inbox", "cron", "session", "reset-policy",
        ],
    });
    if let Some(obj) = command_descriptions.as_object_mut() {
        for (name, description) in crate::inbound::command_descriptions(parts) {
            obj.insert(name.to_string(), json!(description));
        }
    }
    if !hooks.is_empty() {
        out["hooks"] = json!(hooks);
    }
    if !capabilities.is_empty() {
        out["capabilities"] = json!(capabilities);
    }
    out
}

pub fn tool_owner(parts: &Parts, tool: &str) -> Option<&'static str> {
    PARTS
        .iter()
        .filter(|p| parts.on(p.name))
        .find(|p| {
            channel_manifest(p.name)
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| {
                    tools
                        .iter()
                        .any(|t| t.get("name").and_then(Value::as_str) == Some(tool))
                })
        })
        .map(|p| p.name)
}

pub fn command_owner(parts: &Parts, command: &str) -> Option<&'static str> {
    let command = command.trim_start_matches('/');
    let with_slash = format!("/{command}");
    PARTS
        .iter()
        .filter(|p| parts.on(p.name))
        .find(|p| {
            let m = channel_manifest(p.name);
            strings(&m, "commands").contains(&with_slash)
                || strings(&m, "subcommands").iter().any(|c| c == command)
        })
        .map(|p| p.name)
}

pub fn is_part(name: &str) -> bool {
    PARTS.iter().any(|p| p.name == name) && !crate::inbound::is_part(name)
}

pub async fn call_tool(parts: &Parts, params: &Value) -> Option<Value> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let owner = tool_owner(parts, name)?;
    let path = config_path(owner);
    Some(match owner {
        "discord" => discord::sidecar::dispatch("tool/call", params, &path).await,
        "telegram" => telegram::sidecar::dispatch("tool/call", params, &path).await,
        "slack" => slack::sidecar::dispatch("tool/call", params, &path).await,
        "signal" => signal::sidecar::dispatch("tool/call", params, &path).await,
        "matrix" => matrix::sidecar::dispatch("tool/call", params, &path).await,
        "irc" => irc::sidecar::dispatch("tool/call", params, &path).await,
        "email" => email::sidecar::dispatch("tool/call", params, &path).await,
        "ntfy" => ntfy::sidecar::dispatch("tool/call", params, &path).await,
        other => small_dispatch(other, "tool/call", params),
    })
}

pub async fn run_command(parts: &Parts, params: &Value) -> Option<Value> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let owner = command_owner(parts, name)?;
    let path = config_path(owner);
    Some(match owner {
        "discord" => discord::sidecar::dispatch("command/run", params, &path).await,
        "telegram" => telegram::sidecar::dispatch("command/run", params, &path).await,
        "slack" => slack::sidecar::dispatch("command/run", params, &path).await,
        "signal" => signal::sidecar::dispatch("command/run", params, &path).await,
        "matrix" => matrix::sidecar::dispatch("command/run", params, &path).await,
        "irc" => irc::sidecar::dispatch("command/run", params, &path).await,
        "email" => email::sidecar::dispatch("command/run", params, &path).await,
        "ntfy" => ntfy::sidecar::dispatch("command/run", params, &path).await,
        other => small_dispatch(other, "command/run", params),
    })
}

fn small_dispatch(name: &str, method: &str, params: &Value) -> Value {
    match name {
        "whatsapp" => whatsapp::main::dispatch(method, params),
        "teams" => teams::main::dispatch(method, params),
        "twilio" => twilio::main::dispatch(method, params),
        "gchat" => gchat::main::dispatch(method, params),
        "mastodon" => mastodon::main::dispatch(method, params),
        "mattermost" => mattermost::main::dispatch(method, params),
        "webchat" => webchat::main::dispatch(method, params),
        _ => json_error("unknown channel"),
    }
}

pub fn prompt_context(parts: &Parts) -> Option<String> {
    let blocks: Vec<String> = PARTS
        .iter()
        .filter(|p| parts.on(p.name))
        .filter_map(|p| {
            channel_manifest(p.name)
                .get("prompt_context")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

fn json_error(message: &str) -> Value {
    serde_json::json!({"content": message, "is_error": true})
}

pub fn adopt_services(parts: &Parts) {
    for part in PARTS
        .iter()
        .filter(|p| parts.on(p.name) && !crate::inbound::is_part(p.name))
    {
        for path in common::service_migrate::adopt(part.name) {
            eprintln!(
                "{}: repointed at {} {}",
                path.display(),
                env!("CARGO_PKG_NAME"),
                part.name
            );
        }
    }
}

pub fn run_cli(name: &str, argv: &[String], enabled: bool) -> anyhow::Result<()> {
    if matches!(
        argv.first().map(String::as_str),
        Some("run" | "setup" | "install" | "restart" | "start" | "status")
    ) {
        for path in common::service_migrate::adopt(name) {
            eprintln!("{}: now runs {}", path.display(), env!("CARGO_PKG_NAME"));
        }
    }
    let serving = matches!(
        argv.first().map(String::as_str),
        Some("run" | "sidecar" | "install" | "start" | "restart")
    );
    if !enabled && serving {
        anyhow::bail!(
            "the {name} channel is off, so its service will not start — \
             `gray plugin enable gateway/{name}` turns it on"
        );
    }
    let mut full = vec![format!("gray-gateway {name}")];
    full.extend(argv.iter().cloned());
    let code = match name {
        "discord" => discord::main::run(&full),
        "telegram" => telegram::main::run(&full),
        "slack" => slack::main::run(&full),
        "signal" => signal::main::run(&full),
        "matrix" => matrix::main::run(&full),
        "irc" => irc::main::run(&full),
        "email" => email::main::run(&full),
        "ntfy" => ntfy::main::run(&full),
        other => small_cli(other, argv)?,
    };
    std::process::exit(code);
}

fn small_cli(name: &str, argv: &[String]) -> anyhow::Result<i32> {
    let out = match name {
        "webchat" => return Ok(webchat::main::run(argv)),
        "teams" => teams::main::cli(argv),
        "gchat" => gchat::main::cli(argv),
        "twilio" => twilio::main::cli(argv),
        "whatsapp" => whatsapp::main::cli(argv),
        "mastodon" => mastodon::main::cli(argv),
        "mattermost" => mattermost::main::cli(argv),
        _ => anyhow::bail!("unknown channel {name}"),
    };
    match out {
        Ok(text) => {
            println!("{text}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("{e}");
            Ok(1)
        }
    }
}
