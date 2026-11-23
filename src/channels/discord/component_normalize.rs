//! Lenient front door for agent-authored UI documents.
//!
//! Models write Components V2 in many dialects: Discord's raw numeric JSON,
//! `text_display` instead of `text`, bare URLs for media, a list of components
//! with no envelope, a JSON string instead of an object. This module rewrites
//! those dialects into the canonical `component_protocol` shape before the
//! strict serde types see it, and turns any remaining failure into an error
//! that names the exact path so the caller can fix it in one retry.
//!
//! Nothing here widens what can reach Discord: `custom_id` values are still
//! compiler-owned (an incoming `custom_id` only becomes a logical id), file
//! media still resolves through the private store, and the compiler still
//! validates every limit.

use serde_json::{Map, Value, json};

use crate::channels::discord::component_protocol::{MessageNode, ModalNode, Surface, UiDocument};

/// Coerce `raw` into a canonical document object for `surface`.
pub fn coerce_document(raw: &Value, surface: Surface) -> Result<Value, String> {
    let mut value = raw.clone();
    let mut map = loop {
        value = match value {
            Value::String(text) => serde_json::from_str(text.trim()).map_err(|_| {
                "document must be a JSON object, not a string that fails to parse as JSON"
                    .to_string()
            })?,
            Value::Object(mut map) if map.len() == 1 && map.contains_key("document") => {
                map.remove("document").unwrap_or_default()
            }
            Value::Array(items) => from_array(items)?,
            Value::Object(map) => break map,
            other => {
                return Err(format!(
                    "document must be a JSON object, got {}",
                    kind(&other)
                ));
            }
        };
    };

    // Header defaults: version, id, surface.
    if !map.contains_key("surface") {
        let hinted = map
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| matches!(*kind, "message" | "modal"))
            .map(str::to_string);
        if hinted.is_some() {
            map.remove("type");
        }
        let name = hinted.unwrap_or_else(|| surface_name(surface).to_string());
        map.insert("surface".into(), json!(name));
    }
    map.entry("version").or_insert(json!(1));
    if !map.get("document_id").is_some_and(Value::is_string) {
        let id = map
            .remove("id")
            .and_then(|id| id.as_str().map(str::to_string))
            .unwrap_or_else(|| {
                format!(
                    "ui-{}",
                    &crate::channels::discord::durable::uuid_hex()[..12]
                )
            });
        map.insert("document_id".into(), json!(id));
    }
    if map.remove("ephemeral").and_then(|v| v.as_bool()) == Some(true) {
        map.insert("visibility".into(), json!("ephemeral"));
    }
    map.remove("flags");

    let components = match map.remove("components") {
        Some(Value::Array(items)) => items,
        Some(Value::Object(item)) => vec![Value::Object(item)],
        Some(Value::String(text)) => vec![json!({"type": "text", "content": text})],
        Some(Value::Null) | None => Vec::new(),
        Some(other) => return Err(format!("components must be a list, got {}", kind(&other))),
    };

    let is_modal = map.get("surface").and_then(Value::as_str) == Some("modal");
    let components = if is_modal {
        normalize_modal_list(components)
    } else {
        let mut prefix = Vec::new();
        if let Some(title) = map
            .remove("title")
            .and_then(|t| t.as_str().map(str::to_string))
        {
            prefix.push(json!({"type": "text", "content": format!("## {title}")}));
        }
        if let Some(content) = map
            .remove("content")
            .and_then(|t| t.as_str().map(str::to_string))
        {
            if !content.trim().is_empty() {
                prefix.push(json!({"type": "text", "content": content}));
            }
        }
        prefix.extend(components);
        let mut nodes = normalize_message_list(prefix);
        // A top-level accent colour means "put it all in one card".
        let accent = map
            .remove("accent_color")
            .or_else(|| map.remove("color"))
            .and_then(|c| color(&c));
        if let Some(accent) = accent {
            let all_layout = nodes
                .iter()
                .all(|node| !matches!(node.get("type").and_then(Value::as_str), Some("container")));
            if all_layout && !nodes.is_empty() {
                nodes =
                    vec![json!({"type": "container", "accent_color": accent, "components": nodes})];
            }
        }
        nodes
    };
    if components.is_empty() {
        return Err("document has no components; add at least one, e.g. {\"type\":\"text\",\"content\":\"hello\"}".to_string());
    }
    map.insert("components".into(), Value::Array(components));
    Ok(Value::Object(map))
}

/// Deserialize a coerced document and, on failure, walk the tree to report
/// the deepest component that does not parse.
pub fn parse_document(value: &Value) -> Result<UiDocument, String> {
    match serde_json::from_value::<UiDocument>(value.clone()) {
        Ok(document) => Ok(document),
        Err(error) => {
            let surface = value
                .get("surface")
                .and_then(Value::as_str)
                .unwrap_or("message");
            let located = value
                .get("components")
                .and_then(Value::as_array)
                .and_then(|items| locate(items, "$.components", surface == "modal"));
            Err(located.unwrap_or_else(|| format!("$: {error}")))
        }
    }
}

fn locate(items: &[Value], path: &str, modal: bool) -> Option<String> {
    for (index, item) in items.iter().enumerate() {
        let here = format!("{path}[{index}]");
        let ok = if modal {
            serde_json::from_value::<ModalNode>(item.clone()).is_ok()
        } else {
            serde_json::from_value::<MessageNode>(item.clone()).is_ok()
        };
        if ok {
            continue;
        }
        for key in ["components", "children"] {
            if let Some(children) = item.get(key).and_then(Value::as_array) {
                if let Some(deeper) = locate(children, &format!("{here}.{key}"), modal) {
                    return Some(deeper);
                }
            }
        }
        let error = if modal {
            serde_json::from_value::<ModalNode>(item.clone()).err()
        } else {
            serde_json::from_value::<MessageNode>(item.clone()).err()
        };
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("?");
        return Some(format!(
            "{here} (type {kind}): {}",
            error.map(|e| e.to_string()).unwrap_or_default()
        ));
    }
    None
}

fn from_array(items: Vec<Value>) -> Result<Value, String> {
    let looks_like_document =
        |item: &Value| item.get("components").is_some() || item.get("document_id").is_some();
    match items.as_slice() {
        [] => Err("document is an empty list".to_string()),
        [only] if looks_like_document(only) => Ok(only.clone()),
        _ if items.iter().any(looks_like_document) => {
            Err("send one document per call, not a list of documents".to_string())
        }
        // A bare list of components: wrap it.
        _ => Ok(json!({"components": items})),
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn surface_name(surface: Surface) -> &'static str {
    match surface {
        Surface::Message => "message",
        Surface::Modal => "modal",
    }
}

/// Canonical message component name for a type tag (name or Discord number).
fn message_type(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(number) = value.as_u64() {
        return Some(
            match number {
                1 => "action_row",
                2 => "button",
                3 => "string_select",
                5 => "user_select",
                6 => "role_select",
                7 => "mentionable_select",
                8 => "channel_select",
                9 => "section",
                10 => "text",
                11 => "thumbnail",
                12 => "media_gallery",
                13 => "file",
                14 => "separator",
                17 => "container",
                _ => return None,
            }
            .to_string(),
        );
    }
    let name = value
        .as_str()?
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_");
    Some(
        match name.as_str() {
            "text_display" | "textdisplay" | "markdown" | "md" | "heading" | "paragraph" => "text",
            "gallery" | "image" | "images" | "media" | "mediagallery" | "photo" | "photos" => {
                "media_gallery"
            }
            "row" | "actionrow" | "actions" | "buttons" => "action_row",
            "divider" | "spacer" | "hr" => "separator",
            "select" | "select_menu" | "stringselect" => "string_select",
            "attachment" | "document" => "file",
            "card" | "box" | "panel" => "container",
            other => other,
        }
        .to_string(),
    )
}

fn infer_message_type(map: &Map<String, Value>) -> Option<&'static str> {
    if map.contains_key("items") {
        Some("media_gallery")
    } else if map.contains_key("accessory") {
        Some("section")
    } else if map.contains_key("options") {
        Some("string_select")
    } else if map.contains_key("style") || (map.contains_key("label") && map.contains_key("url")) {
        Some("button")
    } else if map.contains_key("content") || map.contains_key("text") {
        Some("text")
    } else if map.contains_key("divider") || map.contains_key("spacing") {
        Some("separator")
    } else {
        None
    }
}

fn is_control(kind: &str) -> bool {
    matches!(
        kind,
        "button"
            | "string_select"
            | "user_select"
            | "role_select"
            | "mentionable_select"
            | "channel_select"
    )
}

/// Normalize a list of message nodes. Bare buttons are grouped into action
/// rows (five per row) and bare selects get a row each, because Discord
/// never accepts an interactive control outside a row or section.
fn normalize_message_list(items: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut pending_buttons: Vec<Value> = Vec::new();
    let flush = |out: &mut Vec<Value>, pending: &mut Vec<Value>| {
        for chunk in pending.chunks(5) {
            out.push(json!({"type": "action_row", "children": chunk}));
        }
        pending.clear();
    };
    for item in items {
        for node in normalize_message_node(item) {
            let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "button" {
                pending_buttons.push(node);
                continue;
            }
            flush(&mut out, &mut pending_buttons);
            if is_control(kind) {
                out.push(json!({"type": "action_row", "children": [node]}));
            } else {
                out.push(node);
            }
        }
    }
    flush(&mut out, &mut pending_buttons);
    out
}

/// Normalize one message node. Returns several nodes when a shape has to be
/// split (a section with no accessory becomes plain text).
fn normalize_message_node(item: Value) -> Vec<Value> {
    let mut map = match item {
        Value::String(text) => return vec![json!({"type": "text", "content": text})],
        Value::Object(map) => map,
        other => return vec![other],
    };
    let kind = message_type(map.get("type"))
        .or_else(|| infer_message_type(&map).map(str::to_string))
        .unwrap_or_default();
    strip_wire_id(&mut map);
    if !kind.is_empty() {
        map.insert("type".into(), json!(kind));
    }
    match kind.as_str() {
        "text" => {
            rename(&mut map, "text", "content");
            if let Some(Value::Array(lines)) = map.get("content") {
                let joined: Vec<String> = lines
                    .iter()
                    .map(|line| {
                        line.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| line.to_string())
                    })
                    .collect();
                map.insert("content".into(), json!(joined.join("\n")));
            }
        }
        "action_row" => {
            rename(&mut map, "components", "children");
            if let Some(Value::Array(children)) = map.remove("children") {
                let children: Vec<Value> = children
                    .into_iter()
                    .flat_map(normalize_message_node)
                    .collect();
                map.insert("children".into(), Value::Array(children));
            }
        }
        "button" => normalize_button(&mut map),
        "string_select" => normalize_string_select(&mut map),
        "user_select" | "role_select" | "mentionable_select" | "channel_select" => {
            normalize_entity_select(&mut map)
        }
        "section" => {
            rename(&mut map, "components", "children");
            let children: Vec<Value> = match map.remove("children") {
                Some(Value::Array(children)) => children,
                Some(other) => vec![other],
                None => Vec::new(),
            }
            .into_iter()
            .map(section_text)
            .collect();
            match map.remove("accessory") {
                Some(accessory) => {
                    map.insert("children".into(), Value::Array(children));
                    map.insert("accessory".into(), normalize_accessory(accessory));
                }
                None => {
                    // Discord requires an accessory; without one this is just text.
                    return children
                        .into_iter()
                        .map(|mut child| {
                            if let Value::Object(ref mut child) = child {
                                child.insert("type".into(), json!("text"));
                            }
                            child
                        })
                        .collect();
                }
            }
        }
        "thumbnail" => {
            normalize_media_holder(&mut map);
            // Thumbnails only exist as section accessories; standalone, show
            // the image as a one-item gallery instead.
            map.remove("type");
            return vec![json!({"type": "media_gallery", "items": [Value::Object(map)]})];
        }
        "media_gallery" => {
            for alias in ["images", "media", "urls", "files", "children", "components"] {
                if !map.contains_key("items") {
                    rename(&mut map, alias, "items");
                }
            }
            let items = match map.remove("items") {
                Some(Value::Array(items)) => items,
                Some(other) => vec![other],
                None => Vec::new(),
            };
            let items: Vec<Value> = items.into_iter().map(gallery_item).collect();
            map.insert("items".into(), Value::Array(items));
        }
        "file" => {
            if !map.contains_key("media") {
                for alias in ["file", "attachment"] {
                    rename(&mut map, alias, "media");
                }
            }
            if !map.contains_key("media") {
                if let Some(id) = map.remove("file_id") {
                    map.insert("media".into(), json!({"kind": "file", "file_id": id}));
                } else if let Some(url) = map.remove("url") {
                    map.insert("media".into(), url);
                }
            }
            if let Some(media) = map.remove("media") {
                map.insert("media".into(), normalize_media(media));
            }
            map.remove("spoiler");
        }
        "separator" => {
            if let Some(spacing) = map.remove("spacing") {
                let spacing = match spacing.as_u64() {
                    Some(2) => json!("large"),
                    Some(_) => json!("small"),
                    None => json!(
                        spacing
                            .as_str()
                            .map(str::to_ascii_lowercase)
                            .unwrap_or_else(|| "small".into())
                    ),
                };
                map.insert("spacing".into(), spacing);
            }
        }
        "container" => {
            rename(&mut map, "children", "components");
            for alias in ["color", "accent", "accent_colour"] {
                if !map.contains_key("accent_color") {
                    rename(&mut map, alias, "accent_color");
                }
            }
            if let Some(accent) = map.remove("accent_color") {
                if let Some(accent) = color(&accent) {
                    map.insert("accent_color".into(), json!(accent));
                } else if !accent.is_null() {
                    map.insert("accent_color".into(), accent);
                }
            }
            if let Some(Value::Array(children)) = map.remove("components") {
                map.insert(
                    "components".into(),
                    Value::Array(normalize_message_list(children)),
                );
            }
        }
        _ => {}
    }
    vec![Value::Object(map)]
}

fn section_text(child: Value) -> Value {
    match child {
        Value::String(text) => json!({"content": text}),
        Value::Object(mut map) => {
            map.remove("type");
            strip_wire_id(&mut map);
            rename(&mut map, "text", "content");
            Value::Object(map)
        }
        other => other,
    }
}

fn normalize_accessory(accessory: Value) -> Value {
    let mut map = match accessory {
        Value::String(url) => {
            return json!({"type": "thumbnail", "media": normalize_media(Value::String(url))});
        }
        Value::Object(map) => map,
        other => return other,
    };
    strip_wire_id(&mut map);
    let kind = message_type(map.get("type")).unwrap_or_else(|| {
        if map.contains_key("media") || (map.contains_key("url") && !map.contains_key("label")) {
            "thumbnail".to_string()
        } else {
            "button".to_string()
        }
    });
    map.insert("type".into(), json!(kind));
    if kind == "thumbnail" {
        normalize_media_holder(&mut map);
    } else {
        normalize_button(&mut map);
    }
    Value::Object(map)
}

fn gallery_item(item: Value) -> Value {
    match item {
        Value::String(_) => json!({"media": normalize_media(item)}),
        Value::Object(mut map) => {
            normalize_media_holder(&mut map);
            map.remove("type");
            Value::Object(map)
        }
        other => other,
    }
}

/// Give an object that should carry `media` (thumbnail, gallery item) its
/// canonical media field, lifting `url`/`file_id`/`alt` if needed.
fn normalize_media_holder(map: &mut Map<String, Value>) {
    rename(map, "alt", "description");
    if !map.contains_key("media") {
        if let Some(id) = map.remove("file_id") {
            map.insert("media".into(), json!({"kind": "file", "file_id": id}));
        } else if let Some(url) = map.remove("url") {
            map.insert("media".into(), url);
        } else if let Some(file) = map.remove("file") {
            map.insert("media".into(), file);
        }
    }
    if let Some(media) = map.remove("media") {
        map.insert("media".into(), normalize_media(media));
    }
}

fn normalize_media(media: Value) -> Value {
    match media {
        Value::String(text) => {
            let lower = text.to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                json!({"kind": "remote", "url": text})
            } else {
                json!({"kind": "file", "file_id": text})
            }
        }
        Value::Object(mut map) => {
            if !map.contains_key("kind") {
                if let Some(id) = map.remove("file_id").or_else(|| map.remove("id")) {
                    return json!({"kind": "file", "file_id": id});
                }
                if map.contains_key("url") {
                    map.insert("kind".into(), json!("remote"));
                }
            }
            if map.get("kind").and_then(Value::as_str) == Some("remote") {
                for wire_only in [
                    "proxy_url",
                    "width",
                    "height",
                    "content_type",
                    "placeholder",
                ] {
                    map.remove(wire_only);
                }
            }
            Value::Object(map)
        }
        other => other,
    }
}

fn normalize_button(map: &mut Map<String, Value>) {
    map.insert("type".into(), json!("button"));
    rename(map, "custom_id", "logical_id");
    rename(map, "text", "label");
    if let Some(style) = map.remove("style") {
        let style = match style.as_u64() {
            Some(1) => json!("primary"),
            Some(2) => json!("secondary"),
            Some(3) => json!("success"),
            Some(4) => json!("danger"),
            Some(5) => json!("link"),
            Some(6) => json!("premium"),
            _ => match style.as_str().map(str::to_ascii_lowercase).as_deref() {
                Some("blurple") => json!("primary"),
                Some("grey") | Some("gray") => json!("secondary"),
                Some("green") => json!("success"),
                Some("red") => json!("danger"),
                Some("url") => json!("link"),
                Some(other) => json!(other),
                None => style,
            },
        };
        map.insert("style".into(), style);
    }
    if !map.contains_key("style") {
        let style = if map.contains_key("url") {
            "link"
        } else if map.contains_key("sku_id") {
            "premium"
        } else {
            "secondary"
        };
        map.insert("style".into(), json!(style));
    }
    let style = map.get("style").and_then(Value::as_str).unwrap_or("");
    if !matches!(style, "link" | "premium") && !map.contains_key("logical_id") {
        let seed = map
            .get("label")
            .and_then(Value::as_str)
            .map(slug)
            .filter(|slug| !slug.is_empty())
            .unwrap_or_else(|| "button".into());
        map.insert("logical_id".into(), json!(seed));
    }
    normalize_emoji(map);
}

fn normalize_string_select(map: &mut Map<String, Value>) {
    map.insert("type".into(), json!("string_select"));
    rename(map, "custom_id", "logical_id");
    ensure_select_id(map);
    if let Some(Value::Array(options)) = map.remove("options") {
        let options: Vec<Value> = options
            .into_iter()
            .map(|option| match option {
                Value::String(text) => json!({"label": text, "value": text}),
                Value::Object(mut option) => {
                    if !option.contains_key("value") {
                        if let Some(label) = option.get("label").cloned() {
                            option.insert("value".into(), label);
                        }
                    }
                    normalize_emoji(&mut option);
                    Value::Object(option)
                }
                other => other,
            })
            .collect();
        map.insert("options".into(), Value::Array(options));
    }
}

fn normalize_entity_select(map: &mut Map<String, Value>) {
    rename(map, "custom_id", "logical_id");
    ensure_select_id(map);
    if let Some(Value::Array(defaults)) = map.remove("default_values") {
        let defaults: Vec<Value> = defaults
            .into_iter()
            .map(|value| match value {
                Value::Object(mut value) => {
                    rename(&mut value, "type", "kind");
                    Value::Object(value)
                }
                other => other,
            })
            .collect();
        map.insert("default_values".into(), Value::Array(defaults));
    }
}

fn ensure_select_id(map: &mut Map<String, Value>) {
    if !map.contains_key("logical_id") {
        let seed = map
            .get("placeholder")
            .and_then(Value::as_str)
            .map(slug)
            .filter(|slug| !slug.is_empty())
            .unwrap_or_else(|| "select".into());
        map.insert("logical_id".into(), json!(seed));
    }
}

fn normalize_emoji(map: &mut Map<String, Value>) {
    if let Some(Value::String(name)) = map.get("emoji").cloned() {
        map.insert("emoji".into(), json!({"name": name}));
    }
}

/// Modal lists: a bare control with a `label` is wrapped in a Label, which is
/// the modern Discord way to title a modal field.
fn normalize_modal_list(items: Vec<Value>) -> Vec<Value> {
    items.into_iter().map(normalize_modal_node).collect()
}

fn modal_type(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(number) = value.as_u64() {
        return Some(
            match number {
                1 => "action_row",
                3 => "string_select",
                4 => "text_input",
                5 => "user_select",
                6 => "role_select",
                7 => "mentionable_select",
                8 => "channel_select",
                18 => "label",
                19 => "file_upload",
                21 => "radio_group",
                22 => "checkbox_group",
                23 => "checkbox",
                _ => return None,
            }
            .to_string(),
        );
    }
    let name = value
        .as_str()?
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_");
    Some(
        match name.as_str() {
            "input" | "text" | "textinput" | "text_field" | "textarea" => "text_input",
            "select" | "select_menu" => "string_select",
            "radio" | "radios" => "radio_group",
            "checkboxes" => "checkbox_group",
            "upload" | "file" => "file_upload",
            "row" | "actionrow" => "action_row",
            other => other,
        }
        .to_string(),
    )
}

fn normalize_modal_node(item: Value) -> Value {
    let Value::Object(mut map) = item else {
        return item;
    };
    strip_wire_id(&mut map);
    let kind = modal_type(map.get("type")).unwrap_or_else(|| {
        if map.contains_key("component") {
            "label".into()
        } else {
            "text_input".into()
        }
    });
    match kind.as_str() {
        "label" => {
            map.insert("type".into(), json!("label"));
            if let Some(component) = map.remove("component") {
                map.insert("component".into(), normalize_modal_control(component));
            }
            Value::Object(map)
        }
        "action_row" => {
            map.insert("type".into(), json!("action_row"));
            rename(&mut map, "components", "children");
            if let Some(Value::Array(children)) = map.remove("children") {
                // Text inputs inside rows are the legacy shape; Discord now
                // wants each field inside a Label, so lift labelled ones out.
                if children.len() == 1 && children[0].get("label").is_some() {
                    return normalize_modal_node(children.into_iter().next().unwrap_or_default());
                }
                let children: Vec<Value> =
                    children.into_iter().map(normalize_modal_control).collect();
                map.insert("children".into(), Value::Array(children));
            }
            Value::Object(map)
        }
        _ => {
            // A bare control: wrap in a Label using its own label text.
            map.insert("type".into(), json!(kind));
            let label = map
                .remove("label")
                .and_then(|label| label.as_str().map(str::to_string));
            let description = map.remove("description");
            let control = normalize_modal_control(Value::Object(map));
            let mut node = json!({
                "type": "label",
                "label": label.unwrap_or_else(|| "Field".into()),
                "component": control,
            });
            if let Some(description) = description.filter(|d| d.is_string()) {
                node["description"] = description;
            }
            node
        }
    }
}

fn normalize_modal_control(item: Value) -> Value {
    let Value::Object(mut map) = item else {
        return item;
    };
    strip_wire_id(&mut map);
    let kind = modal_type(map.get("type")).unwrap_or_else(|| "text_input".into());
    map.insert("type".into(), json!(kind));
    rename(&mut map, "custom_id", "logical_id");
    match kind.as_str() {
        "text_input" => {
            map.remove("label");
            let style = match map.remove("style") {
                Some(style) if style.as_u64() == Some(2) => json!("paragraph"),
                Some(style) if style.as_u64() == Some(1) => json!("short"),
                Some(Value::String(style)) => match style.to_ascii_lowercase().as_str() {
                    "long" | "multiline" | "paragraph" => json!("paragraph"),
                    other => json!(other),
                },
                Some(other) => other,
                None => json!("short"),
            };
            map.insert("style".into(), style);
            if !map.contains_key("logical_id") {
                map.insert("logical_id".into(), json!("input"));
            }
        }
        "string_select" => normalize_string_select(&mut map),
        "radio_group" | "checkbox_group" => {
            ensure_select_id(&mut map);
            if let Some(Value::Array(options)) = map.remove("options") {
                let options: Vec<Value> = options
                    .into_iter()
                    .map(|option| match option {
                        Value::String(text) => json!({"label": text, "value": text}),
                        other => other,
                    })
                    .collect();
                map.insert("options".into(), Value::Array(options));
            }
        }
        "user_select" | "role_select" | "mentionable_select" | "channel_select" => {
            normalize_entity_select(&mut map)
        }
        _ => {
            if !map.contains_key("logical_id") {
                map.insert("logical_id".into(), json!(kind));
            }
        }
    }
    Value::Object(map)
}

/// Discord's optional numeric component `id` is wire-only; drop it.
fn strip_wire_id(map: &mut Map<String, Value>) {
    if map.get("id").is_some_and(Value::is_number) {
        map.remove("id");
    }
}

fn rename(map: &mut Map<String, Value>, from: &str, to: &str) {
    if map.contains_key(to) {
        return;
    }
    if let Some(value) = map.remove(from) {
        map.insert(to.to_string(), value);
    }
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
        if out.len() >= 48 {
            break;
        }
    }
    out.trim_end_matches('_').to_string()
}

/// Parse an accent colour: integer, `#RRGGBB`, `0xRRGGBB`, or a few names.
pub fn color(value: &Value) -> Option<u32> {
    if let Some(number) = value.as_u64() {
        return u32::try_from(number).ok().filter(|n| *n <= 0xFF_FFFF);
    }
    let text = value.as_str()?.trim().to_ascii_lowercase();
    let named = match text.as_str() {
        "blurple" | "discord" => Some(0x5865F2),
        "green" | "success" => Some(0x57F287),
        "yellow" | "warning" => Some(0xFEE75C),
        "red" | "danger" | "error" => Some(0xED4245),
        "fuchsia" | "pink" => Some(0xEB459E),
        "white" => Some(0xFFFFFF),
        "black" => Some(0x23272A),
        "gray" | "grey" => Some(0x99AAB5),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let hex = text
        .strip_prefix('#')
        .or_else(|| text.strip_prefix("0x"))
        .unwrap_or(&text);
    let hex = if hex.len() == 3 {
        hex.chars().flat_map(|c| [c, c]).collect::<String>()
    } else {
        hex.to_string()
    };
    if hex.len() == 6 {
        return u32::from_str_radix(&hex, 16).ok();
    }
    text.parse::<u32>().ok().filter(|n| *n <= 0xFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(raw: Value) -> UiDocument {
        let value = coerce_document(&raw, Surface::Message).expect("coerces");
        parse_document(&value).expect("parses")
    }

    #[test]
    fn bare_text_list_becomes_a_document() {
        let document = message(json!([{"type": "text_display", "content": "hi"}]));
        assert!(matches!(document, UiDocument::Message(_)));
    }

    #[test]
    fn json_string_and_wrapper_are_unwrapped() {
        let raw = json!({"document": "{\"components\":[\"hello\"]}"});
        assert!(matches!(message(raw), UiDocument::Message(_)));
    }

    #[test]
    fn discord_numeric_json_is_accepted() {
        let raw = json!({"components": [{
            "type": 17, "accent_color": "#5865F2", "components": [
                {"type": 10, "content": "## Title"},
                {"type": 14, "spacing": 2},
                {"type": 9, "components": [{"type": 10, "content": "side"}],
                 "accessory": {"type": 11, "media": {"url": "https://example.com/a.png"}}},
                {"type": 12, "items": [{"media": {"url": "https://example.com/b.png"}}]},
                {"type": 1, "components": [{"type": 2, "style": 5, "label": "Docs", "url": "https://example.com"}]}
            ]
        }]});
        let UiDocument::Message(document) = message(raw) else {
            panic!("message")
        };
        let MessageNode::Container(container) = &document.components[0] else {
            panic!("container")
        };
        assert_eq!(container.accent_color, Some(0x5865F2));
        assert_eq!(container.components.len(), 5);
    }

    #[test]
    fn gallery_accepts_bare_urls_and_file_ids() {
        let raw = json!({"components": [{"type": "media_gallery", "items": {"file_id": "abc"}},
                                         {"type": "gallery", "items": ["https://example.com/x.png", "f00d"]}]});
        let UiDocument::Message(document) = message(raw) else {
            panic!("message")
        };
        assert_eq!(document.components.len(), 2);
    }

    #[test]
    fn bare_buttons_are_grouped_into_rows() {
        let raw = json!({"components": [
            {"type": "button", "label": "One"},
            {"type": "button", "label": "Two", "style": "success"},
            {"type": "text", "content": "after"}
        ]});
        let UiDocument::Message(document) = message(raw) else {
            panic!("message")
        };
        let MessageNode::ActionRow(row) = &document.components[0] else {
            panic!("row")
        };
        assert_eq!(row.children.len(), 2);
    }

    #[test]
    fn section_children_may_be_typed_text_and_missing_accessory_degrades() {
        let raw = json!({"components": [
            {"type": "section", "components": [{"type": "text_display", "content": "a"}],
             "accessory": {"type": "thumbnail", "media": "https://example.com/t.png"}},
            {"type": "section", "children": ["no accessory"]}
        ]});
        let UiDocument::Message(document) = message(raw) else {
            panic!("message")
        };
        assert!(matches!(document.components[0], MessageNode::Section(_)));
        assert!(matches!(document.components[1], MessageNode::Text(_)));
    }

    #[test]
    fn title_and_accent_wrap_everything_in_a_card() {
        let raw = json!({"title": "Report", "accent_color": "green", "components": ["body"]});
        let UiDocument::Message(document) = message(raw) else {
            panic!("message")
        };
        assert_eq!(document.components.len(), 1);
        assert!(matches!(document.components[0], MessageNode::Container(_)));
    }

    #[test]
    fn unknown_field_errors_name_the_component_path() {
        let raw = json!({"components": [{"type": "container", "components": [
            {"type": "text", "content": "ok"},
            {"type": "text", "content": "bad", "bogus": true}
        ]}]});
        let value = coerce_document(&raw, Surface::Message).unwrap();
        let error = parse_document(&value).unwrap_err();
        assert!(error.contains("$.components[0].components[1]"), "{error}");
        assert!(error.contains("bogus"), "{error}");
    }

    #[test]
    fn modal_text_inputs_are_wrapped_in_labels() {
        let raw = json!({"title": "Feedback", "components": [
            {"type": 4, "custom_id": "why", "label": "Why?", "style": 2}
        ]});
        let value = coerce_document(&raw, Surface::Modal).unwrap();
        let UiDocument::Modal(modal) = parse_document(&value).unwrap() else {
            panic!("modal")
        };
        assert!(matches!(modal.components[0], ModalNode::Label(_)));
    }

    #[test]
    fn colours_parse() {
        assert_eq!(color(&json!("#fff")), Some(0xFFFFFF));
        assert_eq!(color(&json!("0x5865F2")), Some(0x5865F2));
        assert_eq!(color(&json!(703487)), Some(703487));
        assert_eq!(color(&json!(0x1000000)), None);
    }
}
