//! Transactional inbox/outbox queue.
//! Generation and delivery are separate: a failed delivery retries the same
//! chunks, never the agent turn. Interrupted work is marked uncertain.
use rusqlite::{Connection, params};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct InboxItem {
    pub id: String,
    pub channel: String,
    pub conversation: String,
    pub prompt: String,
    pub state: String,
    pub created: f64,
    pub error: Option<String>,
    pub receipt: Option<String>,
    pub cancel: bool,
    pub interaction_token: Option<String>,
    pub app_id: Option<String>,
    /// Canonical structured input JSON for component turns; `None` keeps
    /// legacy prompt rows unchanged.
    pub input_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OutboxPart {
    pub id: String,
    pub part: i64,
    pub content: String,
    pub channel: String,
    pub attempts: i64,
    /// Slash followup routing (Task 9): `Some` when the row was enqueued from
    /// `/ask`. The production deliver closure sends these via the interaction
    /// webhook (`Rest::followup`) instead of the channel (`Rest::send`).
    pub interaction_token: Option<String>,
    /// Application id paired with `interaction_token` for webhook followups.
    pub app_id: Option<String>,
    /// `Some("v2")` for new Components V2 rows; `None` preserves old rows
    /// written before the renderer migration.
    pub render: Option<String>,
    /// Serialized typed document for new V2 output rows.
    pub document_json: Option<String>,
    pub document_version: Option<u32>,
}

/// One row of the `queue list` view.
pub type QueueRow = (String, String, String, Option<String>);

#[derive(Debug, Clone)]
pub struct Schedule {
    pub id: String,
    pub interval: i64,
    pub prompt: String,
    pub next_at: f64,
    pub status: String,
    /// Where a due fire lands, and which gray home it runs in. Empty on
    /// rows written before `/cron` existed; the ticker then falls back to
    /// the configured home channel.
    pub channel: String,
    pub conversation: String,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS inbox (
    id TEXT PRIMARY KEY, channel TEXT NOT NULL, conversation TEXT NOT NULL,
    prompt TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'queued',
    created REAL NOT NULL, error TEXT, receipt TEXT, cancel INTEGER DEFAULT 0,
    interaction_token TEXT, app_id TEXT, input_json TEXT);
CREATE TABLE IF NOT EXISTS outbox (
    id TEXT NOT NULL, part INTEGER NOT NULL, content TEXT NOT NULL,
    message_id TEXT, attempts INTEGER NOT NULL DEFAULT 0,
    next_at REAL NOT NULL DEFAULT 0, error TEXT, render TEXT,
    document_json TEXT, document_version INTEGER,
    PRIMARY KEY(id,part));
CREATE TABLE IF NOT EXISTS schedules (
    id TEXT PRIMARY KEY, interval INTEGER NOT NULL, prompt TEXT NOT NULL,
    next_at REAL NOT NULL, status TEXT NOT NULL DEFAULT 'scheduled',
    channel TEXT NOT NULL DEFAULT '', conversation TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS pairings (
    code TEXT PRIMARY KEY, user_id TEXT NOT NULL, created REAL NOT NULL);
CREATE TABLE IF NOT EXISTS component_states (
    token TEXT PRIMARY KEY, kind TEXT NOT NULL, user_id TEXT NOT NULL,
    channel_id TEXT NOT NULL, resource_id TEXT NOT NULL,
    created REAL NOT NULL, expires REAL NOT NULL);
CREATE TABLE IF NOT EXISTS ui_component_documents (
    document_id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    guild_id TEXT,
    channel_id TEXT NOT NULL,
    message_id TEXT,
    modal_id TEXT,
    surface TEXT NOT NULL,
    revision INTEGER NOT NULL,
    protocol_version INTEGER NOT NULL,
    status TEXT NOT NULL,
    expires_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS ui_component_states (
    token TEXT PRIMARY KEY,
    document_id TEXT NOT NULL,
    logical_id TEXT NOT NULL,
    action TEXT NOT NULL,
    user_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    state_json TEXT NOT NULL,
    one_shot INTEGER NOT NULL,
    expires_at REAL NOT NULL,
    consumed_at REAL,
    FOREIGN KEY(document_id) REFERENCES ui_component_documents(document_id)
);
CREATE TABLE IF NOT EXISTS ui_component_events (
    interaction_id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL,
    token TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    conversation TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at REAL NOT NULL,
    delivered_at REAL
);
CREATE TABLE IF NOT EXISTS ui_component_files (
    file_id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    media_type TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    expires_at REAL NOT NULL,
    ref_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS ui_component_states_document_idx
    ON ui_component_states(document_id, expires_at);
CREATE INDEX IF NOT EXISTS ui_component_events_document_idx
    ON ui_component_events(document_id, created_at);
CREATE TABLE IF NOT EXISTS asks (
    ask_id TEXT PRIMARY KEY,
    channel TEXT NOT NULL,
    message_id TEXT,
    questions_json TEXT NOT NULL,
    answers_json TEXT NOT NULL DEFAULT '{}',
    state TEXT NOT NULL DEFAULT 'open',
    created REAL NOT NULL,
    expires REAL NOT NULL
);
";

/// One question card (a plugin's `host/ask`): what was asked, what has been
/// answered so far (question id -> answers), and whether it is still open.
#[derive(Debug, Clone, PartialEq)]
pub struct AskRow {
    pub ask_id: String,
    pub channel: String,
    pub message_id: Option<String>,
    pub questions: Value,
    pub answers: Value,
    /// `open`, `answered`, `expired` or `cancelled`.
    pub state: String,
    pub expires: f64,
}

fn connect(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|_| "cannot create queue directory".to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let conn = Connection::open(path).map_err(|_| "cannot open queue".to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| "cannot open queue".to_string())?;
    conn.execute_batch("PRAGMA synchronous=FULL")
        .map_err(|_| "cannot open queue".to_string())?;
    conn.execute_batch("PRAGMA foreign_keys=ON")
        .map_err(|_| "cannot open queue".to_string())?;
    conn.execute_batch(SCHEMA)
        .map_err(|_| "cannot open queue".to_string())?;
    // Migrate pre-slash databases that lack the followup columns.
    for col in ["interaction_token TEXT", "app_id TEXT"] {
        let name = col.split_whitespace().next().unwrap_or(col);
        let sql = format!("ALTER TABLE inbox ADD COLUMN {col}");
        match conn.execute_batch(&sql) {
            Ok(()) => {}
            Err(e) => {
                if !e.to_string().contains(name) {
                    return Err("cannot migrate queue".to_string());
                }
            }
        }
    }
    // Migrate pre-V2 databases without changing the renderer of old rows.
    // Every column in SCHEMA that an old database may lack belongs here:
    // `render` was missed once and the live daemon crash-looped on
    // `next_delivery` (`no such column: o.render`) while all tests, which
    // build fresh databases, stayed green.
    for (table, column) in [
        ("inbox", "input_json TEXT"),
        ("outbox", "render TEXT"),
        ("outbox", "document_json TEXT"),
        ("outbox", "document_version INTEGER"),
    ] {
        let name = column.split_whitespace().next().unwrap_or(column);
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column}");
        match conn.execute_batch(&sql) {
            Ok(()) => {}
            Err(e) => {
                if !e.to_string().contains(name) {
                    return Err("cannot migrate queue".to_string());
                }
            }
        }
    }

    // Migrate pre-slash databases whose schedules carried no target.
    for col in [
        "channel TEXT NOT NULL DEFAULT ''",
        "conversation TEXT NOT NULL DEFAULT ''",
    ] {
        let name = col.split_whitespace().next().unwrap_or(col);
        let sql = format!("ALTER TABLE schedules ADD COLUMN {col}");
        match conn.execute_batch(&sql) {
            Ok(()) => {}
            Err(e) => {
                if !e.to_string().contains(name) {
                    return Err("cannot migrate queue".to_string());
                }
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(conn)
}

pub(crate) fn with_conn<T>(
    path: &Path,
    f: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<T, String> {
    let conn = connect(path)?;
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|_| "queue is busy".to_string())?;
    match f(&conn) {
        Ok(v) => {
            conn.execute_batch("COMMIT")
                .map_err(|_| "cannot commit queue".to_string())?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn row_item(r: &rusqlite::Row) -> Result<InboxItem, rusqlite::Error> {
    Ok(InboxItem {
        id: r.get("id")?,
        channel: r.get("channel")?,
        conversation: r.get("conversation")?,
        prompt: r.get("prompt")?,
        state: r.get("state")?,
        created: r.get("created")?,
        error: r.get("error")?,
        receipt: r.get("receipt")?,
        cancel: r.get::<_, i64>("cancel")? != 0,
        interaction_token: r.get("interaction_token")?,
        app_id: r.get("app_id")?,
        input_json: r.get("input_json")?,
    })
}

/// Transactional inbox/outbox/schedule store.
///
/// `Clone` is cheap (path only — every method opens a fresh connection), so
/// the gateway event loop and worker tasks can share one store handle.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: &Path) -> Result<Self, String> {
        connect(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn enqueue(
        &self,
        id: &str,
        channel: &str,
        prompt: &str,
        conversation: Option<&str>,
        capacity: u64,
    ) -> Result<bool, String> {
        if prompt.trim().is_empty() || prompt.len() > 32000 {
            return Err("Prompt must contain 1–32000 characters".to_string());
        }
        let conv = conversation
            .map(str::to_string)
            .unwrap_or_else(|| format!("chat:{channel}"));
        with_conn(&self.path, |db| {
            enqueue_in_tx(
                db,
                EnqueueRequest {
                    id,
                    channel,
                    conversation: &conv,
                    prompt,
                    input_json: None,
                    interaction_token: None,
                    app_id: None,
                    capacity,
                },
            )
        })
    }

    /// Enqueue a validated structured input without turning it into prompt
    /// prose. The canonical JSON remains available to the native runner.
    pub fn enqueue_component_json(
        &self,
        id: &str,
        channel: &str,
        conversation: &str,
        input_json: &Value,
        capacity: u64,
    ) -> Result<bool, String> {
        if !input_json.is_object() {
            return Err("Structured input must be an object".to_string());
        }
        let encoded = serde_json::to_string(input_json)
            .map_err(|_| "cannot encode structured input".to_string())?;
        with_conn(&self.path, |db| {
            enqueue_in_tx(
                db,
                EnqueueRequest {
                    id,
                    channel,
                    conversation,
                    prompt: "component_event",
                    input_json: Some(&encoded),
                    interaction_token: None,
                    app_id: None,
                    capacity,
                },
            )
        })
    }

    pub fn claim(&self) -> Result<Option<InboxItem>, String> {
        self.claim_except(&[])
    }

    pub fn claim_except(&self, skip: &[String]) -> Result<Option<InboxItem>, String> {
        with_conn(&self.path, |db| {
            let skip_sql = if skip.is_empty() {
                String::new()
            } else {
                format!(
                    " AND IFNULL(q.conversation,'') NOT IN ({})",
                    vec!["?"; skip.len()].join(",")
                )
            };
            let sql = format!(
                "SELECT * FROM inbox q WHERE state='queued' AND cancel=0{skip_sql} AND NOT EXISTS
                 (SELECT 1 FROM inbox r WHERE r.conversation=q.conversation AND r.state='running')
                 ORDER BY created,id LIMIT 1"
            );
            let item: Option<InboxItem> = db
                .query_row(&sql, rusqlite::params_from_iter(skip.iter()), row_item)
                .optional_str()?;
            if let Some(ref it) = item {
                db.execute(
                    "UPDATE inbox SET state='running' WHERE id=?1",
                    params![it.id],
                )
                .map_err(|_| "cannot claim".to_string())?;
            }
            Ok(item)
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<InboxItem>, String> {
        with_conn(&self.path, |db| {
            db.query_row("SELECT * FROM inbox WHERE id=?1", params![id], row_item)
                .optional_str()
        })
    }

    pub fn items(&self) -> Result<Vec<QueueRow>, String> {
        with_conn(&self.path, |db| {
            let mut stmt = db
                .prepare("SELECT id,channel,state,error FROM inbox ORDER BY created DESC LIMIT 100")
                .map_err(|_| "cannot list queue".to_string())?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(|_| "cannot list queue".to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|_| "cannot list queue".to_string())
        })
    }

    pub fn cancel(&self, id: &str) -> Result<(), String> {
        with_conn(&self.path, |db| {
            let n = db.execute(
                "UPDATE inbox SET cancel=1, state=CASE WHEN state='queued' THEN 'cancelled' ELSE state END
                 WHERE id=?1 AND state IN ('queued','running')",
                params![id],
            ).map_err(|_| "cannot cancel".to_string())?;
            if n == 0 {
                return Err("No queued/running item with that ID".to_string());
            }
            Ok(())
        })
    }

    pub fn complete(&self, id: &str, text: &str, receipt: &Value) -> Result<(), String> {
        if text.trim().is_empty() || text.len() > 200000 {
            return Err("Completed answer must contain 1–200000 characters".to_string());
        }
        let chunks =
            crate::channels::discord::text::split_message(text, 2000).map_err(|e| e.to_string())?;
        let mut doc_part: Option<Value> = None;
        let kept: Vec<String> = if chunks.len() <= 8 {
            chunks
        } else {
            let channel: String = with_conn(&self.path, |db| {
                db.query_row("SELECT channel FROM inbox WHERE id=?1", params![id], |r| {
                    r.get(0)
                })
                .optional_str()
            })?
            .ok_or_else(|| "Item is not running".to_string())?;
            let tail = chunks[7..].join("\n");
            let pages = crate::channels::discord::pager::paginate(
                &tail,
                crate::channels::discord::pager::PAGE_LIMIT,
            );
            let token = crate::channels::discord::pager::mint(
                self,
                "*",
                &channel,
                &pages,
                "",
                crate::channels::discord::render::QUIET_ACCENT,
            )?;
            let components = crate::channels::discord::pager::components(
                &pages,
                0,
                &token,
                "",
                crate::channels::discord::render::QUIET_ACCENT,
            );
            doc_part = Some(serde_json::json!({"components": components}));
            chunks[..7].to_vec()
        };
        let receipt_text =
            serde_json::to_string(receipt).map_err(|_| "cannot encode receipt".to_string())?;
        with_conn(&self.path, |db| {
            let n = db
                .execute(
                    "UPDATE inbox SET state='delivery',receipt=?1 WHERE id=?2 AND state='running'",
                    params![receipt_text, id],
                )
                .map_err(|_| "cannot complete".to_string())?;
            if n == 0 {
                return Err("Item is not running".to_string());
            }
            for (part, chunk) in kept.iter().enumerate() {
                db.execute(
                    // Plain content, not a V2 card: the answer is a new
                    // message, and a push notification previews only
                    // `content`.
                    "INSERT INTO outbox(id,part,content,render,document_json,document_version) VALUES(?1,?2,?3,'text',NULL,NULL)",
                    params![id, part as i64, chunk],
                )
                .map_err(|_| "cannot complete".to_string())?;
            }
            if let Some(document) = doc_part {
                let encoded = serde_json::to_string(&document)
                    .map_err(|_| "cannot encode pager document".to_string())?;
                db.execute(
                    "INSERT INTO outbox(id,part,content,render,document_json,document_version) VALUES(?1,?2,'','v2',?3,1)",
                    params![id, kept.len() as i64, encoded],
                )
                .map_err(|_| "cannot complete".to_string())?;
            }
            Ok(())
        })
    }

    /// Complete a turn whose answer already streamed into Discord. The
    /// landed messages are recorded as delivered parts (body and message
    /// id), so recovery never posts them again. `media` is a media-only part
    /// (`MEDIA:` lines) the delivery worker still has to upload; without it
    /// the turn is `sent` at once.
    pub fn complete_streamed(
        &self,
        id: &str,
        receipt: &Value,
        parts: &[(String, String)],
        media: Option<&str>,
    ) -> Result<(), String> {
        if parts.is_empty() && media.is_none() {
            return Err("A streamed answer needs a delivered part or media".to_string());
        }
        let receipt_text =
            serde_json::to_string(receipt).map_err(|_| "cannot encode receipt".to_string())?;
        let state = if media.is_some() { "delivery" } else { "sent" };
        with_conn(&self.path, |db| {
            let n = db
                .execute(
                    "UPDATE inbox SET state=?1,receipt=?2 WHERE id=?3 AND state='running'",
                    params![state, receipt_text, id],
                )
                .map_err(|_| "cannot complete".to_string())?;
            if n == 0 {
                return Err("Item is not running".to_string());
            }
            for (part, (content, message_id)) in parts.iter().enumerate() {
                db.execute(
                    "INSERT INTO outbox(id,part,content,render,message_id) VALUES(?1,?2,?3,'v2',?4)",
                    params![id, part as i64, content, message_id],
                )
                .map_err(|_| "cannot complete".to_string())?;
            }
            if let Some(media) = media {
                db.execute(
                    "INSERT INTO outbox(id,part,content,render) VALUES(?1,?2,?3,'v2')",
                    params![id, parts.len() as i64, media],
                )
                .map_err(|_| "cannot complete".to_string())?;
            }
            Ok(())
        })
    }

    /// Complete a turn with a precompiled V2 document. The document is kept
    /// as JSON in the durable row so delivery can resume after a restart;
    /// callers must run it through the compiler before calling this method.
    pub fn complete_document(
        &self,
        id: &str,
        document: &Value,
        receipt: &Value,
    ) -> Result<(), String> {
        if !document.is_object()
            || document
                .get("components")
                .and_then(Value::as_array)
                .is_none()
        {
            return Err("compiled document is invalid".to_string());
        }
        let encoded = serde_json::to_string(document)
            .map_err(|_| "cannot encode compiled document".to_string())?;
        with_conn(&self.path, |db| {
            let updated = db.execute(
                "UPDATE inbox SET state='delivery',error=NULL,receipt=?1 WHERE id=?2 AND state='running'",
                params![receipt.to_string(), id],
            ).map_err(|_| "cannot complete document".to_string())?;
            if updated == 0 {
                return Err("Item is not running".to_string());
            }
            db.execute("DELETE FROM outbox WHERE id=?1", params![id])
                .map_err(|_| "cannot replace document output".to_string())?;
            db.execute(
                "INSERT INTO outbox(id,part,content,render,document_json,document_version)
                 VALUES(?1,0,'','v2',?2,1)",
                params![id, encoded],
            )
            .map_err(|_| "cannot store compiled document".to_string())?;
            Ok(())
        })
    }

    pub fn fail(&self, id: &str, code: &str) -> Result<(), String> {
        self.fail_with(id, code, None, None)
    }

    pub fn fail_reason(
        &self,
        id: &str,
        code: &str,
        reason: Option<&str>,
        shown: Option<&str>,
    ) -> Result<(), String> {
        self.fail_with(id, code, reason, shown)
    }

    /// [`Self::fail`] for a turn whose live card already shows the failure
    /// (message `shown`): the notice is recorded as delivered there instead
    /// of being posted a second time.
    pub fn fail_shown(&self, id: &str, code: &str, shown: &str) -> Result<(), String> {
        self.fail_with(id, code, None, Some(shown))
    }

    fn fail_with(
        &self,
        id: &str,
        code: &str,
        reason: Option<&str>,
        shown: Option<&str>,
    ) -> Result<(), String> {
        let code = match code {
            "interrupted" | "cancelled" | "timeout" | "stalled" | "agent_failed"
            | "budget_blocked" => code,
            _ => "agent_failed",
        };
        let notice = failure_notice(code, reason);
        with_conn(&self.path, |db| {
            db.execute(
                "UPDATE inbox SET state='uncertain',error=?1 WHERE id=?2 AND state='running'",
                params![code, id],
            )
            .map_err(|_| "cannot fail".to_string())?;
            db.execute(
                "INSERT OR IGNORE INTO outbox(id,part,content,render,document_json,document_version,message_id) VALUES(?1,0,?2,'text',NULL,NULL,?3)",
                params![id, notice, shown],
            )
            .map_err(|_| "cannot fail".to_string())?;
            Ok(())
        })
    }

    /// Channel of every turn still marked running, one entry per turn.
    pub fn running_channels(&self) -> Result<Vec<String>, String> {
        with_conn(&self.path, |db| {
            let mut stmt = db
                .prepare("SELECT channel FROM inbox WHERE state='running' ORDER BY created")
                .map_err(|_| "cannot list running turns".to_string())?;
            let rows = stmt
                .query_map([], |r| r.get(0))
                .map_err(|_| "cannot list running turns".to_string())?;
            rows.collect::<Result<Vec<String>, _>>()
                .map_err(|_| "cannot list running turns".to_string())
        })
    }

    pub fn recover(&self) -> Result<(), String> {
        let ids: Vec<String> = with_conn(&self.path, |db| {
            let mut stmt = db
                .prepare("SELECT id FROM inbox WHERE state='running'")
                .map_err(|_| "cannot recover".to_string())?;
            let rows = stmt
                .query_map([], |r| r.get(0))
                .map_err(|_| "cannot recover".to_string())?;
            rows.collect::<Result<Vec<String>, _>>()
                .map_err(|_| "cannot recover".to_string())
        })?;
        for id in ids {
            self.fail(&id, "interrupted")?;
        }
        Ok(())
    }

    pub fn next_delivery(&self, now: f64) -> Result<Option<OutboxPart>, String> {
        with_conn(&self.path, |db| {
            db.query_row(
                "SELECT o.id,o.part,o.content,i.channel,o.attempts,i.interaction_token,i.app_id,o.render,o.document_json,o.document_version FROM outbox o JOIN inbox i ON i.id=o.id
                 WHERE message_id IS NULL AND next_at<=?1 AND NOT EXISTS
                 (SELECT 1 FROM outbox p WHERE p.id=o.id AND p.part<o.part AND p.message_id IS NULL)
                 ORDER BY i.created,o.part LIMIT 1",
                params![now],
                |r| Ok(OutboxPart { id: r.get(0)?, part: r.get(1)?, content: r.get(2)?, channel: r.get(3)?, attempts: r.get(4)?, interaction_token: r.get(5)?, app_id: r.get(6)?, render: r.get(7)?, document_json: r.get(8)?, document_version: r.get(9)? }),
            ).optional_str()
        })
    }

    pub fn ack(&self, id: &str, part: i64, message_id: &str) -> Result<(), String> {
        with_conn(&self.path, |db| {
            db.execute(
                "UPDATE outbox SET message_id=?1,error=NULL WHERE id=?2 AND part=?3",
                params![message_id, id, part],
            )
            .map_err(|_| "cannot ack".to_string())?;
            let pending: bool = db
                .query_row(
                    "SELECT 1 FROM outbox WHERE id=?1 AND message_id IS NULL",
                    params![id],
                    |_| Ok(true),
                )
                .optional_str()?
                .is_some();
            if !pending {
                db.execute(
                    "UPDATE inbox SET state='sent' WHERE id=?1 AND state='delivery'",
                    params![id],
                )
                .map_err(|_| "cannot ack".to_string())?;
            }
            Ok(())
        })
    }

    pub fn delivery_failed(&self, part: &OutboxPart, code: &str, now: f64) -> Result<(), String> {
        let shift = (part.attempts + 1).clamp(0, 12) as u32;
        let delay = (2u64.saturating_pow(shift)).min(3600) as f64;
        with_conn(&self.path, |db| {
            db.execute(
                "UPDATE outbox SET attempts=attempts+1,next_at=?1,error=?2 WHERE id=?3 AND part=?4",
                params![now + delay, code, part.id, part.part],
            )
            .map_err(|_| "cannot record delivery failure".to_string())?;
            Ok(())
        })
    }

    pub fn schedule_add(
        &self,
        id: &str,
        interval: u64,
        prompt: &str,
        channel: &str,
        conversation: &str,
        now: f64,
    ) -> Result<(), String> {
        if interval < 60 || prompt.trim().is_empty() || prompt.len() > 32000 {
            return Err("Interval must be >=60s and prompt 1–32000 characters".to_string());
        }
        if channel.trim().is_empty() || conversation.trim().is_empty() {
            return Err("A schedule needs a channel and a conversation".to_string());
        }
        with_conn(&self.path, |db| {
            db.execute(
                "INSERT INTO schedules(id,interval,prompt,next_at,channel,conversation)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    id,
                    interval as i64,
                    prompt,
                    now + interval as f64,
                    channel,
                    conversation
                ],
            )
            .map_err(|_| "cannot add schedule".to_string())?;
            Ok(())
        })
    }

    pub fn schedules(&self) -> Result<Vec<Schedule>, String> {
        with_conn(&self.path, |db| {
            let mut stmt = db
                .prepare(
                    "SELECT id,interval,prompt,next_at,status,channel,conversation
                     FROM schedules ORDER BY id",
                )
                .map_err(|_| "cannot list schedules".to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Schedule {
                        id: r.get(0)?,
                        interval: r.get(1)?,
                        prompt: r.get(2)?,
                        next_at: r.get(3)?,
                        status: r.get(4)?,
                        channel: r.get(5)?,
                        conversation: r.get(6)?,
                    })
                })
                .map_err(|_| "cannot list schedules".to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|_| "cannot list schedules".to_string())
        })
    }

    pub fn schedule_remove(&self, id: &str) -> Result<(), String> {
        with_conn(&self.path, |db| {
            let n = db
                .execute("DELETE FROM schedules WHERE id=?1", params![id])
                .map_err(|_| "cannot remove schedule".to_string())?;
            if n == 0 {
                return Err("Schedule not found".to_string());
            }
            Ok(())
        })
    }

    /// Create an opaque, expiring state token for an interactive component.
    /// The resource id stays in SQLite; it is never placed in `custom_id`.
    pub fn component_state_create(
        &self,
        kind: &str,
        user_id: &str,
        channel_id: &str,
        resource_id: &str,
        ttl_secs: u64,
    ) -> Result<String, String> {
        if kind.trim().is_empty()
            || user_id.trim().is_empty()
            || channel_id.trim().is_empty()
            || resource_id.trim().is_empty()
        {
            return Err("Component state fields must not be empty".to_string());
        }
        let token = uuid_hex();
        let now = now_secs();
        let expires = now + ttl_secs.clamp(60, 86_400) as f64;
        with_conn(&self.path, |db| {
            db.execute(
                "DELETE FROM component_states WHERE expires < ?1",
                params![now],
            )
            .map_err(|_| "cannot prune component state".to_string())?;
            db.execute(
                "INSERT INTO component_states(token,kind,user_id,channel_id,resource_id,created,expires)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![token, kind, user_id, channel_id, resource_id, now, expires],
            )
            .map(|_| ())
            .map_err(|_| "cannot store component state".to_string())
        })?;
        Ok(token)
    }

    /// Consume a component token only for its original user, channel, and
    /// action kind. Expired and mismatched tokens are removed/fail closed.
    pub fn component_state_take(
        &self,
        token: &str,
        user_id: &str,
        channel_id: &str,
        kind: &str,
    ) -> Result<Option<String>, String> {
        let now = now_secs();
        with_conn(&self.path, |db| {
            let row: Option<(String, String, String, String, f64)> = db
                .query_row(
                    "SELECT kind,user_id,channel_id,resource_id,expires
                     FROM component_states WHERE token=?1",
                    params![token],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional_str()?;
            let Some((stored_kind, stored_user, stored_channel, resource, expires)) = row else {
                return Ok(None);
            };
            // `*` binds a token to its channel alone: any admitted user
            // there may press it (a turn's Stop button).
            let valid = expires >= now
                && stored_kind == kind
                && (stored_user == user_id || stored_user == "*")
                && stored_channel == channel_id;
            if valid {
                db.execute(
                    "DELETE FROM component_states WHERE token=?1",
                    params![token],
                )
                .map_err(|_| "cannot consume component state".to_string())?;
            } else {
                db.execute(
                    "DELETE FROM component_states WHERE token=?1",
                    params![token],
                )
                .map_err(|_| "cannot remove component state".to_string())?;
            }
            Ok(valid.then_some(resource))
        })
    }

    pub fn component_state_get(
        &self,
        token: &str,
        user_id: &str,
        channel_id: &str,
        kind: &str,
    ) -> Result<Option<String>, String> {
        let now = now_secs();
        with_conn(&self.path, |db| {
            let row: Option<(String, String, String, String, f64)> = db
                .query_row(
                    "SELECT kind,user_id,channel_id,resource_id,expires
                     FROM component_states WHERE token=?1",
                    params![token],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional_str()?;
            let Some((stored_kind, stored_user, stored_channel, resource, expires)) = row else {
                return Ok(None);
            };
            let valid = expires >= now
                && stored_kind == kind
                && (stored_user == user_id || stored_user == "*")
                && stored_channel == channel_id;
            if !valid {
                db.execute(
                    "DELETE FROM component_states WHERE token=?1",
                    params![token],
                )
                .map_err(|_| "cannot remove component state".to_string())?;
            }
            Ok(valid.then_some(resource))
        })
    }

    /// A pending pairing code for this user, if one is already waiting —
    /// repeat "hi" from an unknown DMer reuses it instead of piling codes up.
    pub fn pairing_code_for(&self, user_id: &str) -> Result<Option<String>, String> {
        with_conn(&self.path, |db| {
            db.query_row(
                "SELECT code FROM pairings WHERE user_id=?1 ORDER BY created DESC LIMIT 1",
                params![user_id],
                |r| r.get(0),
            )
            .optional_str()
            .map_err(|_| "cannot read pairings".to_string())
        })
    }

    pub fn pairing_insert(&self, code: &str, user_id: &str) -> Result<(), String> {
        with_conn(&self.path, |db| {
            db.execute(
                "INSERT OR REPLACE INTO pairings(code,user_id,created) VALUES(?1,?2,?3)",
                params![code, user_id, crate::channels::discord::durable::now_secs()],
            )
            .map(|_| ())
            .map_err(|_| "cannot store pairing".to_string())
        })
    }

    /// The user a pending code was issued to, without consuming the code.
    pub fn pairing_peek(&self, code: &str) -> Result<Option<String>, String> {
        with_conn(&self.path, |db| {
            db.query_row(
                "SELECT user_id FROM pairings WHERE code=?1",
                params![code],
                |r| r.get::<_, String>(0),
            )
            .optional_str()
            .map_err(|_| "cannot read pairings".to_string())
        })
    }

    /// Consume a code exactly once. A replay finds nothing.
    pub fn pairing_take(&self, code: &str) -> Result<Option<String>, String> {
        with_conn(&self.path, |db| {
            let found = db
                .query_row(
                    "SELECT user_id FROM pairings WHERE code=?1",
                    params![code],
                    |r| r.get::<_, String>(0),
                )
                .optional_str()
                .map_err(|_| "cannot read pairings".to_string())?;
            if found.is_some() {
                db.execute("DELETE FROM pairings WHERE code=?1", params![code])
                    .map_err(|_| "cannot consume pairing".to_string())?;
            }
            Ok(found)
        })
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>, String> {
        with_conn(&self.path, |db| {
            db.query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
                r.get(0)
            })
            .optional_str()
        })
    }

    /// Upsert a `meta` key.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), String> {
        with_conn(&self.path, |db| {
            db.execute(
                "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                params![key, value],
            )
            .map(|_| ())
            .map_err(|_| "cannot store meta".to_string())
        })
    }

    /// Every `meta` row whose key starts with `prefix`, as (key, value)
    /// pairs. `instr` is a starts-with test, so the prefix needs no
    /// LIKE-wildcard escaping.
    pub fn meta_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>, String> {
        with_conn(&self.path, |db| {
            let mut stmt = db
                .prepare("SELECT key, value FROM meta WHERE instr(key, ?1) = 1")
                .map_err(|_| "cannot read meta".to_string())?;
            let rows = stmt
                .query_map(params![prefix], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|_| "cannot read meta".to_string())?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|_| "cannot read meta".to_string())?);
            }
            Ok(out)
        })
    }

    /// Newest numeric inbox id per channel. Inbox ids are Discord message
    /// (or interaction) snowflakes, so this is the last message the bot
    /// handled there — the backfill cursor for channels that predate cursors
    /// or were only ever written to while the daemon was down.
    pub fn inbox_heads(&self) -> Result<Vec<(String, u64)>, String> {
        with_conn(&self.path, |db| {
            let mut stmt = db
                .prepare(
                    "SELECT channel, MAX(CAST(id AS INTEGER)) FROM inbox \
                     WHERE id GLOB '[0-9]*' AND id NOT GLOB '*[^0-9]*' GROUP BY channel",
                )
                .map_err(|_| "cannot read inbox".to_string())?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|_| "cannot read inbox".to_string())?;
            let mut out = Vec::new();
            for row in rows {
                let (channel, id) = row.map_err(|_| "cannot read inbox".to_string())?;
                out.push((channel, id as u64));
            }
            Ok(out)
        })
    }

    /// Attach slash followup routing to an enqueued row (`/ask` path).
    pub fn set_interaction(&self, id: &str, token: &str, app_id: &str) -> Result<(), String> {
        with_conn(&self.path, |db| {
            db.execute(
                "UPDATE inbox SET interaction_token=?1,app_id=?2 WHERE id=?3",
                params![token, app_id, id],
            )
            .map(|_| ())
            .map_err(|_| "cannot store interaction".to_string())
        })
    }

    /// Flag the running row of `conversation` for cancellation (`/stop`).
    /// Returns true when a running turn was flagged.
    pub fn cancel_conversation(&self, conversation: &str) -> Result<bool, String> {
        with_conn(&self.path, |db| {
            let n = db
                .execute(
                    "UPDATE inbox SET cancel=1 WHERE conversation=?1 AND state='running'",
                    params![conversation],
                )
                .map_err(|_| "cannot cancel".to_string())?;
            Ok(n > 0)
        })
    }

    /// Cancel work that predates a session reset. Queued rows become terminal
    /// immediately; a running child is flagged so its worker can settle it
    /// without restoring the old session pointer.
    pub fn cancel_pending_conversation(&self, conversation: &str) -> Result<bool, String> {
        with_conn(&self.path, |db| {
            let queued = db
                .execute(
                    "UPDATE inbox SET state='cancelled', error='cancelled' WHERE conversation=?1 AND state='queued'",
                    params![conversation],
                )
                .map_err(|_| "cannot cancel".to_string())?;
            let running = db
                .execute(
                    "UPDATE inbox SET cancel=1 WHERE conversation=?1 AND state='running'",
                    params![conversation],
                )
                .map_err(|_| "cannot cancel".to_string())?;
            Ok(queued + running > 0)
        })
    }

    pub fn requeue(&self, id: &str) -> Result<(), String> {
        with_conn(&self.path, |db| {
            db.execute(
                "UPDATE inbox SET state='queued' WHERE id=?1 AND state='running'",
                params![id],
            )
            .map(|_| ())
            .map_err(|_| "cannot requeue".to_string())
        })
    }

    pub fn ahead_in_conversation(&self, conversation: &str, id: &str) -> Result<i64, String> {
        with_conn(&self.path, |db| {
            db.query_row(
                "SELECT count(*) FROM inbox r WHERE r.conversation=?1 AND r.id<>?2 AND r.cancel=0
                 AND (r.state='running' OR (r.state='queued' AND (r.created,r.id) <
                 (SELECT created,id FROM inbox WHERE id=?2)))",
                params![conversation, id],
                |r| r.get(0),
            )
            .map_err(|_| "cannot count queue".to_string())
        })
    }

    /// Queued + running + delivery rows (`/status` queue depth).
    pub fn pending_count(&self) -> Result<i64, String> {
        with_conn(&self.path, |db| {
            db.query_row(
                "SELECT count(*) FROM inbox WHERE state IN ('queued','running','delivery')",
                [],
                |r| r.get(0),
            )
            .map_err(|_| "cannot count queue".to_string())
        })
    }

    pub fn enqueue_due(&self, channel: &str, now: f64) -> Result<(), String> {
        with_conn(&self.path, |db| {
            let mut stmt = db
                .prepare(
                    "SELECT id,interval,prompt,next_at,channel,conversation
                     FROM schedules WHERE next_at<=?1",
                )
                .map_err(|_| "cannot enqueue due".to_string())?;
            let due: Vec<(String, i64, String, f64, String, String)> = stmt
                .query_map(params![now], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })
                .map_err(|_| "cannot enqueue due".to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "cannot enqueue due".to_string())?;
            for (job_id, interval, prompt, next_at, target, target_conv) in due {
                let inbox_id = format!("job:{job_id}:{next_at}");
                // A job made in another channel keeps its own target; a row
                // from before `/cron` existed (empty target) falls back to
                // the configured home channel and its own gray home.
                let (post_to, conv) = if target.trim().is_empty() {
                    (channel.to_string(), format!("job:{job_id}"))
                } else {
                    (target, target_conv)
                };
                db.execute(
                    "INSERT OR IGNORE INTO inbox(id,channel,conversation,prompt,created) VALUES(?1,?2,?3,?4,?5)",
                    params![inbox_id, post_to, conv, prompt, now],
                ).map_err(|_| "cannot enqueue due".to_string())?;
                db.execute(
                    "UPDATE schedules SET next_at=?1,status='queued' WHERE id=?2",
                    params![now + interval as f64, job_id],
                )
                .map_err(|_| "cannot enqueue due".to_string())?;
            }
            Ok(())
        })
    }

    pub fn migrate_jobs(&self, path: &Path) -> Result<(), String> {
        with_conn(&self.path, |db| {
            let done: bool = db
                .query_row("SELECT 1 FROM meta WHERE key='jobs_migrated'", [], |_| {
                    Ok(true)
                })
                .optional_str()?
                .is_some();
            if done {
                return Ok(());
            }
            if path.exists() {
                let text = std::fs::read_to_string(path)
                    .map_err(|_| "cannot read legacy schedules".to_string())?;
                let jobs: Value = serde_json::from_str(&text)
                    .map_err(|_| "Invalid schedules file".to_string())?;
                let arr = jobs
                    .as_array()
                    .ok_or_else(|| "Invalid schedules file".to_string())?;
                for job in arr {
                    let interval = job.get("interval").and_then(Value::as_i64).unwrap_or(0);
                    if interval < 60 {
                        return Err("Invalid legacy schedule interval".to_string());
                    }
                    let id = job.get("id").and_then(Value::as_str).unwrap_or("");
                    let prompt = job.get("prompt").and_then(Value::as_str).unwrap_or("");
                    let next_at = job.get("next_at").and_then(Value::as_f64).unwrap_or(0.0);
                    let status = job
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("scheduled");
                    db.execute(
                        "INSERT OR IGNORE INTO schedules(id,interval,prompt,next_at,status) VALUES(?1,?2,?3,?4,?5)",
                        params![id, interval, prompt, next_at, status],
                    ).map_err(|_| "cannot migrate schedules".to_string())?;
                }
            }
            db.execute("INSERT INTO meta VALUES('jobs_migrated','1')", [])
                .map_err(|_| "cannot migrate schedules".to_string())?;
            Ok(())
        })
    }

    /// Delete terminal inbox rows (sent/cancelled/uncertain) older than
    /// `retention_secs`, plus their outbox parts.
    pub fn prune_terminal(&self, retention_secs: u64, now: f64) -> Result<usize, String> {
        with_conn(&self.path, |db| {
            let cutoff = now - retention_secs as f64;
            let mut stmt = db.prepare("SELECT id FROM inbox WHERE state IN ('sent','cancelled','uncertain') AND created < ?1")
                .map_err(|_| "cannot prune".to_string())?;
            let ids: Vec<String> = stmt
                .query_map(params![cutoff], |r| r.get(0))
                .map_err(|_| "cannot prune".to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "cannot prune".to_string())?;
            for id in &ids {
                db.execute("DELETE FROM outbox WHERE id=?1", params![id])
                    .map_err(|_| "cannot prune".to_string())?;
                db.execute("DELETE FROM inbox WHERE id=?1", params![id])
                    .map_err(|_| "cannot prune".to_string())?;
            }
            Ok(ids.len())
        })
    }
}

/// Fill `buf` from `/dev/urandom` where it exists; elsewhere mix pid +
/// time + RandomState so ids and pairing codes still differ.
pub(crate) fn fill_entropy(buf: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if std::fs::File::open("/dev/urandom")
            .map(|mut f| f.read_exact(buf).is_ok())
            .unwrap_or(false)
        {
            return;
        }
    }
    use std::hash::{BuildHasher, Hasher};
    for (i, chunk) in buf.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(std::process::id());
        h.write_usize(i);
        if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            h.write_u128(d.as_nanos());
        }
        let bytes = h.finish().to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

/// A short random id for a scheduled job.
pub fn uuid_hex() -> String {
    let mut buf = [0u8; 16];
    crate::channels::discord::durable::fill_entropy(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub(crate) struct EnqueueRequest<'a> {
    pub id: &'a str,
    pub channel: &'a str,
    pub conversation: &'a str,
    pub prompt: &'a str,
    pub input_json: Option<&'a str>,
    pub interaction_token: Option<&'a str>,
    pub app_id: Option<&'a str>,
    pub capacity: u64,
}

pub(crate) fn enqueue_in_tx(db: &Connection, request: EnqueueRequest<'_>) -> Result<bool, String> {
    let dup: bool = db
        .query_row(
            "SELECT 1 FROM inbox WHERE id=?1",
            params![request.id],
            |_| Ok(true),
        )
        .optional_str()?
        .is_some();
    if dup {
        return Ok(false);
    }
    let pending: i64 = db
        .query_row(
            "SELECT count(*) FROM inbox WHERE state IN ('queued','running','delivery')",
            [],
            |r| r.get(0),
        )
        .map_err(|_| "cannot enqueue".to_string())?;
    if pending >= request.capacity as i64 {
        return Err("Queue is full; message was not accepted".to_string());
    }
    db.execute(
        "INSERT INTO inbox(id,channel,conversation,prompt,created,input_json,interaction_token,app_id)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            request.id,
            request.channel,
            request.conversation,
            request.prompt,
            now_secs(),
            request.input_json,
            request.interaction_token,
            request.app_id
        ],
    )
    .map_err(|_| "cannot enqueue".to_string())?;
    Ok(true)
}

fn row_ask(r: &rusqlite::Row) -> Result<AskRow, rusqlite::Error> {
    let questions: String = r.get("questions_json")?;
    let answers: String = r.get("answers_json")?;
    Ok(AskRow {
        ask_id: r.get("ask_id")?,
        channel: r.get("channel")?,
        message_id: r.get("message_id")?,
        questions: serde_json::from_str(&questions).unwrap_or(Value::Null),
        answers: serde_json::from_str(&answers).unwrap_or_else(|_| serde_json::json!({})),
        state: r.get("state")?,
        expires: r.get("expires")?,
    })
}

const ASK_COLUMNS: &str =
    "ask_id,channel,message_id,questions_json,answers_json,state,created,expires";

impl Store {
    /// Open a question card. `questions` is the validated list it asks.
    pub fn ask_create(
        &self,
        ask_id: &str,
        channel: &str,
        questions: &Value,
        expires: f64,
    ) -> Result<(), String> {
        let encoded = serde_json::to_string(questions).map_err(|_| "cannot encode ask")?;
        with_conn(&self.path, |db| {
            db.execute(
                "INSERT INTO asks(ask_id,channel,questions_json,created,expires) VALUES(?1,?2,?3,?4,?5)",
                params![ask_id, channel, encoded, now_secs(), expires],
            )
            .map(|_| ())
            .map_err(|_| "cannot store ask".to_string())
        })
    }

    pub fn ask_set_message(&self, ask_id: &str, message_id: &str) -> Result<(), String> {
        with_conn(&self.path, |db| {
            db.execute(
                "UPDATE asks SET message_id=?1 WHERE ask_id=?2",
                params![message_id, ask_id],
            )
            .map(|_| ())
            .map_err(|_| "cannot update ask".to_string())
        })
    }

    pub fn ask_get(&self, ask_id: &str) -> Result<Option<AskRow>, String> {
        with_conn(&self.path, |db| {
            db.query_row(
                &format!("SELECT {ASK_COLUMNS} FROM asks WHERE ask_id=?1"),
                params![ask_id],
                row_ask,
            )
            .optional_str()
        })
    }

    /// The newest open, unexpired question card in `channel`.
    pub fn ask_open_in(&self, channel: &str, now: f64) -> Result<Option<AskRow>, String> {
        with_conn(&self.path, |db| {
            db.query_row(
                &format!(
                    "SELECT {ASK_COLUMNS} FROM asks WHERE channel=?1 AND state='open' AND expires>?2
                     ORDER BY created DESC LIMIT 1"
                ),
                params![channel, now],
                row_ask,
            )
            .optional_str()
        })
    }

    /// Record `answers` for question `question_id` of an open card. The
    /// first answer to a question wins; once every question has one the
    /// card is `answered`. `None` when the card is closed, expired, or the
    /// question was already answered.
    pub fn ask_answer(
        &self,
        ask_id: &str,
        question_id: &str,
        answers: &[String],
        now: f64,
    ) -> Result<Option<AskRow>, String> {
        with_conn(&self.path, |db| {
            let Some(mut row) = db
                .query_row(
                    &format!("SELECT {ASK_COLUMNS} FROM asks WHERE ask_id=?1"),
                    params![ask_id],
                    row_ask,
                )
                .optional_str()?
            else {
                return Ok(None);
            };
            let known = row.questions.as_array().is_some_and(|qs| {
                qs.iter()
                    .any(|q| q.get("id").and_then(Value::as_str) == Some(question_id))
            });
            if row.state != "open"
                || row.expires <= now
                || !known
                || row.answers.get(question_id).is_some()
            {
                return Ok(None);
            }
            row.answers[question_id] = serde_json::json!(answers);
            let total = row.questions.as_array().map_or(0, Vec::len);
            let answered = row.answers.as_object().map_or(0, |a| a.len());
            if answered >= total {
                row.state = "answered".to_string();
            }
            db.execute(
                "UPDATE asks SET answers_json=?1,state=?2 WHERE ask_id=?3",
                params![row.answers.to_string(), row.state, ask_id],
            )
            .map_err(|_| "cannot record answer".to_string())?;
            Ok(Some(row))
        })
    }

    /// Close an open card as `expired` or `cancelled`. `None` when it had
    /// already closed (answered in the meantime, say).
    pub fn ask_close(&self, ask_id: &str, state: &str) -> Result<Option<AskRow>, String> {
        with_conn(&self.path, |db| {
            let n = db
                .execute(
                    "UPDATE asks SET state=?1 WHERE ask_id=?2 AND state='open'",
                    params![state, ask_id],
                )
                .map_err(|_| "cannot close ask".to_string())?;
            if n == 0 {
                return Ok(None);
            }
            db.query_row(
                &format!("SELECT {ASK_COLUMNS} FROM asks WHERE ask_id=?1"),
                params![ask_id],
                row_ask,
            )
            .optional_str()
        })
    }
}

trait OptionalStr<T> {
    fn optional_str(self) -> Result<Option<T>, String>;
}
impl<T> OptionalStr<T> for Result<T, rusqlite::Error> {
    fn optional_str(self) -> Result<Option<T>, String> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("queue query failed: {e}")),
        }
    }
}

pub fn failure_notice(code: &str, reason: Option<&str>) -> String {
    let reason = reason
        .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|r| !r.is_empty())
        .map(|r| {
            if r.chars().count() > 300 {
                format!("{}…", r.chars().take(300).collect::<String>())
            } else {
                r
            }
        });
    let headline = match (code, reason.as_deref()) {
        ("interrupted", _) => "The gateway restarted in the middle of this".to_string(),
        ("cancelled", _) => "Stopped".to_string(),
        ("timeout", _) => "Gave up: this hit the turn time limit".to_string(),
        ("stalled", Some(r)) => format!("Gave up: {}", lowercase_first(r)),
        ("stalled", None) => "Gave up: no output from the agent for too long".to_string(),
        ("budget_blocked", Some(r)) => format!("Stopped: {}", lowercase_first(r)),
        ("budget_blocked", None) => "Stopped: the budget is used up".to_string(),
        (_, Some(r)) => format!("That failed: {}", lowercase_first(r)),
        (_, None) => "That failed before it could answer".to_string(),
    };
    let next = match code {
        "cancelled" => "",
        "budget_blocked" => "",
        "interrupted" => " Send a message to pick it up.",
        _ => " Send a message to try again.",
    };
    let headline = headline.trim_end_matches('.');
    format!("{headline}. Some steps may already have run.{next}")
}

fn lowercase_first(s: &str) -> String {
    let first_word = s.split_whitespace().next().unwrap_or("");
    let mut chars = first_word.chars();
    let sentence_case = matches!(
        (chars.next(), chars.next()),
        (Some(a), Some(b)) if a.is_uppercase() && b.is_lowercase()
    );
    let name = [
        "Claude",
        "Devin",
        "Discord",
        "GitHub",
        "OpenAI",
        "Gemini",
        "Anthropic",
    ]
    .contains(&first_word.trim_matches(|c: char| !c.is_alphanumeric()));
    if !sentence_case || name {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    if let Some(c) = it.next() {
        out.extend(c.to_lowercase());
    }
    out.extend(it);
    out
}
