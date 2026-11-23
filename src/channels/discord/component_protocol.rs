//! Gray's typed Discord Components V2 protocol.
//!
//! The model and plugin callers describe components in these types. Discord
//! numeric type values, `custom_id` values, and wire-only fields are emitted by
//! `component_compile`; they are never accepted from an agent document.

use serde::{Deserialize, Serialize};

pub const UI_PROTOCOL: &str = "gray.discord.ui";
pub const UI_VERSION: u32 = 1;
pub const MAX_COMPONENTS: usize = 40;
pub const MAX_SELECT_OPTIONS: usize = 25;
pub const MAX_FILE_TYPES: usize = 10;
pub const MAX_RADIO_OPTIONS: usize = 10;
pub const MAX_CHECKBOX_OPTIONS: usize = 10;
pub const MAX_LABEL_CHARS: usize = 45;
pub const MAX_DESCRIPTION_CHARS: usize = 100;
pub const MAX_TEXT_DISPLAY_CHARS: usize = 4000;
pub const MAX_MODAL_TITLE_CHARS: usize = 45;
pub const MAX_LOGICAL_ID_CHARS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Agent,
    #[default]
    Plugin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    #[default]
    Message,
    Modal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Public,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    #[default]
    Static,
    OneShot,
    Repeatable,
    Editable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct LogicalId(pub String);

impl LogicalId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageDocument {
    pub version: u32,
    pub document_id: LogicalId,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    pub components: Vec<MessageNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModalDocument {
    pub version: u32,
    pub document_id: LogicalId,
    pub title: String,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    pub components: Vec<ModalNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "surface", rename_all = "snake_case")]
pub enum UiDocument {
    Message(MessageDocument),
    Modal(ModalDocument),
}

impl UiDocument {
    pub fn surface(&self) -> Surface {
        match self {
            Self::Message(_) => Surface::Message,
            Self::Modal(_) => Surface::Modal,
        }
    }

    pub fn document_id(&self) -> &LogicalId {
        match self {
            Self::Message(document) => &document.document_id,
            Self::Modal(document) => &document.document_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ButtonStyle {
    #[default]
    Primary,
    Secondary,
    Success,
    Danger,
    Link,
    Premium,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Emoji {
    pub name: Option<String>,
    pub id: Option<String>,
    pub animated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Button {
    pub logical_id: Option<LogicalId>,
    #[serde(default)]
    pub label: String,
    pub style: ButtonStyle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sku_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<Emoji>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectOption {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<Emoji>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StringSelect {
    pub logical_id: LogicalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub options: Vec<SelectOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_values: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_values: Option<u32>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectDefault {
    pub id: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitySelect {
    pub logical_id: LogicalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub default_values: Vec<SelectDefault>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_values: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_values: Option<u32>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_types: Option<Vec<u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRow {
    pub children: Vec<MessageNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextDisplay {
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thumbnail {
    pub media: MediaSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub spoiler: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GalleryItem {
    pub media: MediaSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub spoiler: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaGallery {
    pub items: Vec<GalleryItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileComponent {
    pub media: MediaSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SeparatorSpacing {
    #[default]
    Small,
    Large,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Separator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub divider: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spacing: Option<SeparatorSpacing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub children: Vec<TextDisplay>,
    pub accessory: SectionAccessory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SectionAccessory {
    Button(Button),
    Thumbnail(Thumbnail),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Container {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<u32>,
    #[serde(default)]
    pub spoiler: bool,
    pub components: Vec<MessageNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageNode {
    ActionRow(ActionRow),
    Button(Button),
    StringSelect(StringSelect),
    UserSelect(EntitySelect),
    RoleSelect(EntitySelect),
    MentionableSelect(EntitySelect),
    ChannelSelect(EntitySelect),
    Section(Section),
    Text(TextDisplay),
    Thumbnail(Thumbnail),
    MediaGallery(MediaGallery),
    File(FileComponent),
    Separator(Separator),
    Container(Container),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TextInputStyle {
    #[default]
    Short,
    Paragraph,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextInput {
    pub logical_id: LogicalId,
    pub style: TextInputStyle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileUpload {
    pub logical_id: LogicalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_values: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_values: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_types: Option<Vec<String>>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceOption {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadioGroup {
    pub logical_id: LogicalId,
    pub options: Vec<ChoiceOption>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckboxGroup {
    pub logical_id: LogicalId,
    pub options: Vec<ChoiceOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_values: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_values: Option<u32>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkbox {
    pub logical_id: LogicalId,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModalControl {
    TextInput(TextInput),
    StringSelect(StringSelect),
    UserSelect(EntitySelect),
    RoleSelect(EntitySelect),
    MentionableSelect(EntitySelect),
    ChannelSelect(EntitySelect),
    FileUpload(FileUpload),
    RadioGroup(RadioGroup),
    CheckboxGroup(CheckboxGroup),
    Checkbox(Checkbox),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Label {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub component: ModalControl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModalActionRow {
    pub children: Vec<ModalControl>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModalNode {
    Label(Label),
    ActionRow(ModalActionRow),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaSource {
    Remote {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default)]
        spoiler: bool,
    },
    File {
        file_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRef {
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentRequest {
    pub document: UiDocument,
    pub origin: Origin,
}
