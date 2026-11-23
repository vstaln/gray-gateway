//! Always-on delivery pump: posts what gray's gateway produced on its own
//! (trigger results, recovered turns) to Discord.
//!
//! Gray's gateway keeps an outbox of delivery intents; this adapter `pull`s
//! the ones for `discord` over `$GRAY_HOME/gateway.sock` (which leases them),
//! posts each to its route's channel, and `ack`s what landed. Anything not
//! acked is offered again after the lease, so a failed post or a restart
//! here loses nothing. No gateway running = nothing to pull, quietly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// How often the outbox is polled.
pub const PUMP_EVERY: Duration = Duration::from_secs(3);

/// One request/response on the gateway's control socket (one JSON line each
/// way). `None` when the gateway is not answering — which is always on
/// Windows, where gray runs no native gateway to ask.
#[cfg(unix)]
pub async fn request(sock: &Path, req: &Value) -> Option<Value> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let io = async {
        let mut stream = tokio::net::UnixStream::connect(sock).await.ok()?;
        let mut line = serde_json::to_vec(req).ok()?;
        line.push(b'\n');
        stream.write_all(&line).await.ok()?;
        let mut answer = String::new();
        tokio::io::BufReader::new(stream)
            .read_line(&mut answer)
            .await
            .ok()?;
        let v: Value = serde_json::from_str(&answer).ok()?;
        (v.get("ok").and_then(Value::as_bool) == Some(true)).then(|| v["result"].clone())
    };
    tokio::time::timeout(Duration::from_secs(5), io)
        .await
        .ok()?
}

#[cfg(not(unix))]
pub async fn request(_sock: &Path, _req: &Value) -> Option<Value> {
    None
}

/// The channel an intent goes to: its route's opaque `route` token, else
/// its `chat`. Gray only marks an intent `discord` when it has a route.
pub fn channel_for(intent: &Value) -> Option<u64> {
    let route = intent.get("route");
    let pick = |k: &str| {
        route
            .and_then(|r| r.get(k))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    };
    pick("route").or_else(|| pick("chat"))?.trim().parse().ok()
}

/// What is posted: autonomous output gets a small provenance line so a
/// trigger result never reads like a reply to something the owner said.
pub fn render(intent: &Value) -> String {
    let text = intent
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    match intent.get("kind").and_then(Value::as_str) {
        Some("trigger") => format!("-# ⚡ trigger\n{text}"),
        Some("system") => format!("-# ⚙ gray\n{text}"),
        _ => text.to_string(),
    }
}

/// Where a post to `channel` is remembered: the DM conversation's home (the
/// same layout `runner::run_gray_input` builds under `conversations`).
// ponytail: DM key only; guild channels key per user, so a post there is not
// carried into replies. Owner routes are DMs.
fn posted_file(conversations: &Path, channel: u64) -> PathBuf {
    let conv = crate::channels::discord::session::conversation_key(&channel.to_string(), "", true);
    conversations
        .join(crate::channels::discord::runner::hex_sha256(
            conv.as_bytes(),
        ))
        .join(POSTED)
}

const POSTED: &str = "always-on-posted.txt";
/// Most recent posted text carried into the next turn.
const POSTED_KEEP: usize = 4000;

/// Remember a post so the owner's reply to it has context.
fn note_posted(conversations: &Path, channel: u64, body: &str) {
    use std::io::Write as _;
    let path = posted_file(conversations, channel);
    let _ = path.parent().map(std::fs::create_dir_all);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{body}\n");
    }
}

/// The prompt for a turn in conversation home `home`, with whatever the
/// gateway posted there since the last turn in front of it. The note is
/// consumed: it is in this turn's transcript from here on.
pub fn with_posted(home: &Path, prompt: String) -> String {
    let path = home.join(POSTED);
    let Ok(posted) = std::fs::read_to_string(&path) else {
        return prompt;
    };
    let _ = std::fs::remove_file(&path);
    let posted = posted.trim();
    if posted.is_empty() {
        return prompt;
    }
    let cut = posted
        .char_indices()
        .rev()
        .nth(POSTED_KEEP)
        .map_or(0, |(i, _)| i);
    format!(
        "[Since your last reply here, you posted this to the owner on your own \
         (trigger/wake); they may be replying to it:]\n{}\n[end]\n\n{prompt}",
        &posted[cut..]
    )
}

pub async fn pump(
    gray_home: PathBuf,
    conversations: PathBuf,
    rest: crate::channels::discord::transport::Rest,
) {
    let sock = gray_home.join("gateway.sock");
    let mut every = tokio::time::interval(PUMP_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        every.tick().await;
        if !sock.exists() {
            continue;
        }
        let Some(result) = request(
            &sock,
            &json!({"verb": "pull", "protocol": 1, "platform": "discord", "limit": 10}),
        )
        .await
        else {
            continue;
        };
        let intents = result
            .get("intents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut acked = Vec::new();
        for intent in &intents {
            let Some(id) = intent.get("id").and_then(Value::as_str) else {
                continue;
            };
            let body = render(intent);
            let Some(channel) = channel_for(intent) else {
                crate::channels::discord::eprintln_stamped!(
                    "[discord] always-on delivery {id} has no channel; dropping"
                );
                acked.push(id.to_string());
                continue;
            };
            if body.trim().is_empty() {
                acked.push(id.to_string());
                continue;
            }
            // The intent id is the nonce: a retried post after a lost ack
            // is deduplicated by Discord instead of posted twice.
            let nonce = &crate::channels::discord::runner::hex_sha256(id.as_bytes())[..24];
            match rest.send_text_v2(channel, &body, Some(nonce)).await {
                Ok(_) => {
                    note_posted(&conversations, channel, &body);
                    acked.push(id.to_string());
                }
                Err(e) => {
                    crate::channels::discord::eprintln_stamped!(
                        "[discord] always-on delivery {id} failed: {e:?}"
                    )
                }
            }
        }
        if !acked.is_empty() {
            let _ = request(&sock, &json!({"verb": "ack", "protocol": 1, "ids": acked})).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_prefers_route_then_chat() {
        let i = json!({"route": {"platform": "discord", "chat": "11", "route": "22"}});
        assert_eq!(channel_for(&i), Some(22));
        let i = json!({"route": {"platform": "discord", "chat": "11"}});
        assert_eq!(channel_for(&i), Some(11));
        assert_eq!(channel_for(&json!({"route": null})), None);
    }

    #[test]
    fn autonomous_output_is_labelled() {
        let t = json!({"kind": "trigger", "text": "PR #12 is red"});
        assert!(render(&t).starts_with("-# ⚡ trigger\nPR #12 is red"));
        assert_eq!(render(&json!({"kind": "user", "text": " hi "})), "hi");
    }

    #[test]
    fn a_post_rides_along_with_the_next_turn_once() {
        let dir = tempfile::tempdir().unwrap();
        note_posted(dir.path(), 42, "disk at 91%");
        let home = posted_file(dir.path(), 42).parent().unwrap().to_path_buf();
        let p = with_posted(&home, "which mount?".into());
        assert!(
            p.contains("disk at 91%") && p.ends_with("which mount?"),
            "{p}"
        );
        assert_eq!(with_posted(&home, "again".into()), "again");
    }
}
