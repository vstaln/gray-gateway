//! Normalize Discord interaction payloads into bounded Gray component events.
//! Discord tokens and callback tokens remain inside the Gateway/Store boundary.

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub enum NormalizedInteraction {
    Button(InteractionValues),
    Select(InteractionValues),
    ModalSubmit(InteractionValues),
    Autocomplete(InteractionValues),
}

#[derive(Debug, Clone, PartialEq)]
pub struct IncomingAttachment {
    pub id: String,
    pub url: String,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InteractionValues {
    pub interaction_id: String,
    pub user_id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub message_id: Option<String>,
    pub modal_id: Option<String>,
    pub state_token: String,
    pub values: Value,
    pub files: Vec<String>,
    pub attachments: Vec<IncomingAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionError {
    pub code: &'static str,
    pub message: &'static str,
}

impl InteractionError {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }
}

impl std::fmt::Display for InteractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for InteractionError {}

impl NormalizedInteraction {
    pub fn values(&self) -> &InteractionValues {
        match self {
            Self::Button(values)
            | Self::Select(values)
            | Self::ModalSubmit(values)
            | Self::Autocomplete(values) => values,
        }
    }

    pub fn values_mut(&mut self) -> &mut InteractionValues {
        match self {
            Self::Button(values)
            | Self::Select(values)
            | Self::ModalSubmit(values)
            | Self::Autocomplete(values) => values,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Button(_) => "button",
            Self::Select(_) => "select",
            Self::ModalSubmit(_) => "modal_submit",
            Self::Autocomplete(_) => "autocomplete",
        }
    }

    /// Build the core envelope only after Store resolution has supplied the
    /// logical component ID. The Discord state token is never included.
    pub fn gray_envelope(
        &self,
        conversation: &str,
        document_id: &str,
        logical_id: &str,
        action: &str,
    ) -> Value {
        let values = self.values();
        json!({
            "protocol": "gray.discord.input",
            "version": 1,
            "kind": "component_event",
            "payload": {
                "conversation": conversation,
                "document_id": document_id,
                "interaction_id": values.interaction_id,
                "component": logical_id,
                "action": action,
                "values": values.values,
                "files": values.files,
                "user_id": values.user_id,
                "channel_id": values.channel_id,
                "guild_id": values.guild_id,
                "message_id": values.message_id,
                "modal_id": values.modal_id
            }
        })
    }
}

pub fn normalize(value: &Value) -> Result<NormalizedInteraction, InteractionError> {
    let interaction_id = string_at(value, "/id")
        .ok_or_else(|| InteractionError::new("invalid_interaction", "interaction id is missing"))?;
    let user_id = value
        .pointer("/user/id")
        .and_then(Value::as_str)
        .ok_or_else(|| InteractionError::new("invalid_interaction", "interaction user is missing"))?
        .to_string();
    let channel_id = string_at(value, "/channel_id")
        .ok_or_else(|| {
            InteractionError::new("invalid_interaction", "interaction channel is missing")
        })?
        .to_string();
    let guild_id = value
        .get("guild_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let message_id = value
        .pointer("/message/id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let data = value.get("data").ok_or_else(|| {
        InteractionError::new("invalid_interaction", "interaction data is missing")
    })?;
    let state_token = data
        .get("custom_id")
        .and_then(Value::as_str)
        .ok_or_else(|| InteractionError::new("invalid_interaction", "component state is missing"))?
        .to_string();
    if state_token.is_empty() || state_token.len() > 200 {
        return Err(InteractionError::new(
            "invalid_interaction",
            "component state is invalid",
        ));
    }
    let files = collect_files(value)?;
    let attachments = collect_attachments(value)?;
    if attachments.len() > 10 {
        return Err(InteractionError::new(
            "invalid_files",
            "interaction has too many files",
        ));
    }

    let component_type = data.get("component_type").and_then(Value::as_u64);
    let normalized = if data.get("components").is_some() {
        NormalizedInteraction::ModalSubmit(InteractionValues {
            interaction_id,
            user_id,
            channel_id,
            guild_id,
            message_id,
            modal_id: value
                .get("modal")
                .and_then(|modal| modal.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            state_token,
            values: parse_modal_components(
                data.get("components")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        InteractionError::new("invalid_modal", "modal components are invalid")
                    })?,
            )?,
            files,
            attachments,
        })
    } else {
        match component_type {
            Some(2) => NormalizedInteraction::Button(InteractionValues {
                interaction_id,
                user_id,
                channel_id,
                guild_id,
                message_id,
                modal_id: None,
                state_token,
                values: json!({}),
                files,
                attachments,
            }),
            Some(3 | 5 | 6 | 7 | 8) => NormalizedInteraction::Select(InteractionValues {
                interaction_id,
                user_id,
                channel_id,
                guild_id,
                message_id,
                modal_id: None,
                state_token,
                values: json!({"values": bounded_array(data.get("values"), 25)?}),
                files,
                attachments,
            }),
            _ => {
                return Err(InteractionError::new(
                    "unsupported_interaction",
                    "component interaction type is unsupported",
                ));
            }
        }
    };
    Ok(normalized)
}

pub fn normalize_autocomplete(value: &Value) -> Result<InteractionValues, InteractionError> {
    let interaction_id = string_at(value, "/id")
        .ok_or_else(|| InteractionError::new("invalid_interaction", "interaction id is missing"))?;
    let user_id = value
        .pointer("/user/id")
        .and_then(Value::as_str)
        .ok_or_else(|| InteractionError::new("invalid_interaction", "interaction user is missing"))?
        .to_string();
    let channel_id = string_at(value, "/channel_id")
        .unwrap_or_default()
        .to_string();
    let state_token = value
        .pointer("/data/custom_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            InteractionError::new("invalid_interaction", "autocomplete state is missing")
        })?
        .to_string();
    let focused = value
        .pointer("/data/focused")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(InteractionValues {
        interaction_id,
        user_id,
        channel_id,
        guild_id: value
            .get("guild_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        message_id: value
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map(str::to_string),
        modal_id: None,
        state_token,
        values: json!({
            "query": value.pointer("/data/query").and_then(Value::as_str).unwrap_or(""),
            "focused": focused,
            "options": bounded_array(value.pointer("/data/options"), 25)?
        }),
        files: Vec::new(),
        attachments: Vec::new(),
    })
}

fn parse_modal_components(components: &[Value]) -> Result<Value, InteractionError> {
    let mut values = serde_json::Map::new();
    for component in components.iter().take(40) {
        let Some(kind) = component.get("type").and_then(Value::as_u64) else {
            return Err(InteractionError::new(
                "invalid_modal",
                "modal component type is missing",
            ));
        };
        let Some(custom_id) = component.get("custom_id").and_then(Value::as_str) else {
            return Err(InteractionError::new(
                "invalid_modal",
                "modal component id is missing",
            ));
        };
        if custom_id.is_empty() || custom_id.len() > 200 {
            return Err(InteractionError::new(
                "invalid_modal",
                "modal component id is invalid",
            ));
        }
        let value = match kind {
            4 => {
                json!({"type": "text", "value": component.get("value").and_then(Value::as_str).unwrap_or("")})
            }
            3 | 5 | 6 | 7 | 8 => {
                json!({"type": "select", "values": bounded_array(component.get("values"), 25)?})
            }
            21 | 22 => {
                json!({"type": if kind == 21 { "radio" } else { "checkbox_group" }, "values": bounded_array(component.get("values"), 10)?})
            }
            23 => {
                json!({"type": "checkbox", "checked": component.get("checked").and_then(Value::as_bool).unwrap_or(false)})
            }
            19 => {
                let values = match component.get("values") {
                    None => Vec::new(),
                    Some(value) => bounded_array(Some(value), 10)?,
                };
                json!({"type": "file_upload", "values": values})
            }
            _ => {
                return Err(InteractionError::new(
                    "invalid_modal",
                    "modal component type is unsupported",
                ));
            }
        };
        values.insert(custom_id.to_string(), value);
    }
    Ok(Value::Object(values))
}

fn collect_files(value: &Value) -> Result<Vec<String>, InteractionError> {
    let mut files = Vec::new();
    if let Some(attachments) = value.get("attachments").and_then(Value::as_array) {
        for attachment in attachments {
            if let Some(id) = attachment.get("id").and_then(Value::as_str) {
                if id.is_empty() || id.len() > 100 {
                    return Err(InteractionError::new(
                        "invalid_files",
                        "attachment id is invalid",
                    ));
                }
                files.push(id.to_string());
            }
        }
    }
    if let Some(components) = value.pointer("/data/components").and_then(Value::as_array) {
        for component in components {
            if let Some(attachments) = component.get("attachments").and_then(Value::as_array) {
                for attachment in attachments {
                    if let Some(id) = attachment.get("id").and_then(Value::as_str)
                        && id.len() <= 100
                    {
                        files.push(id.to_string());
                    }
                }
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_attachments(value: &Value) -> Result<Vec<IncomingAttachment>, InteractionError> {
    let mut attachments = Vec::new();
    collect_attachments_into(value, &mut attachments)?;
    attachments.sort_by(|a, b| a.id.cmp(&b.id));
    attachments.dedup_by(|a, b| a.id == b.id);
    Ok(attachments)
}

fn collect_attachments_into(
    value: &Value,
    attachments: &mut Vec<IncomingAttachment>,
) -> Result<(), InteractionError> {
    if let Some(items) = value.get("attachments").and_then(Value::as_array) {
        for item in items {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            let url = item.get("url").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() || id.len() > 100 || url.is_empty() || url.len() > 2048 {
                return Err(InteractionError::new(
                    "invalid_files",
                    "attachment metadata is invalid",
                ));
            }
            let name = item
                .get("filename")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("attachment");
            let size = item.get("size").and_then(Value::as_u64).unwrap_or(0);
            attachments.push(IncomingAttachment {
                id: id.to_string(),
                url: url.to_string(),
                name: name.to_string(),
                size,
            });
        }
    }
    if let Some(data) = value.get("data") {
        collect_attachments_into(data, attachments)?;
    }
    if let Some(items) = value.get("components").and_then(Value::as_array) {
        for item in items {
            collect_attachments_into(item, attachments)?;
        }
    }
    Ok(())
}

fn bounded_array(value: Option<&Value>, max: usize) -> Result<Vec<Value>, InteractionError> {
    let array = value
        .and_then(Value::as_array)
        .ok_or_else(|| InteractionError::new("invalid_values", "interaction values are invalid"))?;
    if array.len() > max || array.iter().any(|value| value.to_string().len() > 2000) {
        return Err(InteractionError::new(
            "invalid_values",
            "interaction values exceed the limit",
        ));
    }
    Ok(array.clone())
}

fn string_at(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_button_select_and_modal_without_exposing_state_in_envelope() {
        let base = |data: Value| {
            json!({
                "id": "interaction-1", "channel_id": "channel-1", "guild_id": "guild-1",
                "user": {"id": "user-1"}, "data": data
            })
        };
        let button = normalize(&base(
            json!({"component_type": 2, "custom_id": "opaque-state"}),
        ))
        .unwrap();
        let select = normalize(&base(
            json!({"component_type": 3, "custom_id": "opaque-state", "values": ["a"]}),
        ))
        .unwrap();
        let modal = normalize(&base(json!({"custom_id": "opaque-state", "components": [
            {"type": 4, "custom_id": "text", "value": "hello"},
            {"type": 3, "custom_id": "select", "values": ["a"]},
            {"type": 23, "custom_id": "check", "checked": true}
        ]})))
        .unwrap();
        assert_eq!(button.kind(), "button");
        assert_eq!(select.kind(), "select");
        assert_eq!(modal.kind(), "modal_submit");
        let envelope = button.gray_envelope("chat:channel-1", "doc", "run", "button");
        assert!(!envelope.to_string().contains("opaque-state"));
        assert_eq!(envelope["payload"]["component"], "run");
    }

    #[test]
    fn rejects_unknown_type_and_oversized_values() {
        let value = json!({"id":"i","channel_id":"c","user":{"id":"u"},"data":{"component_type":99,"custom_id":"s"}});
        assert_eq!(
            normalize(&value).unwrap_err().code,
            "unsupported_interaction"
        );
        let value = json!({"id":"i","channel_id":"c","user":{"id":"u"},"data":{"component_type":3,"custom_id":"s","values": (0..26).map(|n| n.to_string()).collect::<Vec<_>>()}});
        assert_eq!(normalize(&value).unwrap_err().code, "invalid_values");
    }
}
