//! Chat-bound cron: a job added from Discord comes back to the channel it
//! was added from.
//!
//! gray core owns the semantics (schedule, claim, fire, `[SILENT]`, the
//! `Cronjob Response:` frame) and hands the rendered line back through
//! `gray cron tick --json`. This module only carries bytes: it records
//! where a conversation lives (so a job the model adds with a plain
//! `gray cron add` inherits the binding), ticks each conversation's store,
//! and posts what comes back.
//!
//! Deliberately not a "cron lives in the gateway daemon" layout:
//! every turn here runs in its own gray home, so the jobs live beside the
//! conversation they belong to and are fired with that conversation's
//! credentials, workdir, and skills.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// How often due jobs are claimed. gray's own cron service uses 60s.
pub const TICK_EVERY: Duration = Duration::from_secs(60);

/// Where a conversation's jobs deliver. Written where the channel is known
/// (the gateway, per turn) and read where the home is known (the runner,
/// the ticker) — the binding survives restarts because it is a file.
fn route_path(home: &Path) -> PathBuf {
    home.join("route.json")
}

/// `dm` or `channel` for a conversation key (`session::conversation_key`):
/// DMs are keyed `chat:{channel}`, server channels `chat:{channel}:user:{id}`.
/// Plugins read it from route.json to keep personal data out of shared
/// channels (gray-life, memory-style context).
pub fn route_kind(conversation: &str) -> &'static str {
    if conversation.contains(":user:") {
        "channel"
    } else {
        "dm"
    }
}

pub fn write_route(home: &Path, channel: &str, chat: &str, kind: &str) {
    let route = serde_json::json!({
        "platform": "discord",
        "chat": chat,
        "route": channel,
        "kind": kind,
    });
    let _ = crate::channels::discord::config::atomic_json(&route_path(home), &route);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(route_path(home), std::fs::Permissions::from_mode(0o600));
    }
}

pub fn read_route(home: &Path) -> Option<Value> {
    std::fs::read(route_path(home))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

/// The `GRAY_CRON_ORIGIN` value for a turn: whatever binding this
/// conversation has. `None` outside a chat (a plain local gray home), which
/// keeps `gray cron add` local as before.
pub fn origin_env(home: &Path) -> Option<String> {
    let route = read_route(home)?;
    let platform = route.get("platform")?.as_str()?.trim();
    let chat = route.get("chat")?.as_str()?.trim();
    let route_id = route.get("route")?.as_str()?.trim();
    if platform.is_empty() || chat.is_empty() || route_id.is_empty() {
        return None;
    }
    Some(
        serde_json::json!({
            "platform": platform,
            "chat": chat,
            "route": route_id,
        })
        .to_string(),
    )
}

/// One rendered delivery from `gray cron tick --json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub job_id: String,
    pub channel: String,
    /// Legacy rendered text. Only used when `final_text` is absent (an older
    /// core, which still wraps the answer in its own frame).
    pub text: String,
    /// The final assistant message, or the stored reminder text (newer
    /// core). Rendered literally, never parsed.
    pub final_text: Option<String>,
    pub name: Option<String>,
    pub kind: crate::channels::discord::cron_card::Kind,
    pub status: crate::channels::discord::cron_card::Status,
    pub elapsed: Option<Duration>,
    /// Where core saved the full output. Logged, never shown.
    pub path: Option<String>,
}

/// Split tick stdout into deliveries and drop everything else. Core emits
/// one `cron_delivery` line per chat-bound fire plus a `cron_tick`
/// summary; a line we cannot parse is ignored rather than delivered —
/// narration must never post garbage into someone's channel.
pub fn parse_tick(stdout: &str) -> Vec<Delivery> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("cron_delivery") {
            continue;
        }
        if v.get("platform").and_then(Value::as_str) != Some("discord") {
            continue;
        }
        let (Some(job_id), Some(text)) = (
            v.get("job_id").and_then(Value::as_str),
            v.get("text").and_then(Value::as_str),
        ) else {
            continue;
        };
        // Prefer the explicit route (the channel); fall back to the chat id
        // for a host that routed by conversation.
        let channel = v
            .get("route")
            .and_then(Value::as_str)
            .or_else(|| v.get("chat").and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_string();
        if channel.is_empty() || text.trim().is_empty() {
            continue;
        }
        let str_field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        out.push(Delivery {
            job_id: job_id.to_string(),
            channel,
            text: text.to_string(),
            final_text: str_field("final_text"),
            name: str_field("name"),
            kind: crate::channels::discord::cron_card::Kind::from_wire(
                v.get("kind").and_then(Value::as_str),
            ),
            status: crate::channels::discord::cron_card::Status::from_wire(
                v.get("status").and_then(Value::as_str),
            ),
            // 0 means "no agent turn" (a reminder): show no timing at all.
            elapsed: v
                .get("elapsed_ms")
                .and_then(Value::as_u64)
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis),
            path: str_field("path"),
        });
    }
    out
}

/// Every conversation home that could hold jobs: the route file only
/// exists for a conversation the gateway has actually run.
pub fn routable_homes(conversations_dir: &Path) -> Vec<PathBuf> {
    let mut homes = Vec::new();
    let Ok(entries) = std::fs::read_dir(conversations_dir) else {
        return homes;
    };
    for entry in entries.flatten() {
        let home = entry.path();
        if home.join("cron").is_dir() && read_route(&home).is_some() {
            homes.push(home);
        }
    }
    homes.sort();
    homes
}

/// Claim and fire whatever is due in one home. Returns what to post.
/// A tick that fails (gray missing, provider down) is not a turn failure:
/// the next tick tries again, and the store's claim is released.
pub async fn tick_home(gray_bin: &Path, home: &Path) -> Vec<Delivery> {
    let out = tokio::process::Command::new(gray_bin)
        .arg("cron")
        .arg("tick")
        .arg("--json")
        .env("GRAY_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;
    match out {
        Ok(o) => parse_tick(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => Vec::new(),
    }
}

/// The card for one delivery. Pure (no network), so it is unit-tested.
/// Structured deliveries are rendered literally; only legacy text goes
/// through the lossy frame stripper. Deliberately does NOT call
/// `text::sanitize`: it rewrites URLs into `<…>` and `:done:`-style emoji
/// names, which would break "the reminder text is delivered byte for byte".
pub fn card_for(d: &Delivery) -> crate::channels::discord::cron_card::CronCard {
    use crate::channels::discord::cron_card::CronCard;
    let mut card = match &d.final_text {
        Some(text) => CronCard::from_final_text(d.name.clone(), text, d.kind, d.status),
        None => CronCard::from_raw(&d.text, d.kind, d.status),
    };
    card.elapsed = d.elapsed;
    card
}

/// Post one cron delivery as a Components V2 card. Returns true when the
/// message is delivered (or there was nothing to deliver).
///
/// * 429: wait `retry_after` (bounded to 30s) and retry once.
/// * 400 (Discord rejected the V2 tree) or a local validation refusal: send
///   plain text instead, which the transport splits to fit.
/// * anything else (401/403/404/5xx/network): logged with its status, no
///   retry loop; the caller decides what a failed delivery means.
///
/// The full-output path is logged here and never appears in Discord.
pub async fn post_delivery(
    rest: &crate::channels::discord::transport::Rest,
    store: &crate::channels::discord::durable::Store,
    channel: u64,
    d: &Delivery,
) -> bool {
    use crate::channels::discord::transport::TransportError;
    let card = card_for(d);
    if let Some(path) = card.parsed.full_output.as_deref().or(d.path.as_deref()) {
        crate::channels::discord::eprintln_stamped!("[discord] cron full output: {path}");
    }
    let long = crate::channels::discord::text::utf16_len(&card.parsed.body)
        > crate::channels::discord::cron_card::MAX_BODY_UNITS;
    let components = if long {
        let pages = crate::channels::discord::pager::paginate(
            &card.parsed.body,
            crate::channels::discord::pager::PAGE_LIMIT,
        );
        let (title, accent, _) = card.head();
        match crate::channels::discord::pager::mint(
            store,
            "*",
            &channel.to_string(),
            &pages,
            &title,
            accent,
        ) {
            Ok(token) => card.paged_components(&token, &pages, 0),
            Err(e) => {
                crate::channels::discord::eprintln_stamped!(
                    "[discord] cron pager state failed: {e}"
                );
                card.components().unwrap_or_default()
            }
        }
    } else {
        card.components().unwrap_or_default()
    };
    if components.is_empty() {
        crate::channels::discord::eprintln_stamped!(
            "[discord] cron delivery for channel {channel} has an empty body; not posting"
        );
        return true;
    }
    let mut retried = false;
    loop {
        match rest.send_v2(channel, &components, None).await {
            Ok(_) => return true,
            Err(TransportError::RateLimited(after)) if !retried => {
                retried = true;
                let secs = after
                    .filter(|s| s.is_finite())
                    .unwrap_or(1.0)
                    .clamp(0.0, 30.0);
                crate::channels::discord::eprintln_stamped!(
                    "[discord] cron delivery rate limited; retrying in {secs:.1}s"
                );
                tokio::time::sleep(Duration::from_secs_f64(secs)).await;
            }
            Err(TransportError::Http(400, body)) => {
                crate::channels::discord::eprintln_stamped!(
                    "[discord] cron card rejected (HTTP 400): {body}; sending plain text"
                );
                return send_plain(rest, channel, &card).await;
            }
            Err(TransportError::Invalid(msg)) => {
                crate::channels::discord::eprintln_stamped!(
                    "[discord] cron card refused locally: {msg}; sending plain text"
                );
                return send_plain(rest, channel, &card).await;
            }
            Err(e) => {
                crate::channels::discord::eprintln_stamped!(
                    "[discord] cron delivery to channel {channel} failed: {e:?}"
                );
                return false;
            }
        }
    }
}

async fn send_plain(
    rest: &crate::channels::discord::transport::Rest,
    channel: u64,
    card: &crate::channels::discord::cron_card::CronCard,
) -> bool {
    let pages = crate::channels::discord::pager::paginate(&card.parsed.body, 1900);
    for page in &pages {
        if let Err(e) = rest.send(channel, page, None).await {
            crate::channels::discord::eprintln_stamped!(
                "[discord] cron plain fallback to channel {channel} failed: {e:?}"
            );
            return false;
        }
    }
    true
}

#[cfg(test)]
mod card_tests {
    use super::*;
    use crate::channels::discord::cron_card::{Kind, Status};

    fn one(line: serde_json::Value) -> Vec<Delivery> {
        parse_tick(&line.to_string())
    }

    #[test]
    fn a_structured_delivery_keeps_its_kind_status_and_literal_text() {
        let ds = one(serde_json::json!({
            "type": "cron_delivery", "job_id": "8d90cd866db9", "name": "clean-my-roo",
            "platform": "discord", "chat": "c", "route": "42",
            "kind": "reminder", "status": "ok", "elapsed_ms": 300,
            "final_text": ":done:  clean my roo\n\n\nhttps://x.y/z",
            "text": "clean-my-roo\n\nlegacy", "path": "/tmp/secret/out.md"
        }));
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].kind, Kind::Reminder);
        assert_eq!(ds[0].status, Status::Ok);
        assert_eq!(ds[0].elapsed, Some(Duration::from_millis(300)));
        let shown = serde_json::to_string(&card_for(&ds[0]).components().unwrap()).unwrap();
        // literal: no emoji-name rewrite, no <url> wrapping, no whitespace
        // collapse. The serialized JSON escapes the newlines, so match the
        // raw spelling.
        assert!(
            shown.contains(r#":done:  clean my roo\n\n\nhttps://x.y/z"#),
            "{shown}"
        );
        assert!(!shown.contains("8d90cd866db9"), "job id leaked");
        assert!(!shown.contains("/tmp/secret"), "output path leaked");
    }

    #[test]
    fn a_reminder_shows_no_timing() {
        let ds = one(serde_json::json!({
            "type": "cron_delivery", "job_id": "j", "platform": "discord", "route": "42",
            "kind": "reminder", "elapsed_ms": 0, "final_text": "clean my roo", "text": "x"
        }));
        assert_eq!(ds[0].elapsed, None);
        let shown = serde_json::to_string(&card_for(&ds[0]).components().unwrap()).unwrap();
        assert!(
            !shown.contains("done ·"),
            "a reminder has no agent turn to time"
        );
    }

    #[test]
    fn a_failed_status_is_red() {
        let ds = one(serde_json::json!({
            "type": "cron_delivery", "job_id": "j", "platform": "discord", "route": "42",
            "status": "failed", "kind": "task", "name": "nightly",
            "final_text": "agent run failed: boom", "text": "x"
        }));
        let c = card_for(&ds[0]).components().unwrap();
        assert_eq!(
            c[0]["accent_color"],
            crate::channels::discord::render::DANGER_ACCENT
        );
        assert!(
            c[0]["components"][0]["content"]
                .as_str()
                .unwrap()
                .contains("nightly failed")
        );
    }

    #[test]
    fn legacy_core_output_is_stripped_to_the_answer() {
        let legacy = "Cronjob Response: clean-room\n(job_id: 8d90cd866db9)\n-------------\n\n[tool:bash]\n[result:exit 0 · 0.3s · 45 lines]\n<untrusted-output>\ndrwxr-xr-x 2 u u 4096 supervise\n</untrusted-output>\n\nclean my roo\n\nTo stop or manage this job, send me a new message (e.g. \"stop reminder clean-room\").\nFull output: /home/u/.config/gray-discord/conversations/h/cron/output/8d90cd866db9.md";
        let ds = one(serde_json::json!({
            "type": "cron_delivery", "job_id": "8d90cd866db9", "platform": "discord",
            "route": "42", "text": legacy
        }));
        let shown = serde_json::to_string(&card_for(&ds[0]).components().unwrap()).unwrap();
        assert!(shown.contains("clean my roo"), "{shown}");
        for bad in [
            "8d90cd866db9",
            "job_id",
            "Cronjob Response",
            "[tool:",
            "untrusted",
            "Full output",
            "supervise",
        ] {
            assert!(!shown.contains(bad), "leaked {bad}");
        }
    }

    #[test]
    fn a_line_without_a_route_is_never_delivered() {
        assert!(
            one(serde_json::json!({
                "type": "cron_delivery", "job_id": "j", "platform": "discord",
                "final_text": "hi", "text": "hi"
            }))
            .is_empty()
        );
    }
}
