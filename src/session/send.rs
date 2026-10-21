//! Producer side of the session send mailbox. Wire format is core's
//! `session_send.rs` + `session_mailbox.rs`, byte for byte:
//!   inbox:    <home>/cron/inbox/<session>/<ms:020>-<uuid>.json  (0600, dir 0700)
//!   entry:    {"kind":"send","id","from","text","attachments":[abs],"created_ms"}
//!   receipts: <home>/cron/send-receipts/<id>.json {"id","session","state"}
//! The receiving session (core) claims entries and writes the receipts.

use std::path::{Path, PathBuf};

pub const KIND: &str = "send";

pub const STATE_QUEUED: &str = "queued";
pub const STATE_NOT_LIVE: &str = "queued (session not live)";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn default_label() -> String {
    std::env::var("GRAY_SESSION_ID").unwrap_or_default()
}

pub fn clean_label(raw: &str) -> String {
    let s: String = raw.chars().filter(|c| !c.is_control()).take(64).collect();
    let s = s.trim();
    if s.is_empty() {
        "cli".to_string()
    } else {
        s.to_string()
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Core's `session_mailbox::session_inbox`.
pub fn session_inbox(home: &Path, session: &str) -> Option<PathBuf> {
    let safe = !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    safe.then(|| home.join("cron").join("inbox").join(session))
}

pub fn receipt_dir(home: &Path) -> PathBuf {
    home.join("cron").join("send-receipts")
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

/// Core's `session_mailbox::write_entry`: atomic tmp+rename, 0600, name sorts
/// by enqueue time.
fn write_entry(dir: &Path, body: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{stamp:020}-{}.json", uuid::Uuid::new_v4());
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(&tmp)?, body.as_bytes())?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

pub fn entry_json(id: &str, from: &str, text: &str, attachments: &[PathBuf]) -> String {
    serde_json::json!({
        "kind": KIND,
        "id": id,
        "from": from,
        "text": text,
        "attachments": attachments.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "created_ms": now_ms(),
    })
    .to_string()
}

pub fn enqueue(
    home: &Path,
    session: &str,
    from: &str,
    text: &str,
    attachments: &[PathBuf],
) -> anyhow::Result<String> {
    let dir = session_inbox(home, session)
        .ok_or_else(|| anyhow::anyhow!("unsafe session id {session:?}"))?;
    private_dir(&dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    write_entry(&dir, &entry_json(&id, from, text, attachments))?;
    Ok(id)
}

pub fn read_receipt_state(home: &Path, id: &str) -> Option<String> {
    if !safe_id(id) {
        return None;
    }
    let text = std::fs::read_to_string(receipt_dir(home).join(format!("{id}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("state")?.as_str().map(str::to_string)
}

fn resolve_attachments(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    paths
        .iter()
        .map(|p| {
            let abs =
                std::fs::canonicalize(p).map_err(|e| format!("attach {}: {e}", p.display()))?;
            if abs.is_file() {
                Ok(abs)
            } else {
                Err(format!("attach {}: not a file", p.display()))
            }
        })
        .collect()
}

fn usage(msg: String) -> ! {
    eprintln!("gray send: {msg}");
    std::process::exit(2)
}

pub async fn run(
    session: &str,
    text: &str,
    attach: &[PathBuf],
    from: Option<&str>,
    wait: Option<u64>,
) -> anyhow::Result<()> {
    let home = super::owner_home()?;
    let root = home.join("sessions");
    let session = match super::store::resolve_unique_prefix(&root, session) {
        Ok(id) => id,
        Err(msg) => usage(msg),
    };
    if text.trim().is_empty() && attach.is_empty() {
        usage("nothing to send: give text or --attach".to_string());
    }
    let attachments = resolve_attachments(attach).unwrap_or_else(|e| usage(e));
    let from = clean_label(
        from.map(str::to_string)
            .unwrap_or_else(default_label)
            .as_str(),
    );
    let live = super::store::open_lock_held(&root, &session);
    let id = enqueue(&home, &session, &from, text, &attachments)?;
    let mut state = if live { STATE_QUEUED } else { STATE_NOT_LIVE }.to_string();
    if let Some(secs) = wait {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            if let Some(s) = read_receipt_state(&home, &id) {
                state = s;
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    println!(
        "{}",
        serde_json::json!({ "session": session, "id": id, "state": state })
    );
    Ok(())
}
