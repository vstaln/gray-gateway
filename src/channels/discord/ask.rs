//! Questions on Discord: the bridge's surface for gray's `host/ask`.
//!
//! gray has no question tool of its own. When a questions plugin (such as
//! gray-questions' `request_user_input`) asks through `host/ask`, gray in
//! `--json` mode with `GRAY_JSON_ASK=1` hands the question to the bridge as
//! an `ask` row and waits for the answer on stdin. The bridge posts a
//! question card in the turn's channel, plain text, Components V2:
//!
//! ```text
//! ┃ -# Question from gray
//! ┃ **Branch** · Which branch should I deploy?
//! ┃ [main] [staging] [Other…]                ← buttons (up to 4 options)
//! ┃ ───────────────────────────────────────
//! ┃ -# Waiting for your answer · expires in 5 minutes · or reply in this channel
//! ```
//!
//! Up to 4 options are buttons; more, or a multiple choice, is a select
//! menu. "Other…" (or "Answer…" when there are no options) opens a modal
//! with a text box. A plain message in the channel answers every open
//! question too. The gateway records presses in the store and redraws the
//! card in the same interaction response; the runner's ask task polls the
//! store and writes the answers back to gray.
//!
//! Button `custom_id`s are `ask:<ask id>:<question>:<choice>`. The ask id is
//! 64 random bits; a press is honored only on an open card, in its own
//! channel, from an admitted user (the gateway admits before routing).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How long a card waits for an answer: under gray's 300-second ask budget,
/// so the card closes (and says so) before gray gives up on its own.
pub const ASK_SECS: u64 = 280;
const MAX_QUESTIONS: usize = 3;
const MAX_OPTIONS: usize = 25;
/// Up to this many options render as buttons (plus "Other…" fills a row).
const BUTTON_OPTIONS: usize = 4;

const OPEN: u32 = 0xFEE75C;
const ANSWERED: u32 = 0x57F287;
const CLOSED: u32 = 0x80848E;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub header: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<AskOption>,
    /// Allow picking several options.
    #[serde(default)]
    pub multiple: bool,
}

/// The questions in an `ask` row, fitted to Discord's limits: at most 3
/// questions and 25 options, labels and texts shortened rather than
/// refused (the asking plugin already validated them), ids kept as sent.
pub fn from_host(questions: &Value) -> Vec<Question> {
    let parsed: Vec<Question> = serde_json::from_value(questions.clone()).unwrap_or_default();
    let cut = |text: &str, max: usize| -> String { text.trim().chars().take(max).collect() };
    parsed
        .into_iter()
        .filter(|q| !q.id.is_empty() && !q.question.trim().is_empty())
        .take(MAX_QUESTIONS)
        .map(|q| Question {
            id: q.id,
            header: cut(&q.header, 45),
            question: cut(&q.question, 1000),
            options: q
                .options
                .into_iter()
                .filter(|option| !option.label.trim().is_empty())
                .take(MAX_OPTIONS)
                .map(|option| AskOption {
                    label: cut(&option.label, 80),
                    description: cut(&option.description, 100),
                })
                .collect(),
            multiple: q.multiple,
        })
        .collect()
}

/// A press on a question card, from its `custom_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct Press {
    pub ask_id: String,
    pub question: usize,
    pub choice: Choice,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Option(usize),
    Select,
    /// Open the text box.
    Other,
    /// The text box was submitted.
    Note,
}

pub fn parse_press(custom_id: &str) -> Option<Press> {
    let mut parts = custom_id.strip_prefix("ask:")?.split(':');
    let ask_id = parts.next()?.to_string();
    let question = parts.next()?.parse().ok()?;
    let choice = match parts.next()? {
        "select" => Choice::Select,
        "other" => Choice::Other,
        "note" => Choice::Note,
        index => Choice::Option(index.parse().ok()?),
    };
    if parts.next().is_some() || ask_id.is_empty() {
        return None;
    }
    Some(Press {
        ask_id,
        question,
        choice,
    })
}

/// Free text is marked the way gray-questions marks it.
pub fn note(text: &str) -> String {
    format!("user_note: {}", text.trim())
}

/// The answers for one press, or `None` for an unknown option.
pub fn answers_for(question: &Question, choice: &Choice, values: &[String]) -> Option<Vec<String>> {
    match choice {
        Choice::Option(index) => question
            .options
            .get(*index)
            .map(|option| vec![option.label.clone()]),
        Choice::Select => {
            let picked: Vec<String> = values
                .iter()
                .filter_map(|value| value.parse::<usize>().ok())
                .filter_map(|index| question.options.get(index))
                .map(|option| option.label.clone())
                .collect();
            (!picked.is_empty()).then_some(picked)
        }
        Choice::Other | Choice::Note => None,
    }
}

/// The question card for `row` as it stands.
pub fn render(row: &crate::channels::discord::durable::AskRow) -> Vec<Value> {
    let questions: Vec<Question> =
        serde_json::from_value(row.questions.clone()).unwrap_or_default();
    let open = row.state == "open";
    let accent = match row.state.as_str() {
        "open" => OPEN,
        "answered" => ANSWERED,
        _ => CLOSED,
    };
    let heading = if questions.len() == 1 {
        "-# Question from gray"
    } else {
        "-# Questions from gray"
    };
    let mut children = vec![json!({"type": 10, "content": heading})];
    for (index, q) in questions.iter().enumerate() {
        if index > 0 {
            children.push(json!({"type": 14, "divider": false, "spacing": 1}));
        }
        let title = if q.header.trim().is_empty() {
            format!("**{}**", q.question.trim())
        } else {
            format!("**{}** · {}", q.header.trim(), q.question.trim())
        };
        children.push(json!({"type": 10, "content": title}));
        let answered = row.answers.get(&q.id).and_then(Value::as_array);
        if let Some(answers) = answered {
            let shown: Vec<String> = answers
                .iter()
                .filter_map(Value::as_str)
                .map(|answer| {
                    answer
                        .strip_prefix("user_note: ")
                        .unwrap_or(answer)
                        .to_string()
                })
                .collect();
            children
                .push(json!({"type": 10, "content": format!("-# Answer: {}", shown.join(", "))}));
            continue;
        }
        if !open {
            continue;
        }
        let buttons = q.options.len() <= BUTTON_OPTIONS && !q.multiple;
        let described: Vec<String> = q
            .options
            .iter()
            .filter(|option| buttons && !option.description.trim().is_empty())
            .map(|option| {
                format!(
                    "-# **{}**: {}",
                    option.label.trim(),
                    option.description.trim()
                )
            })
            .collect();
        if !described.is_empty() {
            children.push(json!({"type": 10, "content": described.join("\n")}));
        }
        let id = |choice: &str| format!("ask:{}:{index}:{choice}", row.ask_id);
        let other_label = if q.options.is_empty() {
            "Answer…"
        } else {
            "Other…"
        };
        let other = json!({"type": 2, "style": 2, "label": other_label, "custom_id": id("other")});
        if buttons {
            let mut row_buttons: Vec<Value> = q
                .options
                .iter()
                .enumerate()
                .map(|(o, option)| {
                    json!({"type": 2, "style": 2, "label": option.label.trim(), "custom_id": id(&o.to_string())})
                })
                .collect();
            row_buttons.push(other);
            children.push(json!({"type": 1, "components": row_buttons}));
        } else {
            let options: Vec<Value> = q
                .options
                .iter()
                .enumerate()
                .map(|(o, option)| {
                    let mut item = json!({"label": option.label.trim(), "value": o.to_string()});
                    if !option.description.trim().is_empty() {
                        item["description"] = json!(option.description.trim());
                    }
                    item
                })
                .collect();
            let max = if q.multiple { q.options.len() } else { 1 };
            children.push(json!({"type": 1, "components": [{
                "type": 3,
                "custom_id": id("select"),
                "placeholder": if q.multiple { "Choose any…" } else { "Choose one…" },
                "options": options,
                "min_values": 1,
                "max_values": max,
            }]}));
            children.push(json!({"type": 1, "components": [other]}));
        }
    }
    let footer = match row.state.as_str() {
        "open" => format!(
            "-# Waiting for your answer · expires <t:{}:R> · or reply in this channel",
            row.expires as i64
        ),
        "answered" => "-# Answered".to_string(),
        "expired" => "-# No answer in time; gray went ahead on its own judgement".to_string(),
        _ => "-# The turn ended before an answer".to_string(),
    };
    children.push(json!({"type": 14, "divider": true, "spacing": 1}));
    children.push(json!({"type": 10, "content": footer}));
    vec![json!({"type": 17, "accent_color": accent, "components": children})]
}

/// The modal for "Other…" / "Answer…": one paragraph text box.
pub fn note_modal(
    row: &crate::channels::discord::durable::AskRow,
    question: usize,
) -> Option<Value> {
    let questions: Vec<Question> = serde_json::from_value(row.questions.clone()).ok()?;
    let q = questions.get(question)?;
    let label: String = if q.header.trim().is_empty() {
        "Your answer".to_string()
    } else {
        q.header.trim().chars().take(45).collect()
    };
    let description: String = q.question.trim().chars().take(100).collect();
    Some(json!({
        "custom_id": format!("ask:{}:{question}:note", row.ask_id),
        "title": "Answer gray",
        "components": [{
            "type": 18,
            "label": label,
            "description": description,
            "component": {
                "type": 4,
                "custom_id": "note",
                "style": 2,
                "min_length": 1,
                "max_length": 1000,
            },
        }],
    }))
}

/// Post the card in `channel` and wait for the answer: until every
/// question has one, `wait` runs out (the card closes as expired), or the
/// turn is over (`turn_over`, closed as cancelled). Returns what gray's
/// `--json` ask wants back: answers by question id, `{"<id>": ["<label>"]}`
/// (typed answers start with `user_note: `); empty when nobody answered.
pub async fn ask(
    rest: &crate::channels::discord::transport::Rest,
    store: &crate::channels::discord::durable::Store,
    channel: u64,
    questions: &[Question],
    wait: std::time::Duration,
    poll: std::time::Duration,
    turn_over: &std::sync::atomic::AtomicBool,
) -> Value {
    if questions.is_empty() {
        return json!({});
    }
    let ask_id = crate::channels::discord::durable::uuid_hex()[..16].to_string();
    let expires = crate::channels::discord::durable::now_secs() + wait.as_secs_f64();
    let row = match store
        .ask_create(&ask_id, &channel.to_string(), &json!(questions), expires)
        .and_then(|()| store.ask_get(&ask_id))
    {
        Ok(Some(row)) => row,
        _ => return json!({}),
    };
    let message = match rest.send_v2(channel, &render(&row), None).await {
        Ok(id) => id,
        Err(error) => {
            crate::channels::discord::eprintln_stamped!("[discord] question card refused: {error}");
            let _ = store.ask_close(&ask_id, "cancelled");
            return json!({});
        }
    };
    let _ = store.ask_set_message(&ask_id, &message);
    let deadline = tokio::time::Instant::now() + wait;
    let row = loop {
        tokio::time::sleep(poll).await;
        let current = store.ask_get(&ask_id).ok().flatten();
        if let Some(row) = current.as_ref().filter(|row| row.state != "open") {
            break row.clone();
        }
        let closing = if turn_over.load(std::sync::atomic::Ordering::Relaxed) {
            Some("cancelled")
        } else if tokio::time::Instant::now() >= deadline {
            Some("expired")
        } else {
            None
        };
        if let Some(state) = closing {
            match store.ask_close(&ask_id, state) {
                Ok(Some(row)) => {
                    let _ = rest.edit_v2(channel, &message, &render(&row)).await;
                    break row;
                }
                // Answered in the same instant: take the answer.
                _ => match store.ask_get(&ask_id).ok().flatten() {
                    Some(row) => break row,
                    None => return json!({}),
                },
            }
        }
    };
    row.answers
}

/// A plain message in a channel with an open card answers every question
/// still open on it, in the person's own words. True when the message was
/// taken as an answer (and must not start a turn of its own).
pub async fn answer_typed(
    store: &crate::channels::discord::durable::Store,
    rest: &crate::channels::discord::transport::Rest,
    channel: &str,
    text: &str,
) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let now = crate::channels::discord::durable::now_secs();
    let Ok(Some(row)) = store.ask_open_in(channel, now) else {
        return false;
    };
    let questions: Vec<Question> =
        serde_json::from_value(row.questions.clone()).unwrap_or_default();
    let mut updated = None;
    for q in questions
        .iter()
        .filter(|q| row.answers.get(&q.id).is_none())
    {
        if let Ok(Some(next)) = store.ask_answer(&row.ask_id, &q.id, &[note(text)], now) {
            updated = Some(next);
        }
    }
    let Some(updated) = updated else {
        return false;
    };
    if let (Some(message), Ok(ch)) = (updated.message_id.as_deref(), channel.parse::<u64>()) {
        let _ = rest.edit_v2(ch, message, &render(&updated)).await;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::discord::durable::AskRow;

    fn row(questions: Value, answers: Value, state: &str) -> AskRow {
        AskRow {
            ask_id: "abc123".into(),
            channel: "42".into(),
            message_id: None,
            questions,
            answers,
            state: state.into(),
            expires: 1_700_000_300.0,
        }
    }

    fn branch() -> Value {
        json!([{
            "id": "branch", "header": "Branch", "question": "Which branch should I deploy?",
            "options": [{"label": "main", "description": "production"}, {"label": "staging"}]
        }])
    }

    #[test]
    fn a_few_options_are_buttons_plus_other() {
        let card = render(&row(branch(), json!({}), "open"));
        crate::channels::discord::render::validate_components(&card).unwrap();
        let text = card[0].to_string();
        assert!(text.contains("**Branch** · Which branch should I deploy?"));
        assert!(text.contains("-# **main**: production"));
        assert!(text.contains("\"custom_id\":\"ask:abc123:0:0\""));
        assert!(text.contains("\"custom_id\":\"ask:abc123:0:1\""));
        assert!(text.contains("\"label\":\"Other…\""));
        assert!(text.contains("expires <t:1700000300:R>"));
        assert_eq!(card[0]["accent_color"], json!(OPEN));
        assert!(!text.contains("emoji"), "plain text, no emojis");
    }

    #[test]
    fn many_options_or_a_multiple_choice_use_a_select() {
        let options: Vec<Value> = (0..6).map(|i| json!({"label": format!("o{i}")})).collect();
        let qs = json!([{"id": "pick", "question": "Pick", "options": options, "multiple": true}]);
        let card = render(&row(qs, json!({}), "open"));
        crate::channels::discord::render::validate_components(&card).unwrap();
        let select = card[0]["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["type"] == 1 && c["components"][0]["type"] == 3)
            .unwrap();
        assert_eq!(select["components"][0]["max_values"], 6);
        assert_eq!(select["components"][0]["custom_id"], "ask:abc123:0:select");
    }

    #[test]
    fn an_answered_card_shows_answers_and_no_controls() {
        let card = render(&row(
            branch(),
            json!({"branch": ["user_note: the hotfix branch"]}),
            "answered",
        ));
        crate::channels::discord::render::validate_components(&card).unwrap();
        let text = card[0].to_string();
        assert!(text.contains("-# Answer: the hotfix branch"));
        assert!(!text.contains("ask:abc123"), "no buttons left");
        assert_eq!(card[0]["accent_color"], json!(ANSWERED));
    }

    #[test]
    fn presses_parse_and_map_to_answers() {
        let press = parse_press("ask:abc123:0:1").unwrap();
        assert_eq!(press.choice, Choice::Option(1));
        let q: Vec<Question> = serde_json::from_value(branch()).unwrap();
        assert_eq!(
            answers_for(&q[0], &press.choice, &[]),
            Some(vec!["staging".into()])
        );
        assert_eq!(
            answers_for(&q[0], &Choice::Select, &["0".into(), "9".into()]),
            Some(vec!["main".into()])
        );
        assert_eq!(
            parse_press("ask:abc123:0:other").unwrap().choice,
            Choice::Other
        );
        assert!(parse_press("ask::0:1").is_none());
        assert!(parse_press("turn:stop:x").is_none());
    }

    #[test]
    fn the_text_box_modal_is_a_label_with_a_paragraph() {
        let modal = note_modal(&row(branch(), json!({}), "open"), 0).unwrap();
        assert_eq!(modal["custom_id"], "ask:abc123:0:note");
        assert_eq!(modal["components"][0]["type"], 18);
        assert_eq!(modal["components"][0]["component"]["style"], 2);
    }

    #[test]
    fn host_questions_are_fitted_to_discord_not_refused() {
        let long = "x".repeat(200);
        let qs = from_host(&json!([
            {"id": "a", "header": long, "question": "Pick", "is_other": true,
             "options": [{"label": long, "description": long}]},
            {"id": "", "question": "dropped: no id"},
            {"id": "b", "question": "two"}, {"id": "c", "question": "three"},
            {"id": "d", "question": "a fourth is one too many"}
        ]));
        assert_eq!(qs.len(), 3);
        assert_eq!(qs[0].header.chars().count(), 45);
        assert_eq!(qs[0].options[0].label.chars().count(), 80);
        assert_eq!(qs[0].options[0].description.chars().count(), 100);
        assert!(from_host(&json!("garbage")).is_empty());
    }
}
