//! Discord-side pairing. A user who DMs an unconfigured
//! bot is told their own Discord ID and a one-shot code; the owner approves
//! the code with `gray discord pairing approve discord <code> --as <user_id>` and that user
//! joins the allowlist. No terminal wizard, no 5-minute timer, nothing to
//! hunt — the discovery happens where the user already is.

use serde_json::{Value, json};
use std::path::Path;

/// A fresh code: 8 uppercase alphanumerics (short enough
/// to read off a phone, 36^8 space, single-use).
pub fn gen_code() -> String {
    const ALPHABET: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = [0u8; 8];
    crate::channels::discord::durable::fill_entropy(&mut bytes);
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

/// The reply an unconfigured DMer sees. Their own ID comes from Discord
/// itself, so they cannot typo it; the code is single-use.
pub fn unconfigured_reply(user_id: &str, code: &str) -> String {
    format!(
        "access not configured.\n\
         Your Discord user id: {user_id}\n\
         Pairing code: {code}\n\
         Ask the bot owner to approve with:\n\
         gray discord pairing approve discord {code} --as {user_id}"
    )
}

/// The notice, as an embed: their own ID and the code as fields,
/// the approve command in a code block. Everything the plain text carried,
/// none of the ragged line breaks.
pub fn unconfigured_embed(user_id: &str, code: &str) -> Value {
    const GREY: u32 = 0x57_4F_6E; // warm grey — matches no error state
    json!({
        "title": "Access not configured",
        "color": GREY,
        "description": "You can DM this bot, but it is not configured to answer you yet.",
        "fields": [
            {
                "name": "Your Discord user id",
                "value": format!("`{user_id}`"),
                "inline": true
            },
            {
                "name": "Pairing code",
                "value": format!("`{code}`"),
                "inline": true
            }
        ],
        "footer": {"text": "gray-discord"},
    })
}

/// What the gateway does when a message is not admitted and the sender is a
/// human DMing the bot: mint (or reuse) a code and reply. Bots, guild
/// messages, and admitted senders never see this — `None` for them.
pub struct PairingReply {
    pub text: String,
    pub code: String,
}

pub fn reply_for(
    store: &crate::channels::discord::durable::Store,
    author: &str,
    dm: bool,
    bot: bool,
) -> Option<PairingReply> {
    if bot || !dm || author.is_empty() {
        return None;
    }
    let code = store
        .pairing_code_for(author)
        .ok()
        .flatten()
        .unwrap_or_else(gen_code);
    store.pairing_insert(&code, author).ok()?;
    Some(PairingReply {
        text: unconfigured_reply(author, &code),
        code,
    })
}

/// Consume a code and admit its user: they become `owner_id` if the config
/// has none, otherwise they join `allowed_users`. Config is rewritten
/// privately (0600) exactly as the wizard would.
///
/// `expect_user` must be the Discord ID the code was issued to, as the
/// requester saw it in their DM. A code alone admits nobody: whoever is handed
/// a code still has to state the ID it belongs to, and a mismatch is refused
/// without consuming the code.
pub fn approve(
    config_path: &Path,
    platform: &str,
    code: &str,
    expect_user: &str,
) -> Result<String, String> {
    if platform != "discord" {
        return Err(format!(
            "unknown platform '{platform}'; the only one is discord"
        ));
    }
    let store = crate::channels::discord::gateway::open_store(config_path)?;
    let code = code.trim();
    let issued_to = store
        .pairing_peek(code)?
        .ok_or_else(|| "unknown or already-used pairing code".to_string())?;
    if issued_to != expect_user.trim() {
        return Err(
            "that code was issued to a different user; check the Discord ID in their DM (nothing was approved)"
                .to_string(),
        );
    }
    let user = store
        .pairing_take(code)?
        .ok_or_else(|| "unknown or already-used pairing code".to_string())?;
    let mut config = crate::channels::discord::config::load_config(config_path)?;
    let owner_missing = config
        .get("owner_id")
        .and_then(Value::as_str)
        .is_none_or(|o| o.is_empty());
    let mut note;
    if owner_missing {
        config["owner_id"] = Value::String(user.clone());
        note = format!("{user} is now the bot owner");
    } else {
        let mut allowed: Vec<String> = config
            .get("allowed_users")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if allowed.iter().any(|id| id == &user) {
            return Ok(format!("{user} was already allowed"));
        }
        allowed.push(user.clone());
        config["allowed_users"] = Value::Array(allowed.into_iter().map(Value::String).collect());
        note = format!("{user} can now trigger the bot");
    }
    crate::channels::discord::config::save_config(config_path, &config)?;
    note.push_str("; restart the gateway to apply (it re-reads on start)");
    Ok(note)
}
