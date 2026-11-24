//! Compile the Gray-owned UI protocol into Discord's Components V2 wire JSON.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::IpAddr;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::channels::discord::component_protocol::*;

pub const IS_COMPONENTS_V2: u64 = 1 << 15;
pub const EPHEMERAL: u64 = 1 << 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub code: String,
    pub path: String,
    pub message: String,
}

impl CompileError {
    pub(crate) fn new(code: &str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}: {}", self.code, self.path, self.message)
    }
}

impl std::error::Error for CompileError {}

pub trait ComponentIdAllocator {
    fn allocate(&mut self, logical_id: &str, kind: &str) -> Result<String, CompileError>;
    fn resolve_file(&self, file_id: &str) -> Result<FileRef, CompileError>;
    fn premium_enabled(&self) -> bool {
        false
    }
    fn sku_allowed(&self, _sku_id: &str) -> bool {
        false
    }
}

pub struct CompileContext<'a> {
    pub origin: Origin,
    pub allocator: &'a mut dyn ComponentIdAllocator,
}

impl<'a> CompileContext<'a> {
    pub fn new(origin: Origin, allocator: &'a mut dyn ComponentIdAllocator) -> Self {
        Self { origin, allocator }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompiledMessage {
    pub components: Vec<Value>,
    pub flags: u64,
    pub visibility: Visibility,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompiledModal {
    pub custom_id: String,
    pub title: String,
    pub components: Vec<Value>,
}

/// Deterministic allocator for trusted internal rendering and unit tests.
/// Stateful Gateway/agent paths provide a database-backed implementation.
#[derive(Debug, Default)]
pub struct DeterministicAllocator {
    next: u64,
    files: HashMap<String, FileRef>,
}

impl DeterministicAllocator {
    pub fn with_file(mut self, file: FileRef) -> Self {
        self.files.insert(file.id.clone(), file);
        self
    }

    pub fn next_id(&mut self, logical_id: &str, kind: &str) -> String {
        self.next += 1;
        format!("gray_{kind}_{}_{}", sanitize_id(logical_id), self.next)
    }
}

impl ComponentIdAllocator for DeterministicAllocator {
    fn allocate(&mut self, logical_id: &str, kind: &str) -> Result<String, CompileError> {
        validate_logical_id(logical_id)?;
        Ok(self.next_id(logical_id, kind))
    }

    fn resolve_file(&self, file_id: &str) -> Result<FileRef, CompileError> {
        self.files.get(file_id).cloned().ok_or_else(|| {
            CompileError::new(
                "file_not_found",
                "$.file_id",
                format!("managed file {file_id:?} is not available"),
            )
        })
    }
}

struct Compiler<'a, 'b> {
    context: &'b mut CompileContext<'a>,
    count: usize,
    logical_ids: HashSet<String>,
    text_chars: usize,
    in_modal: bool,
}

pub fn compile_message(
    document: &MessageDocument,
    context: &mut CompileContext<'_>,
) -> Result<CompiledMessage, CompileError> {
    validate_document_header(&document.version, &document.document_id, "$.document_id")?;
    if document.components.is_empty() {
        return Err(CompileError::new(
            "empty_document",
            "$.components",
            "a message needs at least one component",
        ));
    }
    let mut compiler = Compiler {
        context,
        count: 0,
        logical_ids: HashSet::new(),
        text_chars: 0,
        in_modal: false,
    };
    let mut components = Vec::with_capacity(document.components.len());
    for (index, node) in document.components.iter().enumerate() {
        let path = format!("$.components[{index}]");
        if matches!(
            node,
            MessageNode::Button(_) | MessageNode::StringSelect(_) | MessageNode::UserSelect(_)
        ) {
            return Err(CompileError::new(
                "invalid_placement",
                path,
                "interactive controls must be nested in an action row",
            ));
        }
        components.push(omit_nulls(compiler.message_node(node, &path)?));
    }
    Ok(CompiledMessage {
        components,
        flags: IS_COMPONENTS_V2,
        visibility: document.visibility,
    })
}

pub fn compile_modal(
    document: &ModalDocument,
    context: &mut CompileContext<'_>,
) -> Result<CompiledModal, CompileError> {
    validate_document_header(&document.version, &document.document_id, "$.document_id")?;
    if document.title.is_empty() || document.title.chars().count() > MAX_MODAL_TITLE_CHARS {
        return Err(CompileError::new(
            "invalid_text",
            "$.title",
            "modal title must contain 1..=45 characters",
        ));
    }
    let mut compiler = Compiler {
        context,
        count: 0,
        logical_ids: HashSet::new(),
        text_chars: 0,
        in_modal: true,
    };
    let custom_id = compiler.allocate_logical_id(&document.document_id, "modal", "$.custom_id")?;
    let mut components = Vec::with_capacity(document.components.len());
    for (index, node) in document.components.iter().enumerate() {
        components.push(omit_nulls(
            compiler.modal_node(node, &format!("$.components[{index}]"))?,
        ));
    }
    Ok(CompiledModal {
        custom_id,
        title: document.title.clone(),
        components,
    })
}

impl Compiler<'_, '_> {
    fn message_node(&mut self, node: &MessageNode, path: &str) -> Result<Value, CompileError> {
        self.count_component(path)?;
        match node {
            MessageNode::ActionRow(row) => {
                if row.children.is_empty() || row.children.len() > 5 {
                    return Err(CompileError::new(
                        "invalid_row",
                        path,
                        "action row must contain 1..=5 controls",
                    ));
                }
                let select_count = row
                    .children
                    .iter()
                    .filter(|child| {
                        matches!(
                            child,
                            MessageNode::StringSelect(_)
                                | MessageNode::UserSelect(_)
                                | MessageNode::RoleSelect(_)
                                | MessageNode::MentionableSelect(_)
                                | MessageNode::ChannelSelect(_)
                        )
                    })
                    .count();
                if select_count > 1 || (select_count == 1 && row.children.len() > 1) {
                    return Err(CompileError::new(
                        "invalid_row",
                        path,
                        "action row must contain one select or up to five buttons",
                    ));
                }
                let mut children = Vec::with_capacity(row.children.len());
                for (index, child) in row.children.iter().enumerate() {
                    let child_path = format!("{path}.children[{index}]");
                    let child = match child {
                        MessageNode::Button(button) => self.button(button, &child_path)?,
                        MessageNode::StringSelect(select) => {
                            self.string_select(select, &child_path, 3)?
                        }
                        MessageNode::UserSelect(select) => {
                            self.entity_select(select, &child_path, 5, "user")?
                        }
                        MessageNode::RoleSelect(select) => {
                            self.entity_select(select, &child_path, 6, "role")?
                        }
                        MessageNode::MentionableSelect(select) => {
                            self.entity_select(select, &child_path, 7, "mentionable")?
                        }
                        MessageNode::ChannelSelect(select) => {
                            self.entity_select(select, &child_path, 8, "channel")?
                        }
                        _ => {
                            return Err(CompileError::new(
                                "invalid_row_child",
                                child_path,
                                "action rows accept buttons and selects only",
                            ));
                        }
                    };
                    children.push(child);
                }
                Ok(json!({"type": 1, "components": children}))
            }
            MessageNode::Button(_)
            | MessageNode::StringSelect(_)
            | MessageNode::UserSelect(_)
            | MessageNode::RoleSelect(_)
            | MessageNode::MentionableSelect(_)
            | MessageNode::ChannelSelect(_) => Err(CompileError::new(
                "invalid_placement",
                path,
                "interactive controls must be nested in an action row",
            )),
            MessageNode::Text(text) => {
                let content = bounded_text(&text.content, path)?;
                self.spend_text(&content, path)?;
                Ok(json!({"type": 10, "content": content}))
            }
            MessageNode::Section(section) => self.section(section, path),
            MessageNode::Thumbnail(_) => Err(CompileError::new(
                "invalid_placement",
                path,
                "a thumbnail is only valid as a section accessory; use a media_gallery for a standalone image",
            )),
            MessageNode::MediaGallery(gallery) => {
                if gallery.items.is_empty() || gallery.items.len() > 10 {
                    return Err(CompileError::new(
                        "invalid_gallery",
                        path,
                        "media gallery must contain 1..=10 items",
                    ));
                }
                let mut items = Vec::with_capacity(gallery.items.len());
                for (index, item) in gallery.items.iter().enumerate() {
                    let item_path = format!("{path}.items[{index}]");
                    let media_path = format!("{item_path}.media");
                    // Gallery cells only draw image/video; refuse the rest
                    // before resolving it so nothing half-posts.
                    self.require_renderable(&item.media, &media_path)?;
                    let (description, spoiler) =
                        media_extras(&item.media, &item.description, item.spoiler);
                    validate_media_description(description.as_deref(), &item_path)?;
                    items.push(json!({
                        "media": self.media(&item.media, &media_path)?,
                        "description": description,
                        "spoiler": spoiler
                    }));
                }
                Ok(json!({"type": 12, "items": items}))
            }
            MessageNode::File(file) => {
                if matches!(file.media, MediaSource::Remote { .. }) {
                    return Err(CompileError::new(
                        "invalid_media",
                        format!("{path}.media"),
                        "Discord file components only accept uploaded files; import it with discord_file and pass its file_id (remote images belong in a media_gallery)",
                    ));
                }
                Ok(json!({
                    "type": 13,
                    "file": {"url": self.media_url(&file.media, &format!("{path}.media"))?}
                }))
            }
            MessageNode::Separator(separator) => Ok(json!({
                "type": 14,
                "divider": separator.divider.unwrap_or(true),
                "spacing": match separator.spacing.unwrap_or_default() {
                    SeparatorSpacing::Small => 1,
                    SeparatorSpacing::Large => 2,
                }
            })),
            MessageNode::Container(container) => {
                if container
                    .accent_color
                    .is_some_and(|color| color > 0xFF_FFFF)
                {
                    return Err(CompileError::new(
                        "invalid_color",
                        format!("{path}.accent_color"),
                        "accent_color must be an RGB integer from 0x000000 to 0xFFFFFF",
                    ));
                }
                if container.components.is_empty() {
                    return Err(CompileError::new(
                        "invalid_container",
                        path,
                        "container must contain at least one component",
                    ));
                }
                let mut children = Vec::with_capacity(container.components.len());
                for (index, child) in container.components.iter().enumerate() {
                    let child_path = format!("{path}.components[{index}]");
                    if matches!(child, MessageNode::Container(_)) {
                        return Err(CompileError::new(
                            "invalid_placement",
                            child_path,
                            "containers cannot be nested",
                        ));
                    }
                    children.push(self.message_node(child, &child_path)?);
                }
                Ok(
                    json!({"type": 17, "accent_color": container.accent_color, "spoiler": container.spoiler, "components": children}),
                )
            }
        }
    }

    fn section(&mut self, section: &Section, path: &str) -> Result<Value, CompileError> {
        if section.children.is_empty() || section.children.len() > 3 {
            return Err(CompileError::new(
                "invalid_section",
                path,
                "section must contain 1..=3 text children",
            ));
        }
        let mut children = Vec::with_capacity(section.children.len());
        for (index, child) in section.children.iter().enumerate() {
            let child_path = format!("{path}.children[{index}]");
            let content = bounded_text(&child.content, &child_path)?;
            self.spend_text(&content, &child_path)?;
            children.push(json!({"type": 10, "content": content}));
        }
        let accessory = match &section.accessory {
            SectionAccessory::Button(button) => {
                self.button(button, &format!("{path}.accessory"))?
            }
            SectionAccessory::Thumbnail(thumbnail) => {
                let (description, spoiler) =
                    media_extras(&thumbnail.media, &thumbnail.description, thumbnail.spoiler);
                validate_media_description(description.as_deref(), &format!("{path}.accessory"))?;
                json!({
                    "type": 11,
                    "media": self.media(&thumbnail.media, &format!("{path}.accessory.media"))?,
                    "description": description,
                    "spoiler": spoiler
                })
            }
        };
        Ok(json!({"type": 9, "components": children, "accessory": accessory}))
    }

    fn button(&mut self, button: &Button, path: &str) -> Result<Value, CompileError> {
        if button.label.chars().count() > 80 {
            return Err(CompileError::new(
                "invalid_text",
                format!("{path}.label"),
                "button label is too long",
            ));
        }
        let mut value = Map::new();
        value.insert("type".into(), json!(2));
        value.insert("style".into(), json!(button_style(button.style)));
        if !button.label.is_empty() {
            value.insert("label".into(), json!(button.label));
        }
        if let Some(emoji) = &button.emoji {
            value.insert(
                "emoji".into(),
                serde_json::to_value(emoji).map_err(|error| {
                    CompileError::new("invalid_component", path, error.to_string())
                })?,
            );
        }
        if button.disabled {
            value.insert("disabled".into(), json!(true));
        }
        match button.style {
            ButtonStyle::Link => {
                let url = button.url.as_deref().ok_or_else(|| {
                    CompileError::new("invalid_url", path, "link button requires url")
                })?;
                validate_url(url, &format!("{path}.url"))?;
                value.insert("url".into(), json!(url));
                if button.logical_id.is_some() || button.sku_id.is_some() {
                    return Err(CompileError::new(
                        "invalid_button",
                        path,
                        "link button cannot have a logical id or sku",
                    ));
                }
            }
            ButtonStyle::Premium => {
                if !self.context.allocator.premium_enabled() {
                    return Err(CompileError::new(
                        "premium_disabled",
                        path,
                        "premium buttons are disabled",
                    ));
                }
                let sku = button.sku_id.as_deref().ok_or_else(|| {
                    CompileError::new("invalid_button", path, "premium button requires sku_id")
                })?;
                if !self.context.allocator.sku_allowed(sku) {
                    return Err(CompileError::new(
                        "premium_not_allowed",
                        path,
                        "premium sku is not configured",
                    ));
                }
                value.insert("sku_id".into(), json!(sku));
            }
            _ => {
                let logical_id = button.logical_id.as_ref().ok_or_else(|| {
                    CompileError::new("invalid_button", path, "action button requires logical_id")
                })?;
                let custom_id = self.allocate_logical_id(logical_id, "button", path)?;
                value.insert("custom_id".into(), json!(custom_id));
                if button.url.is_some() || button.sku_id.is_some() {
                    return Err(CompileError::new(
                        "invalid_button",
                        path,
                        "action button cannot have url or sku_id",
                    ));
                }
            }
        }
        Ok(Value::Object(value))
    }

    fn string_select(
        &mut self,
        select: &StringSelect,
        path: &str,
        type_id: u8,
    ) -> Result<Value, CompileError> {
        if select.options.is_empty() || select.options.len() > MAX_SELECT_OPTIONS {
            return Err(CompileError::new(
                "invalid_options",
                path,
                "string select must contain 1..=25 options",
            ));
        }
        validate_values(select.min_values, select.max_values, path)?;
        let custom_id = self.allocate_logical_id(&select.logical_id, "select", path)?;
        let required = self.in_modal.then_some(select.required);
        let mut options = Vec::with_capacity(select.options.len());
        for (index, option) in select.options.iter().enumerate() {
            validate_option(option, &format!("{path}.options[{index}]"))?;
            options.push(json!({
                "label": option.label,
                "value": option.value,
                "description": option.description,
                "emoji": option.emoji,
                "default": option.default
            }));
        }
        Ok(
            json!({"type": type_id, "custom_id": custom_id, "placeholder": select.placeholder, "options": options, "min_values": select.min_values, "max_values": select.max_values, "required": required, "disabled": select.disabled}),
        )
    }

    fn entity_select(
        &mut self,
        select: &EntitySelect,
        path: &str,
        type_id: u8,
        kind: &str,
    ) -> Result<Value, CompileError> {
        validate_values(select.min_values, select.max_values, path)?;
        if select.default_values.len() > 25 {
            return Err(CompileError::new(
                "invalid_options",
                path,
                "default value count exceeds 25",
            ));
        }
        if type_id != 8 && select.channel_types.is_some() {
            return Err(CompileError::new(
                "invalid_select",
                path,
                "channel_types is only valid for channel selects",
            ));
        }
        if select.min_values.unwrap_or(0) > select.default_values.len() as u32 && select.required {
            return Err(CompileError::new(
                "invalid_select",
                path,
                "required select has fewer defaults than min_values",
            ));
        }
        let custom_id = self.allocate_logical_id(&select.logical_id, kind, path)?;
        let required = self.in_modal.then_some(select.required);
        let defaults: Vec<Value> = select
            .default_values
            .iter()
            .map(|value| json!({"id": value.id, "type": value.kind}))
            .collect();
        Ok(
            json!({"type": type_id, "custom_id": custom_id, "placeholder": select.placeholder, "default_values": defaults, "min_values": select.min_values, "max_values": select.max_values, "required": required, "disabled": select.disabled, "channel_types": select.channel_types}),
        )
    }

    fn modal_node(&mut self, node: &ModalNode, path: &str) -> Result<Value, CompileError> {
        self.count_component(path)?;
        match node {
            ModalNode::Label(label) => {
                if label.label.is_empty() || label.label.chars().count() > MAX_LABEL_CHARS {
                    return Err(CompileError::new(
                        "invalid_label",
                        format!("{path}.label"),
                        "label must contain 1..=45 characters",
                    ));
                }
                if label
                    .description
                    .as_ref()
                    .is_some_and(|value| value.chars().count() > MAX_DESCRIPTION_CHARS)
                {
                    return Err(CompileError::new(
                        "invalid_description",
                        format!("{path}.description"),
                        "label description is too long",
                    ));
                }
                let component =
                    self.modal_control(&label.component, &format!("{path}.component"))?;
                Ok(
                    json!({"type": 18, "label": label.label, "description": label.description, "component": component}),
                )
            }
            ModalNode::ActionRow(row) => {
                if row.children.is_empty() || row.children.len() > 5 {
                    return Err(CompileError::new(
                        "invalid_row",
                        path,
                        "modal action row must contain 1..=5 controls",
                    ));
                }
                let mut children = Vec::with_capacity(row.children.len());
                for (index, child) in row.children.iter().enumerate() {
                    children.push(self.modal_control(child, &format!("{path}.children[{index}]"))?);
                }
                Ok(json!({"type": 1, "components": children}))
            }
        }
    }

    fn modal_control(&mut self, control: &ModalControl, path: &str) -> Result<Value, CompileError> {
        match control {
            ModalControl::TextInput(input) => {
                let custom_id = self.allocate_logical_id(&input.logical_id, "text_input", path)?;
                let min = input.min_length.unwrap_or(0);
                let max = input.max_length.unwrap_or(4000);
                if min > max || max > 4000 {
                    return Err(CompileError::new(
                        "invalid_text_input",
                        path,
                        "text input lengths must satisfy 0 <= min <= max <= 4000",
                    ));
                }
                if input
                    .value
                    .as_ref()
                    .is_some_and(|value| value.chars().count() > max as usize)
                {
                    return Err(CompileError::new(
                        "invalid_text_input",
                        path,
                        "initial value exceeds max_length",
                    ));
                }
                Ok(
                    json!({"type": 4, "custom_id": custom_id, "style": match input.style { TextInputStyle::Short => 1, TextInputStyle::Paragraph => 2 }, "min_length": input.min_length, "max_length": input.max_length, "value": input.value, "placeholder": input.placeholder, "required": input.required, "disabled": input.disabled}),
                )
            }
            ModalControl::StringSelect(select) => self.string_select(select, path, 3),
            ModalControl::UserSelect(select) => self.entity_select(select, path, 5, "user"),
            ModalControl::RoleSelect(select) => self.entity_select(select, path, 6, "role"),
            ModalControl::MentionableSelect(select) => {
                self.entity_select(select, path, 7, "mentionable")
            }
            ModalControl::ChannelSelect(select) => self.entity_select(select, path, 8, "channel"),
            ModalControl::FileUpload(upload) => {
                if upload
                    .file_types
                    .as_ref()
                    .is_some_and(|types| types.len() > MAX_FILE_TYPES)
                {
                    return Err(CompileError::new(
                        "invalid_file_types",
                        path,
                        "file upload accepts at most 10 extensions",
                    ));
                }
                validate_values(upload.min_values, upload.max_values, path)?;
                if upload.max_values.unwrap_or(10) > 10 || upload.min_values.unwrap_or(0) > 10 {
                    return Err(CompileError::new(
                        "invalid_file_count",
                        path,
                        "file upload accepts at most 10 files",
                    ));
                }
                let custom_id =
                    self.allocate_logical_id(&upload.logical_id, "file_upload", path)?;
                Ok(
                    json!({"type": 19, "custom_id": custom_id, "min_values": upload.min_values, "max_values": upload.max_values, "file_types": upload.file_types, "required": upload.required}),
                )
            }
            ModalControl::RadioGroup(group) => {
                if group.options.len() < 2 || group.options.len() > MAX_RADIO_OPTIONS {
                    return Err(CompileError::new(
                        "invalid_options",
                        path,
                        "radio group must contain 2..=10 options",
                    ));
                }
                let custom_id = self.allocate_logical_id(&group.logical_id, "radio", path)?;
                let mut options = Vec::with_capacity(group.options.len());
                for (index, option) in group.options.iter().enumerate() {
                    validate_choice(option, &format!("{path}.options[{index}]"))?;
                    options.push(json!({"label": option.label, "value": option.value, "description": option.description, "default": option.default}));
                }
                Ok(
                    json!({"type": 21, "custom_id": custom_id, "options": options, "required": group.required, "disabled": group.disabled}),
                )
            }
            ModalControl::CheckboxGroup(group) => {
                if group.options.is_empty() || group.options.len() > MAX_CHECKBOX_OPTIONS {
                    return Err(CompileError::new(
                        "invalid_options",
                        path,
                        "checkbox group must contain 1..=10 options",
                    ));
                }
                validate_values(group.min_values, group.max_values, path)?;
                let custom_id =
                    self.allocate_logical_id(&group.logical_id, "checkbox_group", path)?;
                let mut options = Vec::with_capacity(group.options.len());
                for (index, option) in group.options.iter().enumerate() {
                    validate_choice(option, &format!("{path}.options[{index}]"))?;
                    options.push(json!({"label": option.label, "value": option.value, "description": option.description, "default": option.default}));
                }
                Ok(
                    json!({"type": 22, "custom_id": custom_id, "options": options, "min_values": group.min_values, "max_values": group.max_values, "required": group.required, "disabled": group.disabled}),
                )
            }
            ModalControl::Checkbox(checkbox) => {
                let custom_id = self.allocate_logical_id(&checkbox.logical_id, "checkbox", path)?;
                Ok(
                    json!({"type": 23, "custom_id": custom_id, "default": checkbox.default, "required": checkbox.required, "disabled": checkbox.disabled}),
                )
            }
        }
    }

    fn media(&mut self, media: &MediaSource, path: &str) -> Result<Value, CompileError> {
        match media {
            MediaSource::Remote { url, .. } => {
                // Discord only lets callers set `url` on an unfurled media
                // item; description/spoiler live on the item that holds it.
                validate_url(url, &format!("{path}.url"))?;
                Ok(json!({"url": url}))
            }
            MediaSource::File { .. } => Ok(json!({"url": self.media_url(media, path)?})),
        }
    }

    /// Discord renders images and video inside a media gallery. A managed file
    /// of any other type arrives as a broken "Image failed to load" card, so
    /// it is refused here, where the author can still read the path and fix
    /// the document. A remote URL is judged only by a known-bad extension: an
    /// odd or missing extension is let through, because guessing wrong would
    /// break a URL that does render.
    fn require_renderable(&self, media: &MediaSource, path: &str) -> Result<(), CompileError> {
        let label = match media {
            MediaSource::File { file_id } => {
                let file = self
                    .context
                    .allocator
                    .resolve_file(file_id)
                    .map_err(|error| prefix_path(error, path))?;
                if RENDERABLE_TYPES
                    .iter()
                    .any(|kind| file.media_type.starts_with(kind))
                {
                    return Ok(());
                }
                format!("{} is {}", file.name, file.media_type)
            }
            MediaSource::Remote { url, .. } => match unrenderable_extension(url) {
                Some(extension) => extension,
                None => return Ok(()),
            },
        };
        Err(CompileError::new(
            "invalid_gallery",
            path,
            format!(
                "a media gallery item must be image/* or video/* ({label}); \
                 use a file component instead"
            ),
        ))
    }

    fn media_url(&self, media: &MediaSource, path: &str) -> Result<String, CompileError> {
        match media {
            MediaSource::Remote { url, .. } => {
                validate_url(url, &format!("{path}.url"))?;
                Ok(url.clone())
            }
            MediaSource::File { file_id } => {
                let file = self
                    .context
                    .allocator
                    .resolve_file(file_id)
                    .map_err(|error| prefix_path(error, path))?;
                Ok(format!("attachment://{}", file.name))
            }
        }
    }

    fn allocate_logical_id(
        &mut self,
        logical_id: &LogicalId,
        kind: &str,
        path: &str,
    ) -> Result<String, CompileError> {
        validate_logical_id(&logical_id.0).map_err(|error| prefix_path(error, path))?;
        if !self.logical_ids.insert(logical_id.0.clone()) {
            return Err(CompileError::new(
                "duplicate_logical_id",
                path,
                "logical component IDs must be unique within a document",
            ));
        }
        self.context
            .allocator
            .allocate(&logical_id.0, kind)
            .map_err(|error| prefix_path(error, path))
    }

    /// Discord caps the combined Text Display content of one message.
    fn spend_text(&mut self, content: &str, path: &str) -> Result<(), CompileError> {
        self.text_chars += content.chars().count();
        if self.text_chars > MAX_TEXT_DISPLAY_CHARS {
            return Err(CompileError::new(
                "text_limit",
                path,
                "all text in one message must total at most 4000 characters; split it across messages",
            ));
        }
        Ok(())
    }

    fn count_component(&mut self, path: &str) -> Result<(), CompileError> {
        self.count += 1;
        if self.count > MAX_COMPONENTS {
            return Err(CompileError::new(
                "component_limit",
                path,
                "message or modal exceeds 40 components",
            ));
        }
        Ok(())
    }
}

/// Item-level description/spoiler, falling back to the ones an author put on
/// a remote media source.
fn media_extras(
    media: &MediaSource,
    description: &Option<String>,
    spoiler: bool,
) -> (Option<String>, bool) {
    match media {
        MediaSource::Remote {
            description: inner,
            spoiler: inner_spoiler,
            ..
        } => (
            description.clone().or_else(|| inner.clone()),
            spoiler || *inner_spoiler,
        ),
        MediaSource::File { .. } => (description.clone(), spoiler),
    }
}

fn validate_media_description(description: Option<&str>, path: &str) -> Result<(), CompileError> {
    if description.is_some_and(|value| value.chars().count() > 1024) {
        return Err(CompileError::new(
            "invalid_text",
            format!("{path}.description"),
            "media description must contain at most 1024 characters",
        ));
    }
    Ok(())
}

fn omit_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, omit_nulls(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(omit_nulls).collect()),
        other => other,
    }
}

fn validate_document_header(version: &u32, id: &LogicalId, path: &str) -> Result<(), CompileError> {
    if *version != UI_VERSION {
        return Err(CompileError::new(
            "unsupported_version",
            "$.version",
            "document version is unsupported",
        ));
    }
    validate_logical_id(&id.0).map_err(|error| prefix_path(error, path))
}

fn validate_logical_id(id: &str) -> Result<(), CompileError> {
    if id.is_empty()
        || id.chars().count() > MAX_LOGICAL_ID_CHARS
        || id.chars().any(|ch| ch.is_control())
    {
        return Err(CompileError::new(
            "invalid_logical_id",
            "$.logical_id",
            "logical id must be 1..=100 printable characters",
        ));
    }
    Ok(())
}

fn validate_values(min: Option<u32>, max: Option<u32>, path: &str) -> Result<(), CompileError> {
    if min.is_some_and(|value| value > 25)
        || max.is_some_and(|value| value > 25)
        || min.zip(max).is_some_and(|(min, max)| min > max)
    {
        return Err(CompileError::new(
            "invalid_values",
            path,
            "select values must satisfy 0 <= min <= max <= 25",
        ));
    }
    Ok(())
}

fn validate_choice(option: &ChoiceOption, path: &str) -> Result<(), CompileError> {
    if option.label.is_empty() || option.label.chars().count() > 100 {
        return Err(CompileError::new(
            "invalid_text",
            format!("{path}.label"),
            "choice label must contain 1..=100 characters",
        ));
    }
    if option.value.is_empty() || option.value.chars().count() > 100 {
        return Err(CompileError::new(
            "invalid_text",
            format!("{path}.value"),
            "choice value must contain 1..=100 characters",
        ));
    }
    if option
        .description
        .as_ref()
        .is_some_and(|value| value.chars().count() > 100)
    {
        return Err(CompileError::new(
            "invalid_text",
            format!("{path}.description"),
            "choice description is too long",
        ));
    }
    Ok(())
}

fn validate_option(option: &SelectOption, path: &str) -> Result<(), CompileError> {
    if option.label.is_empty() || option.label.chars().count() > 100 {
        return Err(CompileError::new(
            "invalid_text",
            format!("{path}.label"),
            "option label must contain 1..=100 characters",
        ));
    }
    if option.value.is_empty() || option.value.chars().count() > 100 {
        return Err(CompileError::new(
            "invalid_text",
            format!("{path}.value"),
            "option value must contain 1..=100 characters",
        ));
    }
    if option
        .description
        .as_ref()
        .is_some_and(|value| value.chars().count() > 100)
    {
        return Err(CompileError::new(
            "invalid_text",
            format!("{path}.description"),
            "option description is too long",
        ));
    }
    Ok(())
}

/// Media types a Discord media gallery will actually draw.
const RENDERABLE_TYPES: &[&str] = &["image/", "video/"];

/// Extensions that are certainly not an image or a video. Deliberately a
/// short deny-list: anything unknown is allowed through to Discord.
const UNRENDERABLE_EXTENSIONS: &[&str] = &[
    "pdf", "txt", "log", "md", "csv", "json", "zip", "gz", "bz2", "xz", "tar", "7z", "bin", "exe",
    "mp3", "ogg", "wav", "epub",
];

/// The lowercased extension when it is one Discord cannot draw, else `None`.
fn unrenderable_extension(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or("");
    let (_, extension) = last.rsplit_once('.')?;
    let extension = extension.to_ascii_lowercase();
    UNRENDERABLE_EXTENSIONS
        .contains(&extension.as_str())
        .then_some(extension)
}

fn validate_url(url: &str, path: &str) -> Result<(), CompileError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| CompileError::new("invalid_url", path, "URL is invalid"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(CompileError::new(
            "invalid_url",
            path,
            "media and link URLs must be absolute HTTP(S) URLs",
        ));
    }
    let host = parsed
        .host_str()
        .unwrap_or_default()
        .trim_matches(|c| c == '[' || c == ']');
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return Err(CompileError::new(
            "invalid_url",
            path,
            "private hosts are not allowed",
        ));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        let private = match ip {
            IpAddr::V4(ip) => {
                ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
            }
            IpAddr::V6(ip) => {
                ip.is_loopback() || ip.is_unspecified() || (ip.segments()[0] & 0xfe00) == 0xfc00
            }
        };
        if private {
            return Err(CompileError::new(
                "invalid_url",
                path,
                "private hosts are not allowed",
            ));
        }
    }
    Ok(())
}

fn bounded_text(value: &str, path: &str) -> Result<String, CompileError> {
    if value.is_empty() || value.chars().count() > MAX_TEXT_DISPLAY_CHARS {
        return Err(CompileError::new(
            "invalid_text",
            path,
            "text display content must contain 1..=4000 characters",
        ));
    }
    Ok(value.to_owned())
}

fn button_style(style: ButtonStyle) -> u8 {
    match style {
        ButtonStyle::Primary => 1,
        ButtonStyle::Secondary => 2,
        ButtonStyle::Success => 3,
        ButtonStyle::Danger => 4,
        ButtonStyle::Link => 5,
        ButtonStyle::Premium => 6,
    }
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

fn prefix_path(mut error: CompileError, prefix: &str) -> CompileError {
    error.path = format!("{prefix}.{}", error.path);
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    pub(super) fn allocator() -> DeterministicAllocator {
        DeterministicAllocator::default().with_file(FileRef {
            id: "file-1".into(),
            name: "diagram.png".into(),
            media_type: "image/png".into(),
            size: 3,
            sha256: "hash".into(),
        })
    }

    fn entity(id: &str) -> EntitySelect {
        EntitySelect {
            logical_id: LogicalId::new(id),
            placeholder: None,
            default_values: Vec::new(),
            min_values: None,
            max_values: Some(1),
            required: false,
            disabled: false,
            channel_types: None,
        }
    }

    fn modal_control(kind: &str) -> ModalControl {
        match kind {
            "text_input" => ModalControl::TextInput(TextInput {
                logical_id: LogicalId::new("text"),
                style: TextInputStyle::Short,
                min_length: Some(0),
                max_length: Some(100),
                value: None,
                placeholder: None,
                required: false,
                disabled: false,
            }),
            "string_select" => ModalControl::StringSelect(StringSelect {
                logical_id: LogicalId::new("string"),
                placeholder: None,
                options: vec![SelectOption {
                    label: "A".into(),
                    value: "a".into(),
                    description: None,
                    emoji: None,
                    default: false,
                }],
                min_values: Some(1),
                max_values: Some(1),
                required: false,
                disabled: false,
            }),
            "user" => ModalControl::UserSelect(entity("user")),
            "role" => ModalControl::RoleSelect(entity("role")),
            "mentionable" => ModalControl::MentionableSelect(entity("mentionable")),
            "channel" => ModalControl::ChannelSelect(entity("channel")),
            "file_upload" => ModalControl::FileUpload(FileUpload {
                logical_id: LogicalId::new("file"),
                min_values: Some(1),
                max_values: Some(2),
                file_types: Some(vec!["png".into()]),
                required: false,
            }),
            "radio" => ModalControl::RadioGroup(RadioGroup {
                logical_id: LogicalId::new("radio"),
                options: vec![
                    ChoiceOption {
                        label: "A".into(),
                        value: "a".into(),
                        description: None,
                        default: false,
                    },
                    ChoiceOption {
                        label: "B".into(),
                        value: "b".into(),
                        description: None,
                        default: true,
                    },
                ],
                required: false,
                disabled: false,
            }),
            "checkbox_group" => ModalControl::CheckboxGroup(CheckboxGroup {
                logical_id: LogicalId::new("checks"),
                options: vec![ChoiceOption {
                    label: "A".into(),
                    value: "a".into(),
                    description: None,
                    default: false,
                }],
                min_values: Some(1),
                max_values: Some(1),
                required: false,
                disabled: false,
            }),
            _ => ModalControl::Checkbox(Checkbox {
                logical_id: LogicalId::new("check"),
                default: false,
                required: false,
                disabled: false,
            }),
        }
    }

    pub(super) fn message_document() -> MessageDocument {
        MessageDocument {
            version: 1,
            document_id: LogicalId::new("message"),
            visibility: Visibility::Public,
            lifecycle: Lifecycle::Static,
            components: vec![
                MessageNode::ActionRow(ActionRow {
                    children: vec![MessageNode::Button(Button {
                        logical_id: Some(LogicalId::new("run")),
                        label: "Run".into(),
                        style: ButtonStyle::Primary,
                        url: None,
                        sku_id: None,
                        emoji: None,
                        disabled: false,
                    })],
                }),
                MessageNode::Text(TextDisplay {
                    content: "Hello".into(),
                }),
                MessageNode::Section(Section {
                    children: vec![TextDisplay {
                        content: "Section".into(),
                    }],
                    accessory: SectionAccessory::Button(Button {
                        logical_id: Some(LogicalId::new("section-button")),
                        label: "Go".into(),
                        style: ButtonStyle::Secondary,
                        url: None,
                        sku_id: None,
                        emoji: None,
                        disabled: false,
                    }),
                }),
                MessageNode::Section(Section {
                    children: vec![TextDisplay {
                        content: "With image".into(),
                    }],
                    accessory: SectionAccessory::Thumbnail(Thumbnail {
                        media: MediaSource::Remote {
                            url: "https://example.com/image.png".into(),
                            description: None,
                            spoiler: false,
                        },
                        description: None,
                        spoiler: false,
                    }),
                }),
                MessageNode::MediaGallery(MediaGallery {
                    items: vec![GalleryItem {
                        media: MediaSource::File {
                            file_id: "file-1".into(),
                        },
                        description: None,
                        spoiler: false,
                    }],
                }),
                MessageNode::File(FileComponent {
                    media: MediaSource::File {
                        file_id: "file-1".into(),
                    },
                    name: Some("diagram.png".into()),
                }),
                MessageNode::Separator(Separator {
                    divider: Some(true),
                    spacing: Some(SeparatorSpacing::Small),
                }),
                MessageNode::Container(Container {
                    accent_color: Some(0x5865F2),
                    spoiler: false,
                    components: vec![MessageNode::Text(TextDisplay {
                        content: "Nested".into(),
                    })],
                }),
                MessageNode::ActionRow(ActionRow {
                    children: vec![MessageNode::StringSelect(StringSelect {
                        logical_id: LogicalId::new("select"),
                        placeholder: None,
                        options: vec![SelectOption {
                            label: "A".into(),
                            value: "a".into(),
                            description: None,
                            emoji: None,
                            default: false,
                        }],
                        min_values: Some(1),
                        max_values: Some(1),
                        required: false,
                        disabled: false,
                    })],
                }),
            ],
        }
    }

    #[test]
    fn compiler_emits_all_message_type_numbers() {
        let mut allocator = allocator();
        let mut context = CompileContext::new(Origin::Plugin, &mut allocator);
        let compiled = compile_message(&message_document(), &mut context).unwrap();
        let mut types = Vec::new();
        fn collect(value: &Value, out: &mut Vec<u64>) {
            if let Some(kind) = value.get("type").and_then(Value::as_u64) {
                out.push(kind);
            }
            if let Some(items) = value.get("components").and_then(Value::as_array) {
                for item in items {
                    collect(item, out);
                }
            }
            if let Some(accessory) = value.get("accessory") {
                collect(accessory, out);
            }
            if let Some(items) = value.get("items").and_then(Value::as_array) {
                for item in items {
                    if let Some(media) = item.get("media")
                        && let Some(url) = media.get("url")
                    {
                        let _ = url;
                    }
                }
            }
        }
        for component in &compiled.components {
            collect(component, &mut types);
        }
        for expected in [1, 2, 9, 10, 11, 12, 13, 14, 17] {
            assert!(
                types.contains(&expected),
                "missing type {expected}: {types:?}"
            );
        }
        assert_eq!(compiled.flags, IS_COMPONENTS_V2);
        // The wire output must satisfy the independent Discord validator.
        crate::channels::discord::render::validate_components(&compiled.components).unwrap();
        let wire = serde_json::to_string(&compiled.components).unwrap();
        assert!(wire.contains(r#""spacing":1"#), "{wire}");
        assert!(!wire.contains("required"), "{wire}");
    }

    #[test]
    fn compiler_emits_every_modal_type_number() {
        let controls = [
            "text_input",
            "string_select",
            "user",
            "role",
            "mentionable",
            "channel",
            "file_upload",
            "radio",
            "checkbox_group",
            "check",
        ]
        .into_iter()
        .map(|kind| {
            ModalNode::Label(Label {
                label: kind.into(),
                description: None,
                component: modal_control(kind),
            })
        })
        .collect();
        let document = ModalDocument {
            version: 1,
            document_id: LogicalId::new("modal"),
            title: "Input".into(),
            lifecycle: Lifecycle::Static,
            components: controls,
        };
        let mut allocator = allocator();
        let mut context = CompileContext::new(Origin::Plugin, &mut allocator);
        let compiled = compile_modal(&document, &mut context).unwrap();
        let mut types = Vec::new();
        fn collect(value: &Value, out: &mut Vec<u64>) {
            if let Some(kind) = value.get("type").and_then(Value::as_u64) {
                out.push(kind);
            }
            if let Some(component) = value.get("component") {
                collect(component, out);
            }
        }
        for component in &compiled.components {
            collect(component, &mut types);
        }
        for expected in [3, 4, 5, 6, 7, 8, 18, 19, 21, 22, 23] {
            assert!(
                types.contains(&expected),
                "missing type {expected}: {types:?}"
            );
        }
    }

    #[test]
    fn compiler_rejects_unresolved_file_and_plain_button_placement() {
        let mut ids2 = allocator();
        let mut context = CompileContext::new(Origin::Plugin, &mut ids2);
        let document = MessageDocument {
            components: vec![MessageNode::File(FileComponent {
                media: MediaSource::File {
                    file_id: "missing".into(),
                },
                name: None,
            })],
            ..message_document()
        };
        assert_eq!(
            compile_message(&document, &mut context).unwrap_err().code,
            "file_not_found"
        );
        let mut ids = allocator();
        let mut context = CompileContext::new(Origin::Plugin, &mut ids);
        let document = MessageDocument {
            components: vec![MessageNode::Button(Button {
                logical_id: Some(LogicalId::new("x")),
                label: "X".into(),
                style: ButtonStyle::Primary,
                url: None,
                sku_id: None,
                emoji: None,
                disabled: false,
            })],
            ..message_document()
        };
        assert_eq!(
            compile_message(&document, &mut context).unwrap_err().code,
            "invalid_placement"
        );
    }
}

#[cfg(test)]
mod negative_tests {
    use super::tests::{allocator, message_document};
    use super::*;

    fn button(id: &str) -> MessageNode {
        MessageNode::Button(Button {
            logical_id: Some(LogicalId::new(id)),
            label: "Go".into(),
            style: ButtonStyle::Primary,
            url: None,
            sku_id: None,
            emoji: None,
            disabled: false,
        })
    }

    fn select(id: &str) -> MessageNode {
        MessageNode::StringSelect(StringSelect {
            logical_id: LogicalId::new(id),
            placeholder: None,
            options: vec![SelectOption {
                label: "A".into(),
                value: "a".into(),
                description: None,
                emoji: None,
                default: false,
            }],
            min_values: Some(1),
            max_values: Some(1),
            required: false,
            disabled: false,
        })
    }

    #[test]
    fn duplicate_logical_ids_and_mixed_rows_fail_closed() {
        let document = MessageDocument {
            version: 1,
            document_id: LogicalId::new("negative"),
            visibility: Visibility::Public,
            lifecycle: Lifecycle::Static,
            components: vec![MessageNode::ActionRow(ActionRow {
                children: vec![button("same"), button("same")],
            })],
        };
        let mut allocator = DeterministicAllocator::default();
        let error = compile_message(
            &document,
            &mut CompileContext::new(Origin::Agent, &mut allocator),
        )
        .unwrap_err();
        assert_eq!(error.code, "duplicate_logical_id");

        let document = MessageDocument {
            version: 1,
            document_id: LogicalId::new("negative-row"),
            visibility: Visibility::Public,
            lifecycle: Lifecycle::Static,
            components: vec![MessageNode::ActionRow(ActionRow {
                children: vec![select("one"), select("two")],
            })],
        };
        let mut allocator = DeterministicAllocator::default();
        let error = compile_message(
            &document,
            &mut CompileContext::new(Origin::Agent, &mut allocator),
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_row");
    }

    #[test]
    fn private_urls_and_long_choices_fail_before_wire_json() {
        let mut document = MessageDocument {
            version: 1,
            document_id: LogicalId::new("negative-url"),
            visibility: Visibility::Public,
            lifecycle: Lifecycle::Static,
            components: vec![MessageNode::MediaGallery(MediaGallery {
                items: vec![GalleryItem {
                    media: MediaSource::Remote {
                        url: "http://127.0.0.1/image.png".into(),
                        description: None,
                        spoiler: false,
                    },
                    description: None,
                    spoiler: false,
                }],
            })],
        };
        let mut allocator = DeterministicAllocator::default();
        let error = compile_message(
            &document,
            &mut CompileContext::new(Origin::Agent, &mut allocator),
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_url");

        document.components = vec![MessageNode::ActionRow(ActionRow {
            children: vec![MessageNode::StringSelect(StringSelect {
                logical_id: LogicalId::new("long"),
                placeholder: None,
                options: vec![SelectOption {
                    label: "x".repeat(101),
                    value: "x".into(),
                    description: None,
                    emoji: None,
                    default: false,
                }],
                min_values: None,
                max_values: None,
                required: false,
                disabled: false,
            })],
        })];
        let mut allocator = DeterministicAllocator::default();
        let error = compile_message(
            &document,
            &mut CompileContext::new(Origin::Agent, &mut allocator),
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_text");
    }

    /// Discord draws images and video in a media gallery; every other media
    /// type arrives as a broken "Image failed to load" card, so the gallery
    /// arm refuses it while the author can still read the path.
    #[test]
    fn gallery_refuses_media_it_cannot_draw() {
        let mut allocator = DeterministicAllocator::default()
            .with_file(FileRef {
                id: "pdf-1".into(),
                name: "deck.pdf".into(),
                media_type: "application/pdf".into(),
                size: 10,
                sha256: "hash".into(),
            })
            .with_file(FileRef {
                id: "zip-1".into(),
                name: "bundle.zip".into(),
                media_type: "application/octet-stream".into(),
                size: 10,
                sha256: "hash".into(),
            });
        for file_id in ["pdf-1", "zip-1"] {
            let mut context = CompileContext::new(Origin::Agent, &mut allocator);
            let document = MessageDocument {
                components: vec![MessageNode::MediaGallery(MediaGallery {
                    items: vec![GalleryItem {
                        media: MediaSource::File {
                            file_id: file_id.into(),
                        },
                        description: None,
                        spoiler: false,
                    }],
                })],
                ..message_document()
            };
            let error = compile_message(&document, &mut context).unwrap_err();
            assert_eq!(error.code, "invalid_gallery");
            assert_eq!(error.path, "$.components[0].items[0].media");
            assert!(
                error.message.contains("use a file component"),
                "message should name the fix: {}",
                error.message
            );
        }

        // a remote URL is judged only by a known-bad extension
        let mut context = CompileContext::new(Origin::Agent, &mut allocator);
        let document = MessageDocument {
            components: vec![MessageNode::MediaGallery(MediaGallery {
                items: vec![GalleryItem {
                    media: MediaSource::Remote {
                        url: "https://example.com/deck.pdf?v=2".into(),
                        description: None,
                        spoiler: false,
                    },
                    description: None,
                    spoiler: false,
                }],
            })],
            ..message_document()
        };
        assert_eq!(
            compile_message(&document, &mut context).unwrap_err().code,
            "invalid_gallery"
        );
    }

    #[test]
    fn gallery_accepts_images_video_and_unknown_urls() {
        let mut allocator = allocator();
        let mut context = CompileContext::new(Origin::Agent, &mut allocator);
        let remote = |url: &str| GalleryItem {
            media: MediaSource::Remote {
                url: url.into(),
                description: None,
                spoiler: false,
            },
            description: None,
            spoiler: false,
        };
        let document = MessageDocument {
            components: vec![MessageNode::MediaGallery(MediaGallery {
                items: vec![
                    GalleryItem {
                        media: MediaSource::File {
                            file_id: "file-1".into(),
                        },
                        description: None,
                        spoiler: false,
                    },
                    remote("https://example.com/clip.mp4"),
                    // no extension: unknown, so Discord gets to decide
                    remote("https://example.com/render/9f2a"),
                ],
            })],
            ..message_document()
        };
        let compiled = compile_message(&document, &mut context)
            .expect("images, video and extension-less URLs still compile");
        let gallery = compiled
            .components
            .iter()
            .find(|component| component.get("type").and_then(Value::as_u64) == Some(12))
            .expect("gallery compiled");
        assert_eq!(gallery["items"].as_array().map(Vec::len), Some(3));
    }
}
