//! Safe Discord Components V2 rendering.
//!
//! Model output and command handlers never construct component JSON directly.
//! They describe a small semantic surface here, and [`validate_components`]
//! checks the resulting payload before it reaches the transport.

use serde_json::{Value, json};
use std::collections::HashSet;

use crate::channels::discord::component_compile::{
    CompileContext, DeterministicAllocator, compile_message,
};
use crate::channels::discord::component_protocol::{
    Container, Lifecycle, LogicalId, MessageDocument, MessageNode, Origin, TextDisplay, Visibility,
};

/// Discord's `IS_COMPONENTS_V2` message flag (1 << 15).
pub const IS_COMPONENTS_V2: u64 = 1 << 15;
/// Discord's `EPHEMERAL` message/interaction flag (1 << 6).
pub const EPHEMERAL: u64 = 1 << 6;
/// V2 messages have a total component budget, including nested components.
pub const MAX_COMPONENTS: usize = 40;
/// Interactive component identifiers are limited to 100 characters.
pub const MAX_CUSTOM_ID: usize = 100;

pub const INFO_ACCENT: u32 = 0x0058_65F2;
pub const DANGER_ACCENT: u32 = 0xED4245;
pub(crate) const QUIET_ACCENT: u32 = 0x0057_4F6E;
pub(crate) const MAX_DISPLAY_CHARS: usize = 3800;
const MAX_TEXT_DISPLAY_CHARS: usize = 4000;
const MAX_BUTTONS_PER_ROW: usize = 5;

/// A Text Display component.
pub fn text_display(content: impl Into<String>) -> Value {
    json!({
        "type": 10,
        "content": content.into(),
    })
}

/// An Action Row containing buttons or one select menu.
pub fn action_row(components: Vec<Value>) -> Value {
    json!({
        "type": 1,
        "components": components,
    })
}

/// A link-style button. Link buttons are not interactive and therefore do
/// not carry a custom ID.
pub fn link_button(label: impl Into<String>, url: impl Into<String>) -> Value {
    json!({
        "type": 2,
        "label": label.into(),
        "style": 5,
        "url": url.into(),
    })
}

/// A Separator component.
pub fn separator() -> Value {
    json!({
        "type": 14,
        "divider": true,
        "spacing": 1,
    })
}

/// A Container component with an optional accent color.
pub fn container(components: Vec<Value>, accent: Option<u32>) -> Value {
    let mut out = json!({
        "type": 17,
        "components": components,
    });
    if let Some(accent) = accent {
        out["accent_color"] = json!(accent);
    }
    out
}

/// Render ordinary model output as one safe V2 Text Display.
///
/// Empty output is rejected rather than producing an invisible message.
pub fn text_message(text: &str) -> Result<Vec<Value>, String> {
    let text = crate::channels::discord::text::sanitize(text);
    if text.trim().is_empty() {
        return Err("Text display must not be empty".to_string());
    }
    let document = MessageDocument {
        version: 1,
        document_id: LogicalId::new("gray-trusted-text"),
        visibility: Visibility::Public,
        lifecycle: Lifecycle::Static,
        components: vec![MessageNode::Text(TextDisplay {
            content: bounded(&text, MAX_DISPLAY_CHARS),
        })],
    };
    let mut allocator = DeterministicAllocator::default();
    let mut context = CompileContext::new(Origin::Plugin, &mut allocator);
    compile_message(&document, &mut context)
        .map(|compiled| compiled.components)
        .map_err(|error| error.message)
}

/// Render a structured Gray card.
fn card_legacy(
    title: &str,
    body: &str,
    accent: u32,
    actions: &[Value],
) -> Result<Vec<Value>, String> {
    let mut children = Vec::new();
    if !title.trim().is_empty() {
        children.push(text_display(format!(
            "## {}",
            bounded(&crate::channels::discord::text::sanitize(title), 256)
        )));
    }
    if !body.trim().is_empty() {
        children.push(text_display(bounded(
            &crate::channels::discord::text::sanitize(body),
            MAX_DISPLAY_CHARS,
        )));
    }
    if children.is_empty() {
        return Err("Card must contain a title or body".to_string());
    }
    for row in actions.chunks(MAX_BUTTONS_PER_ROW) {
        if !row.is_empty() {
            children.push(action_row(row.to_vec()));
        }
    }
    if children.len() + 1 > MAX_COMPONENTS {
        return Err("Card exceeds Discord's component budget".to_string());
    }
    Ok(vec![container(children, Some(accent))])
}

/// Render a typed card. Legacy interactive command buttons stay on the
/// compatibility adapter until their durable state contract is migrated.
pub fn card(title: &str, body: &str, accent: u32, actions: &[Value]) -> Result<Vec<Value>, String> {
    if !actions.is_empty() {
        return card_legacy(title, body, accent, actions);
    }
    let mut children = Vec::new();
    if !title.trim().is_empty() {
        children.push(MessageNode::Text(TextDisplay {
            content: format!(
                "## {}",
                bounded(&crate::channels::discord::text::sanitize(title), 256)
            ),
        }));
    }
    if !body.trim().is_empty() {
        children.push(MessageNode::Text(TextDisplay {
            content: bounded(
                &crate::channels::discord::text::sanitize(body),
                MAX_DISPLAY_CHARS,
            ),
        }));
    }
    if children.is_empty() {
        return Err("Card must contain a title or body".to_string());
    }
    let document = MessageDocument {
        version: 1,
        document_id: LogicalId::new("gray-trusted-card"),
        visibility: Visibility::Public,
        lifecycle: Lifecycle::Static,
        components: vec![MessageNode::Container(Container {
            accent_color: Some(accent),
            spoiler: false,
            components: children,
        })],
    };
    let mut allocator = DeterministicAllocator::default();
    let mut context = CompileContext::new(Origin::Plugin, &mut allocator);
    compile_message(&document, &mut context)
        .map(|compiled| compiled.components)
        .map_err(|error| error.message)
}

/// Render the opt-in end-of-turn tally card (`activity_card`). Live
/// narration and replies are plain Text Displays (see [`crate::channels::discord::stream`]).
pub fn tool_card(text: &str) -> Result<Vec<Value>, String> {
    card("gray · done", text, QUIET_ACCENT, &[])
}

/// Render an error card.
pub fn error_card(text: &str) -> Result<Vec<Value>, String> {
    card("Error", text, DANGER_ACCENT, &[])
}

/// Convert one of the plugin's existing, trusted command embeds into V2.
///
/// This is deliberately a narrow adapter rather than a general JSON-to-JSON
/// converter: only title, description, fields, footer, color, and supplied
/// buttons are read. It keeps command rendering and Discord wire validation in
/// separate layers while retiring embeds from the wire.
pub fn from_embed(embed: &Value, buttons: &[Value]) -> Result<Vec<Value>, String> {
    let (title, body, accent) = merged(embed);
    let components = card(&title, &body, accent, buttons)?;
    validate_components(&components)?;
    Ok(components)
}

pub fn merged(embed: &Value) -> (String, String, u32) {
    let mut title = embed
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let description = embed
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let accent = embed
        .get("color")
        .and_then(Value::as_u64)
        .unwrap_or(INFO_ACCENT as u64)
        .min(0xFF_FFFF) as u32;

    let mut body = String::new();
    if title.chars().count() > 256 {
        body.push_str(title.trim());
        title = String::new();
    }
    if !description.trim().is_empty() {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&description);
    }
    if let Some(fields) = embed.get("fields").and_then(Value::as_array) {
        for field in fields {
            let name = field
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let value = field
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if name.is_empty() && value.is_empty() {
                continue;
            }
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            if !name.is_empty() {
                body.push_str(&crate::channels::discord::text::sanitize(name));
            }
            if !name.is_empty() && !value.is_empty() {
                body.push('\n');
            }
            if !value.is_empty() {
                body.push_str(&crate::channels::discord::text::sanitize(value));
            }
        }
    }
    if let Some(footer) = embed
        .get("footer")
        .and_then(|f| f.get("text"))
        .and_then(Value::as_str)
        && !footer.trim().is_empty()
    {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&crate::channels::discord::text::sanitize(footer));
    }
    (title, body, accent)
}

/// Validate a V2 component tree before transmission.
pub fn validate_components(components: &[Value]) -> Result<(), String> {
    if components.is_empty() {
        return Err("V2 message must contain at least one component".to_string());
    }
    let mut state = ValidationState {
        count: 0,
        custom_ids: HashSet::new(),
    };
    for component in components {
        validate_component(component, true, &mut state)?;
    }
    if state.count > MAX_COMPONENTS {
        return Err(format!(
            "V2 message has {} components; Discord allows {MAX_COMPONENTS}",
            state.count
        ));
    }
    Ok(())
}

struct ValidationState {
    count: usize,
    custom_ids: HashSet<String>,
}

fn validate_component(
    component: &Value,
    top_level: bool,
    state: &mut ValidationState,
) -> Result<(), String> {
    state.count += 1;
    if state.count > MAX_COMPONENTS {
        return Err(format!(
            "V2 message has more than {MAX_COMPONENTS} components"
        ));
    }
    let kind = component
        .get("type")
        .and_then(Value::as_u64)
        .ok_or_else(|| "component is missing an integer type".to_string())?;
    match kind {
        1 => {
            let children = array(component, "components")?;
            if children.is_empty() || children.len() > MAX_BUTTONS_PER_ROW {
                return Err("Action Row must contain 1–5 components".to_string());
            }
            let kinds: Vec<u64> = children
                .iter()
                .map(|child| {
                    child
                        .get("type")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| "Action Row child is missing a type".to_string())
                })
                .collect::<Result<_, _>>()?;
            if kinds
                .iter()
                .any(|kind| !matches!(kind, 2 | 3 | 5 | 6 | 7 | 8))
            {
                return Err("Action Row contains a non-interactive component".to_string());
            }
            if kinds.len() > 1 && kinds.iter().any(|kind| *kind != 2) {
                return Err("Action Row may contain one select or up to five buttons".to_string());
            }
            for child in children {
                validate_component(child, false, state)?;
            }
        }
        2 => {
            if top_level {
                return Err("Button must be inside an Action Row or Section accessory".to_string());
            }
            let style = component
                .get("style")
                .and_then(Value::as_u64)
                .ok_or_else(|| "Button is missing a style".to_string())?;
            let label = component.get("label").and_then(Value::as_str);
            if let Some(label) = label
                && label.chars().count() > 80
            {
                return Err("Button label must contain at most 80 characters".to_string());
            }
            match style {
                1..=4 => {
                    validate_custom_id(component, state)?;
                    if component.get("url").is_some() || component.get("sku_id").is_some() {
                        return Err("Action buttons cannot carry url or sku_id".to_string());
                    }
                    if label.is_none_or(str::is_empty) && component.get("emoji").is_none() {
                        return Err("Action buttons need a label or emoji".to_string());
                    }
                }
                5 => {
                    let url = component
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "Link button requires a URL".to_string())?;
                    if url.is_empty() || url.chars().count() > 512 {
                        return Err("Link button URL must contain 1–512 characters".to_string());
                    }
                    if component.get("custom_id").is_some() || component.get("sku_id").is_some() {
                        return Err("Link buttons cannot carry custom_id or sku_id".to_string());
                    }
                }
                6 => return Err("Premium buttons are not supported by this bridge".to_string()),
                _ => return Err("Button style must be 1–5".to_string()),
            }
        }
        3 | 5 | 6 | 7 | 8 => {
            if top_level {
                return Err("Select menu cannot be a top-level V2 component".to_string());
            }
            validate_custom_id(component, state)?;
            if kind == 3 {
                let options = array(component, "options")?;
                if options.is_empty() || options.len() > 25 {
                    return Err("String select must contain 1–25 options".to_string());
                }
            }
        }
        9 => {
            let children = array(component, "components")?;
            if children.is_empty() {
                return Err("Section must contain at least one Text Display".to_string());
            }
            for child in children {
                if child.get("type").and_then(Value::as_u64) != Some(10) {
                    return Err("Section children must be Text Displays".to_string());
                }
                validate_component(child, false, state)?;
            }
            let accessory = component
                .get("accessory")
                .ok_or_else(|| "Section requires an accessory".to_string())?;
            let accessory_kind = accessory
                .get("type")
                .and_then(Value::as_u64)
                .ok_or_else(|| "Section accessory is missing a type".to_string())?;
            if !matches!(accessory_kind, 2 | 11) {
                return Err("Section accessory must be a Button or Thumbnail".to_string());
            }
            validate_component(accessory, false, state)?;
        }
        10 => {
            let content = component
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "Text Display is missing content".to_string())?;
            if content.trim().is_empty() {
                return Err("Text Display content must not be empty".to_string());
            }
            if content.chars().count() > MAX_TEXT_DISPLAY_CHARS {
                return Err(format!(
                    "Text Display content must contain at most {MAX_TEXT_DISPLAY_CHARS} characters"
                ));
            }
        }
        11 => {
            if top_level {
                return Err("Thumbnail must be used as a Section accessory".to_string());
            }
            validate_media_source(component.get("media"), "Thumbnail")?;
            if component
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|value| value.chars().count() > 1024)
            {
                return Err(
                    "Thumbnail description must contain at most 1024 characters".to_string()
                );
            }
        }
        12 => {
            let items = array(component, "items")?;
            if items.is_empty() || items.len() > 10 {
                return Err("Media Gallery must contain 1–10 items".to_string());
            }
            for item in items {
                validate_media_source(item.get("media"), "Media Gallery item")?;
            }
        }
        13 => {
            let file = component
                .get("file")
                .ok_or_else(|| "File requires a file object".to_string())?;
            if file
                .get("url")
                .and_then(Value::as_str)
                .is_none_or(|value| !value.starts_with("attachment://"))
            {
                return Err("File media must use an attachment:// URL".to_string());
            }
        }
        14 => {
            if let Some(spacing) = component.get("spacing")
                && !matches!(spacing.as_u64(), Some(1) | Some(2))
            {
                return Err("Separator spacing must be 1 or 2".to_string());
            }
            if component
                .get("divider")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err("Separator divider must be boolean".to_string());
            }
        }
        17 => {
            if component
                .get("accent_color")
                .is_some_and(|value| value.as_u64().is_none_or(|color| color > 0xFF_FFFF))
            {
                return Err("Container accent_color must be between 0 and 0xFFFFFF".to_string());
            }
            let children = array(component, "components")?;
            if children.is_empty() {
                return Err("Container must contain at least one component".to_string());
            }
            for child in children {
                let child_kind = child
                    .get("type")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "Container child is missing a type".to_string())?;
                if !matches!(child_kind, 1 | 9 | 10 | 12 | 13 | 14) {
                    return Err(format!(
                        "Container contains unsupported component type {child_kind}"
                    ));
                }
                validate_component(child, false, state)?;
            }
        }
        other => return Err(format!("Unsupported V2 component type {other}")),
    }
    Ok(())
}

fn validate_media_source(media: Option<&Value>, label: &str) -> Result<(), String> {
    let media = media.ok_or_else(|| format!("{label} requires a media object"))?;
    let has_url = media
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let has_attachment = media
        .get("attachment")
        .and_then(Value::as_str)
        .is_some_and(|value| value.starts_with("attachment://"));
    if has_url || has_attachment {
        Ok(())
    } else {
        Err(format!(
            "{label} media requires a URL or attachment reference"
        ))
    }
}

fn validate_custom_id(component: &Value, state: &mut ValidationState) -> Result<(), String> {
    let id = component
        .get("custom_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "Interactive component is missing custom_id".to_string())?;
    if id.is_empty() || id.chars().count() > MAX_CUSTOM_ID {
        return Err(format!(
            "custom_id must contain 1–{MAX_CUSTOM_ID} characters"
        ));
    }
    if !state.custom_ids.insert(id.to_string()) {
        return Err("custom_id values must be unique within a message".to_string());
    }
    Ok(())
}

fn array<'a>(component: &'a Value, name: &str) -> Result<&'a [Value], String> {
    component
        .get(name)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("component is missing {name} array"))
}

fn bounded(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(24)).collect();
    format!("{head}\n… (truncated)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_is_a_v2_text_display() {
        let components = text_message("hello <@123>").unwrap();
        assert_eq!(components[0]["type"], 10);
        assert_eq!(components[0]["content"], "hello <@123>");
        validate_components(&components).unwrap();
    }

    #[test]
    fn empty_text_is_refused() {
        assert!(text_message("  \n").is_err());
    }

    #[test]
    fn cards_are_nested_and_bounded() {
        let button = json!({
            "type": 2,
            "label": "Stop",
            "style": 4,
            "custom_id": "gray:stop:1"
        });
        let components = card("Status", "working", INFO_ACCENT, &[button]).unwrap();
        assert_eq!(components[0]["type"], 17);
        assert_eq!(components[0]["components"][2]["type"], 1);
        validate_components(&components).unwrap();
    }

    #[test]
    fn embed_adapter_reads_only_the_known_shape() {
        let embed = json!({
            "title": "Cron",
            "description": "one job",
            "color": 0xED4245,
            "fields": [{"name": "Next", "value": "soon"}],
            "footer": {"text": "gray-discord"},
            "unexpected": {"type": 2, "custom_id": "must-not-pass-through"}
        });
        let components = from_embed(&embed, &[]).unwrap();
        validate_components(&components).unwrap();
        let serialized = serde_json::to_string(&components).unwrap();
        assert!(!serialized.contains("must-not-pass-through"));
        assert!(serialized.contains("Next"));
    }

    #[test]
    fn validator_counts_nested_components_and_rejects_excess() {
        let mut children = Vec::new();
        for i in 0..40 {
            children.push(text_display(format!("line {i}")));
        }
        let components = vec![container(children, None)];
        assert!(validate_components(&components).is_err());
    }

    #[test]
    fn action_rows_and_link_buttons_follow_v2_rules() {
        let row = action_row(vec![link_button("Docs", "https://example.com")]);
        validate_components(&[row]).unwrap();
        let bad = action_row(vec![
            link_button("Docs", "https://example.com"),
            json!({
                "type": 3,
                "custom_id": "choose",
                "options": [{"label": "One", "value": "1"}]
            }),
        ]);
        assert!(validate_components(&[bad]).is_err());
    }

    #[test]
    fn validator_rejects_duplicate_custom_ids() {
        let button = json!({
            "type": 2,
            "label": "A",
            "style": 1,
            "custom_id": "same"
        });
        let components = card("x", "y", INFO_ACCENT, &[button.clone(), button]).unwrap();
        assert!(validate_components(&components).is_err());
    }
}
