//! Durable state and idempotency for typed Discord component interactions.

use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use std::collections::HashMap;

use crate::channels::discord::component_compile::{CompileError, ComponentIdAllocator};
use crate::channels::discord::component_media::FileStore;
use crate::channels::discord::component_protocol::{FileRef, UI_VERSION};
use crate::channels::discord::durable::{Store, enqueue_in_tx, now_secs, uuid_hex, with_conn};

#[derive(Debug, Clone)]
pub struct NewDocument {
    pub document_id: String,
    pub owner_id: String,
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub message_id: Option<String>,
    pub modal_id: Option<String>,
    pub surface: String,
    pub revision: u32,
    pub protocol_version: u32,
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentRecord {
    pub document_id: String,
    pub owner_id: String,
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub message_id: Option<String>,
    pub modal_id: Option<String>,
    pub surface: String,
    pub revision: u32,
    pub protocol_version: u32,
    pub status: String,
    pub expires_at: f64,
}

#[derive(Debug, Clone)]
pub struct NewState {
    pub document_id: String,
    pub logical_id: String,
    pub action: String,
    pub user_id: String,
    pub channel_id: String,
    pub state: Value,
    pub one_shot: bool,
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedState {
    pub token: String,
    pub document_id: String,
    pub logical_id: String,
    pub action: String,
    pub user_id: String,
    pub channel_id: String,
    pub state: Value,
    pub one_shot: bool,
    pub expires_at: f64,
    pub revision: u32,
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub interaction_id: String,
    pub token: String,
    pub user_id: String,
    pub channel_id: String,
    pub kind: String,
    pub payload: Value,
    pub conversation: String,
    pub interaction_token: Option<String>,
    pub app_id: Option<String>,
    pub capacity: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventReceipt {
    pub interaction_id: String,
    pub document_id: String,
    pub queued: bool,
    pub duplicate: bool,
}

impl Store {
    pub fn create_document(&self, request: NewDocument) -> Result<DocumentRecord, String> {
        validate_document(&request)?;
        let now = now_secs();
        let expires_at = now + request.ttl_secs.clamp(60, 86_400) as f64;
        let record = DocumentRecord {
            document_id: request.document_id.clone(),
            owner_id: request.owner_id,
            guild_id: request.guild_id,
            channel_id: request.channel_id,
            message_id: request.message_id,
            modal_id: request.modal_id,
            surface: request.surface,
            revision: request.revision,
            protocol_version: request.protocol_version,
            status: "active".to_string(),
            expires_at,
        };
        with_conn(self.path(), |db| {
            db.execute(
                "INSERT INTO ui_component_documents
                 (document_id,owner_id,guild_id,channel_id,message_id,modal_id,surface,revision,protocol_version,status,expires_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(document_id) DO UPDATE SET
                   owner_id=excluded.owner_id, guild_id=excluded.guild_id,
                   channel_id=excluded.channel_id, message_id=excluded.message_id,
                   modal_id=excluded.modal_id, surface=excluded.surface,
                   revision=excluded.revision, protocol_version=excluded.protocol_version,
                   status='active', expires_at=excluded.expires_at",
                params![
                    record.document_id,
                    record.owner_id,
                    record.guild_id,
                    record.channel_id,
                    record.message_id,
                    record.modal_id,
                    record.surface,
                    record.revision as i64,
                    record.protocol_version as i64,
                    record.status,
                    record.expires_at,
                ],
            )
            .map_err(|_| "cannot store component document".to_string())?;
            db.execute(
                "UPDATE ui_component_states SET consumed_at=?1
                 WHERE document_id=?2 AND consumed_at IS NULL",
                params![now, record.document_id],
            )
            .map_err(|_| "cannot invalidate old component state".to_string())?;
            Ok(())
        })?;
        Ok(record)
    }

    pub fn create_state(&self, request: NewState) -> Result<String, String> {
        validate_state(&request)?;
        let token = uuid_hex();
        let now = now_secs();
        let expires_at = now + request.ttl_secs.clamp(60, 86_400) as f64;
        with_conn(self.path(), |db| {
            let document: Option<(String, i64, String, f64)> = db
                .query_row(
                    "SELECT owner_id,revision,status,expires_at FROM ui_component_documents WHERE document_id=?1",
                    params![request.document_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()
                .map_err(|_| "component document not found".to_string())?;
            let Some((_, _, status, document_expiry)) = document else {
                return Err("component document not found".to_string());
            };
            if status != "active" || document_expiry < now {
                return Err("component document is not active".to_string());
            }
            let encoded = serde_json::to_string(&request.state)
                .map_err(|_| "cannot encode component state".to_string())?;
            db.execute(
                "INSERT INTO ui_component_states
                 (token,document_id,logical_id,action,user_id,channel_id,state_json,one_shot,expires_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    token,
                    request.document_id,
                    request.logical_id,
                    request.action,
                    request.user_id,
                    request.channel_id,
                    encoded,
                    request.one_shot as i64,
                    expires_at,
                ],
            )
            .map_err(|_| "cannot store component state".to_string())?;
            Ok(())
        })?;
        Ok(token)
    }

    pub fn resolve_state(
        &self,
        token: &str,
        user_id: &str,
        channel_id: &str,
    ) -> Result<ResolvedState, String> {
        with_conn(self.path(), |db| {
            let row = db
                .query_row(
                    "SELECT s.token,s.document_id,s.logical_id,s.action,s.user_id,s.channel_id,
                            s.state_json,s.one_shot,s.expires_at,d.revision,d.status,d.expires_at
                     FROM ui_component_states s
                     JOIN ui_component_documents d ON d.document_id=s.document_id
                     WHERE s.token=?1",
                    params![token],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, i64>(7)? != 0,
                            row.get::<_, f64>(8)?,
                            row.get::<_, i64>(9)? as u32,
                            row.get::<_, String>(10)?,
                            row.get::<_, f64>(11)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| "component state is invalid".to_string())?;
            let Some((
                token,
                document_id,
                logical_id,
                action,
                stored_user,
                stored_channel,
                state_json,
                one_shot,
                expires_at,
                revision,
                document_status,
                document_expiry,
            )) = row
            else {
                return Err("component state is invalid or expired".to_string());
            };
            let now = now_secs();
            if expires_at < now
                || document_expiry < now
                || document_status != "active"
                || stored_user != user_id
                || stored_channel != channel_id
            {
                return Err("component state is invalid or expired".to_string());
            }
            Ok(ResolvedState {
                token,
                document_id,
                logical_id,
                action,
                user_id: stored_user,
                channel_id: stored_channel,
                state: serde_json::from_str(&state_json)
                    .map_err(|_| "component state is invalid".to_string())?,
                one_shot,
                expires_at,
                revision,
            })
        })
    }

    /// Resolve, consume (when one-shot), persist, and enqueue an interaction
    /// in one transaction. Replayed Discord interaction IDs return the same
    /// document receipt without creating another inbox row.
    pub fn accept_event(&self, request: NewEvent) -> Result<EventReceipt, String> {
        validate_event(&request)?;
        with_conn(self.path(), |db| {
            if let Some((document_id, conversation)) = db
                .query_row(
                    "SELECT document_id,conversation FROM ui_component_events WHERE interaction_id=?1",
                    params![request.interaction_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(|_| "cannot read component event".to_string())?
            {
                return Ok(EventReceipt {
                    interaction_id: request.interaction_id,
                    document_id,
                    queued: conversation.is_empty(),
                    duplicate: true,
                });
            }

            let row = db
                .query_row(
                    "SELECT s.document_id,s.logical_id,s.action,s.state_json,s.one_shot,s.expires_at,
                            d.owner_id,d.revision,d.status,d.expires_at
                     FROM ui_component_states s
                     JOIN ui_component_documents d ON d.document_id=s.document_id
                     WHERE s.token=?1 AND s.user_id=?2 AND s.channel_id=?3 AND s.consumed_at IS NULL",
                    params![request.token, request.user_id, request.channel_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)? != 0,
                            row.get::<_, f64>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, String>(8)?,
                            row.get::<_, f64>(9)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| "component state is invalid".to_string())?;
            let Some((
                document_id,
                _logical_id,
                _action,
                state_json,
                one_shot,
                state_expiry,
                _owner,
                _revision,
                status,
                document_expiry,
            )) = row
            else {
                return Err("component state is invalid or expired".to_string());
            };
            let now = now_secs();
            if state_expiry < now || document_expiry < now || status != "active" {
                return Err("component state is invalid or expired".to_string());
            }
            if one_shot && request.kind != "autocomplete" {
                db.execute(
                    "UPDATE ui_component_states SET consumed_at=?1 WHERE token=?2 AND consumed_at IS NULL",
                    params![now, request.token],
                )
                .map_err(|_| "cannot consume component state".to_string())?;
            }
            let payload = serde_json::to_string(&request.payload)
                .map_err(|_| "cannot encode component event".to_string())?;
            db.execute(
                "INSERT INTO ui_component_events
                 (interaction_id,document_id,token,kind,payload_json,conversation,state,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,'queued',?7)",
                params![
                    request.interaction_id,
                    document_id,
                    request.token,
                    request.kind,
                    payload,
                    request.conversation,
                    now
                ],
            )
            .map_err(|_| "cannot store component event".to_string())?;

            let event_values = request
                .payload
                .get("values")
                .cloned()
                .unwrap_or_else(|| request.payload.clone());
            let event_files = request
                .payload
                .get("files")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let envelope = serde_json::json!({
                "protocol": "gray.discord.input",
                "version": 1,
                "kind": "component_event",
                "payload": {
                    "document_id": document_id,
                    "interaction_id": request.interaction_id,
                    "component": _logical_id,
                    "action": _action,
                    "values": event_values,
                    "files": event_files,
                    "user_id": request.user_id,
                    "channel_id": request.channel_id,
                    "state": serde_json::from_str::<Value>(&state_json).unwrap_or(Value::Null),
                    "conversation": request.conversation,
                    "reply": {
                        "interaction_id": request.interaction_id,
                        "app_id": request.app_id
                    }
                }
            });
            let encoded = serde_json::to_string(&envelope)
                .map_err(|_| "cannot encode component input".to_string())?;
            let queued = enqueue_in_tx(
                db,
                crate::channels::discord::durable::EnqueueRequest {
                    id: &format!("component:{}", request.interaction_id),
                    channel: &request.channel_id,
                    conversation: &request.conversation,
                    prompt: "component_event",
                    input_json: Some(&encoded),
                    interaction_token: request.interaction_token.as_deref(),
                    app_id: request.app_id.as_deref(),
                    capacity: request.capacity,
                },
            )?;
            Ok(EventReceipt {
                interaction_id: request.interaction_id,
                document_id,
                queued,
                duplicate: false,
            })
        })
    }

    pub fn bind_message(&self, document_id: &str, message_id: &str) -> Result<(), String> {
        if document_id.trim().is_empty() || message_id.trim().is_empty() {
            return Err("document binding is invalid".to_string());
        }
        with_conn(self.path(), |db| {
            db.execute(
                "UPDATE ui_component_documents SET message_id=?1 WHERE document_id=?2 AND status='active'",
                params![message_id, document_id],
            )
            .map_err(|_| "cannot bind component message".to_string())?;
            Ok(())
        })
    }

    pub fn bind_modal(&self, document_id: &str, modal_id: &str) -> Result<(), String> {
        if document_id.trim().is_empty() || modal_id.trim().is_empty() {
            return Err("modal binding is invalid".to_string());
        }
        with_conn(self.path(), |db| {
            db.execute(
                "UPDATE ui_component_documents SET modal_id=?1 WHERE document_id=?2 AND status='active'",
                params![modal_id, document_id],
            )
            .map_err(|_| "cannot bind component modal".to_string())?;
            Ok(())
        })
    }

    pub fn invalidate_document(&self, document_id: &str) -> Result<(), String> {
        with_conn(self.path(), |db| {
            db.execute(
                "UPDATE ui_component_documents SET status='disabled' WHERE document_id=?1",
                params![document_id],
            )
            .map_err(|_| "cannot disable component document".to_string())?;
            db.execute(
                "UPDATE ui_component_states SET consumed_at=?1 WHERE document_id=?2 AND consumed_at IS NULL",
                params![now_secs(), document_id],
            )
            .map_err(|_| "cannot revoke component states".to_string())?;
            Ok(())
        })
    }

    pub fn prune_component_state(&self, retention_secs: u64) -> Result<usize, String> {
        let cutoff = now_secs() - retention_secs as f64;
        with_conn(self.path(), |db| {
            let mut count = 0;
            count += db.execute("DELETE FROM ui_component_events WHERE created_at < ?1 AND delivered_at IS NOT NULL", params![cutoff]).map_err(|_| "cannot prune component events".to_string())?;
            count += db
                .execute(
                    "DELETE FROM ui_component_states WHERE expires_at < ?1",
                    params![cutoff],
                )
                .map_err(|_| "cannot prune component states".to_string())?;
            count += db.execute("DELETE FROM ui_component_documents WHERE expires_at < ?1 AND status IN ('disabled','expired')", params![cutoff]).map_err(|_| "cannot prune component documents".to_string())?;
            Ok(count)
        })
    }
}

/// Allocates opaque interaction IDs backed by the UI state table. File
/// resolution is intentionally supplied by the media layer in Task 4.
pub struct StoreAllocator<'a> {
    pub store: &'a Store,
    pub files: Option<&'a FileStore>,
    pub document_id: String,
    pub user_id: String,
    pub channel_id: String,
    pub ttl_secs: u64,
    pub one_shot: bool,
    pub premium_enabled: bool,
    pub allowed_skus: Vec<String>,
    component_options: HashMap<String, Value>,
}

impl<'a> StoreAllocator<'a> {
    pub fn new(
        store: &'a Store,
        files: &'a FileStore,
        document_id: impl Into<String>,
        user_id: impl Into<String>,
        channel_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            files: Some(files),
            document_id: document_id.into(),
            user_id: user_id.into(),
            channel_id: channel_id.into(),
            ttl_secs: 3600,
            one_shot: true,
            premium_enabled: false,
            allowed_skus: Vec::new(),
            component_options: HashMap::new(),
        }
    }

    pub fn set_component_options(&mut self, logical_id: &str, options: Value) {
        self.component_options
            .insert(logical_id.to_string(), options);
    }
}

impl ComponentIdAllocator for StoreAllocator<'_> {
    fn allocate(&mut self, logical_id: &str, action: &str) -> Result<String, CompileError> {
        let mut state = serde_json::json!({"component": logical_id, "kind": action});
        if let Some(options) = self.component_options.get(logical_id) {
            state["options"] = options.clone();
        }
        self.store
            .create_state(NewState {
                document_id: self.document_id.clone(),
                logical_id: logical_id.to_string(),
                action: action.to_string(),
                user_id: self.user_id.clone(),
                channel_id: self.channel_id.clone(),
                state,
                one_shot: self.one_shot,
                ttl_secs: self.ttl_secs,
            })
            .map_err(|message| CompileError::new("state_unavailable", "$.components", message))
    }

    fn resolve_file(&self, file_id: &str) -> Result<FileRef, CompileError> {
        let Some(files) = self.files else {
            return Err(CompileError::new(
                "file_not_found",
                "$.file_id",
                "managed file is not available",
            ));
        };
        files
            .metadata(&self.user_id, file_id)
            .map(|metadata| FileRef {
                id: metadata.id,
                name: metadata.name,
                media_type: metadata.media_type,
                size: metadata.size,
                sha256: metadata.sha256,
            })
            .map_err(|message| CompileError::new("file_not_found", file_id, message))
    }

    fn premium_enabled(&self) -> bool {
        self.premium_enabled
    }

    fn sku_allowed(&self, sku_id: &str) -> bool {
        self.allowed_skus.iter().any(|sku| sku == sku_id)
    }
}

fn validate_document(request: &NewDocument) -> Result<(), String> {
    if request.document_id.trim().is_empty()
        || request.owner_id.trim().is_empty()
        || request.channel_id.trim().is_empty()
        || request.surface.trim().is_empty()
        || request.protocol_version != UI_VERSION
    {
        return Err("component document fields are invalid".to_string());
    }
    Ok(())
}

fn validate_state(request: &NewState) -> Result<(), String> {
    if request.document_id.trim().is_empty()
        || request.logical_id.trim().is_empty()
        || request.action.trim().is_empty()
        || request.user_id.trim().is_empty()
        || request.channel_id.trim().is_empty()
    {
        return Err("component state fields are invalid".to_string());
    }
    Ok(())
}

fn validate_event(request: &NewEvent) -> Result<(), String> {
    if request.interaction_id.trim().is_empty()
        || request.token.trim().is_empty()
        || request.user_id.trim().is_empty()
        || request.channel_id.trim().is_empty()
        || request.kind.trim().is_empty()
        || request.conversation.trim().is_empty()
    {
        return Err("component event fields are invalid".to_string());
    }
    Ok(())
}
