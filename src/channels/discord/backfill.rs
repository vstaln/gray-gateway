//! Missed-message backfill.
//!
//! While the daemon is down — most importantly while a hosted seat's VM is
//! idle-sleeping — Discord drops gateway events, so messages sent in that
//! window are lost unless they are replayed on reconnect. The live path
//! stamps every seen message into `meta` under `last_msg:<channel>`; on
//! READY we pull `?after=<cursor>` per channel and run what was missed
//! through the same admission (`policy::incoming`) and dedupe (the inbox
//! primary key) as live traffic. A channel only backfills once it has a
//! cursor, so a fresh install never replays history; the window is capped
//! by count and age so a long outage cannot queue a backlog of stale
//! prompts.

use crate::channels::discord::attachments::AttachmentRef;
use crate::channels::discord::durable::Store;
use crate::channels::discord::transport::Rest;
use serde_json::Value;
use std::path::Path;

/// Cursor prefix in the `meta` table — one row per channel the gateway has
/// ever seen a message in.
const CURSOR_PREFIX: &str = "last_msg:";
/// Most messages replayed per channel per boot.
pub const MAX_MESSAGES: usize = 50;
/// Oldest a replayed message may be, in minutes (snowflake timestamp).
pub const MAX_AGE_MINUTES: i64 = 30;

/// Snowflake → unix milliseconds (Discord epoch 2015-01-01).
fn snowflake_ms(id: u64) -> i64 {
    ((id >> 22) + 1_420_070_400_000) as i64
}

fn parse_id(v: &Value) -> Option<u64> {
    v.as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| v.as_u64())
}

/// Persist the newest message id seen on a channel. The live path calls
/// this on every MESSAGE_CREATE before admission — "seen" means seen, so
/// ineligible traffic never replays either.
pub fn note_message(store: &Store, channel: &str, message_id: &str) {
    if channel.is_empty() || message_id.is_empty() {
        return;
    }
    let _ = store.meta_set(&format!("{CURSOR_PREFIX}{channel}"), message_id);
}

/// Channel cursors as (channel_id, last_seen_message_id) pairs. A channel
/// with no stamped cursor falls back to the newest message the inbox holds
/// for it: a seat that only ever hears messages while asleep would otherwise
/// never get a cursor and never backfill.
pub fn cursors(store: &Store) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = store
        .meta_prefix(CURSOR_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, value)| {
            let channel = key.strip_prefix(CURSOR_PREFIX)?.to_string();
            Some((channel, value.parse::<u64>().ok()?))
        })
        .collect();
    for (channel, head) in store.inbox_heads().unwrap_or_default() {
        if !channel.is_empty() && !out.iter().any(|(c, _)| *c == channel) {
            out.push((channel, head));
        }
    }
    out
}

/// Run a fetched message through the live admission policy. `None` means
/// skip — the cursor still passes over it so it is never reconsidered.
fn admit(
    msg: &Value,
    owner_id: &str,
    allowed: &[String],
    bot_id: &str,
    cutoff_ms: i64,
) -> Option<(u64, String, String, String, bool)> {
    let id = parse_id(msg.get("id")?)?;
    if snowflake_ms(id) < cutoff_ms {
        return None;
    }
    let author = msg.get("author")?;
    let author_id = author.get("id").and_then(Value::as_str)?.to_string();
    let is_bot = author.get("bot").and_then(Value::as_bool).unwrap_or(false);
    let is_dm = msg.get("guild_id").is_none_or(Value::is_null);
    let channel_id = msg
        .get("channel_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if channel_id.is_empty() {
        return None;
    }
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
    let has_files = msg
        .get("attachments")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    let prompt = crate::channels::discord::policy::incoming(
        &author_id, owner_id, is_bot, is_dm, content, bot_id, allowed, has_files,
    )?;
    Some((id, author_id, channel_id, prompt, is_dm))
}

/// Pull messages newer than each channel's cursor and queue the ones that
/// would have run had the gateway been live. Returns the count accepted —
/// the inbox primary key dedupes anything Discord also delivered live, so
/// "exactly once" falls out of storage, not timing.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    rest: &Rest,
    store: &Store,
    config: &Value,
    token: &str,
    workdir: &Path,
    owner_id: &str,
    allowed: &[String],
    bot_id: &str,
    capacity: u64,
) -> usize {
    let cutoff_ms = chrono::Utc::now().timestamp_millis() - MAX_AGE_MINUTES * 60_000;
    let mut accepted = 0usize;
    for (channel, cursor) in cursors(store) {
        let Ok(channel_id) = channel.parse::<u64>() else {
            continue;
        };
        let Ok(messages) = rest
            .messages_after(channel_id, cursor, MAX_MESSAGES as u32)
            .await
        else {
            continue;
        };
        let mut newest = cursor;
        for msg in &messages {
            let Some((id, author_id, channel_id, prompt, is_dm)) =
                admit(msg, owner_id, allowed, bot_id, cutoff_ms)
            else {
                if let Some(id) = msg.get("id").and_then(parse_id) {
                    newest = newest.max(id);
                }
                continue;
            };
            let atts: Vec<AttachmentRef<'_>> = msg
                .get("attachments")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| {
                            Some(AttachmentRef {
                                url: a.get("url").and_then(Value::as_str)?,
                                filename: a
                                    .get("filename")
                                    .and_then(Value::as_str)
                                    .unwrap_or("attachment"),
                                size: a.get("size").and_then(Value::as_u64).unwrap_or(0),
                                content_type: a.get("content_type").and_then(Value::as_str),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let prompt = crate::channels::discord::attachments::enrich_prompt(
                config,
                token,
                workdir,
                &id.to_string(),
                prompt,
                &atts,
            )
            .await;
            let conv =
                crate::channels::discord::session::conversation_key(&channel_id, &author_id, is_dm);
            match store.enqueue(&id.to_string(), &channel_id, &prompt, Some(&conv), capacity) {
                Ok(true) => {
                    accepted += 1;
                    newest = newest.max(id);
                }
                // Already in the inbox — live delivery beat the backfill to
                // it, which is fine: it is handled exactly once.
                Ok(false) => newest = newest.max(id),
                // Queue full or invalid: stop this channel with the cursor
                // at the last processed message so it retries next boot.
                Err(_) => break,
            }
        }
        if newest > cursor {
            note_message(store, &channel, &newest.to_string());
        }
    }
    accepted
}
