//! Where a chat-hosted gray home delivers: the chat it belongs to.
//!
//! A chat bridge (discord, telegram, ...) runs each conversation in its own
//! gray home and marks it two ways: `GRAY_CRON_ORIGIN` on the turn's env, and
//! `route.json` beside the home's `cron/` store. A job created inside such a
//! home must come back to that chat, not to the throwaway REPL session that
//! happened to create it; nobody ever drains that session's inbox.

use std::path::Path;

use super::store::Origin;

fn origin_from(v: &serde_json::Value) -> Option<Origin> {
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let origin = Origin {
        platform: field("platform")?,
        chat: field("chat")?,
        thread: field("thread"),
        route: field("route"),
    };
    let hosted =
        origin.platform != crate::cron_serve::SESSION_PLATFORM && origin.platform != "local";
    hosted.then_some(origin)
}

/// The chat this home belongs to, from its `route.json`.
pub fn from_home(home: &Path) -> Option<Origin> {
    let bytes = std::fs::read(home.join("route.json")).ok()?;
    origin_from(&serde_json::from_slice(&bytes).ok()?)
}

/// The chat a job created now should deliver to: the turn's
/// `GRAY_CRON_ORIGIN`, else the home's `route.json`.
pub fn current(home: &Path) -> Option<Origin> {
    std::env::var("GRAY_CRON_ORIGIN")
        .ok()
        .and_then(|raw| serde_json::from_str(raw.trim()).ok())
        .and_then(|v| origin_from(&v))
        .or_else(|| from_home(home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_file_marks_a_chat_home() {
        let home = tempfile::tempdir().unwrap();
        assert!(from_home(home.path()).is_none());
        std::fs::write(
            home.path().join("route.json"),
            r#"{"platform":"discord","chat":"c1","route":"42"}"#,
        )
        .unwrap();
        let o = from_home(home.path()).unwrap();
        assert_eq!(
            (o.platform.as_str(), o.route.as_deref()),
            ("discord", Some("42"))
        );
    }

    #[test]
    fn repl_and_local_are_not_chats() {
        let v = serde_json::json!({"platform": "repl", "chat": "sid"});
        assert!(origin_from(&v).is_none());
        let v = serde_json::json!({"platform": "local", "chat": "x"});
        assert!(origin_from(&v).is_none());
    }
}
