use std::path::Path;

use super::event::Kind;
use super::outbox::Intent;
use super::settings::Settings;
use crate::cron::store::Origin as Route;

pub const SILENT_TOKENS: [&str; 3] = ["NO_REPLY", "[SILENT]", "HEARTBEAT_OK"];

pub fn is_silent(text: &str, kind: Kind) -> bool {
    if text.trim().is_empty() {
        return true;
    }
    if !kind.autonomous() {
        return SILENT_TOKENS.contains(&text.trim());
    }
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    lines.next().is_some_and(is_token_line) || lines.next_back().is_some_and(is_token_line)
}

fn is_token_line(line: &str) -> bool {
    let bare = line.trim_matches(|c| c == '*' || c == '`' || c == ' ' || c == '\t');
    SILENT_TOKENS.contains(&bare)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Deliver(Intent),

    Suppressed(&'static str),
}

pub fn decide(
    dir: &Path,
    settings: &Settings,
    key: &str,
    kind: Kind,
    route: Option<Route>,
    last_route: Option<Route>,
    text: &str,
) -> Decision {
    if is_silent(text, kind) {
        suppressed(dir, key, kind, "silent", text);
        return Decision::Suppressed("silent");
    }
    let route = route
        .or(last_route)
        .or_else(|| settings.owners.first().cloned());
    let intent = Intent::new(key, kind, route, text);
    if let Err(e) = super::outbox::enqueue(dir, &intent) {
        log::warn!("gateway: cannot enqueue delivery for {key}: {e:#}");
        super::activity::log(
            dir,
            "deliver_failed",
            serde_json::json!({"key": key, "kind": kind.as_str(), "error": format!("{e:#}")}),
        );
        return Decision::Suppressed("enqueue failed");
    }
    super::activity::log(
        dir,
        "queued",
        serde_json::json!({"key": key, "kind": kind.as_str(), "platform": &intent.platform,
            "text": text.chars().take(300).collect::<String>()}),
    );
    Decision::Deliver(intent)
}

fn suppressed(dir: &Path, key: &str, kind: Kind, reason: &'static str, text: &str) {
    super::activity::log(
        dir,
        "suppressed",
        serde_json::json!({"key": key, "kind": kind.as_str(), "reason": reason,
            "text": text.chars().take(200).collect::<String>()}),
    );
}
