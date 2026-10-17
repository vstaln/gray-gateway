use serde_json::{Value, json};
use std::path::Path;

pub const STATE_FILE: &str = "lifecycle.json";
pub const RESTART_MARKER: &str = "restart_pending";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Previous {
    FirstBoot,

    Clean { restart: bool },

    Crashed,
}

impl Previous {
    pub fn as_str(self) -> &'static str {
        match self {
            Previous::FirstBoot => "first_boot",
            Previous::Clean { restart: true } => "restart",
            Previous::Clean { restart: false } => "clean",
            Previous::Crashed => "crashed",
        }
    }
}

pub fn boot(dir: &Path) -> Previous {
    let restart_marker = dir.join(RESTART_MARKER);
    let restart_requested = restart_marker.exists();
    let _ = std::fs::remove_file(&restart_marker);
    let previous = std::fs::read(dir.join(STATE_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let previous = match previous {
        None => Previous::FirstBoot,
        Some(state) => match state.get("state").and_then(Value::as_str) {
            Some("stopped") => Previous::Clean {
                restart: restart_requested
                    || state.get("restart").and_then(Value::as_bool) == Some(true),
            },
            _ => Previous::Crashed,
        },
    };
    write(
        dir,
        json!({"state": "running", "at": crate::cron::now_secs()}),
    );
    previous
}

pub fn restart_requested(dir: &Path) -> bool {
    dir.join(RESTART_MARKER).exists()
}

pub fn request_restart(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(RESTART_MARKER), b"")?;
    Ok(())
}

pub fn clear_restart(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(RESTART_MARKER));
}

pub fn mark_stopped(dir: &Path) -> bool {
    let restart = restart_requested(dir);
    write(
        dir,
        json!({"state": "stopped", "restart": restart, "at": crate::cron::now_secs()}),
    );
    restart
}

fn write(dir: &Path, state: Value) {
    let _ = std::fs::create_dir_all(dir);
    let _ = crate::cron::store::atomic_write_json(&dir.join(STATE_FILE), &state);
}

pub fn startup_notice(previous: Previous, interrupted: usize) -> Option<String> {
    let mut text = match previous {
        Previous::FirstBoot => return None,
        Previous::Clean { restart: true } => "Gateway restarted. gray is back and ready.",
        Previous::Clean { restart: false } => "Gateway online. gray is back and ready.",
        Previous::Crashed => {
            "Gateway is back after an unexpected stop (crash, kill, or reboot). gray is ready."
        }
    }
    .to_string();
    match interrupted {
        0 => {}
        1 => text.push_str("\n1 turn was cut short; send a message in its chat to continue."),
        n => text.push_str(&format!(
            "\n{n} turns were cut short; send a message in their chats to continue."
        )),
    }
    Some(text)
}

pub fn active_notice(restart: bool) -> &'static str {
    if restart {
        "Gateway restarting: your current task will be interrupted. Send any message after the restart to pick up where you left off."
    } else {
        "Gateway shutting down: your current task will be interrupted. Once it is back online, send any message to pick up where you left off."
    }
}

pub fn home_notice(restart: bool) -> &'static str {
    if restart {
        "Gateway restarting. Back in a moment."
    } else {
        "Gateway shutting down."
    }
}

pub fn boot_json(dir: &Path, interrupted: usize) -> Value {
    let previous = boot(dir);
    json!({
        "previous": previous.as_str(),
        "notice": startup_notice(previous, interrupted),
    })
}

pub fn stop_json(dir: &Path) -> Value {
    let restart = mark_stopped(dir);
    json!({
        "restart": restart,
        "active_notice": active_notice(restart),
        "home_notice": home_notice(restart),
    })
}
