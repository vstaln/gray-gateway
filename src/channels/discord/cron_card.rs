//! Cron delivery -> Discord Components V2 card.
//!
//! Pure: no I/O, no network, so every rule below is unit-testable. Depends
//! only on `serde_json` and the existing `render`/`text` helpers.
//!
//! Two entry points:
//! * [`CronCard::from_final_text`] - the structured path. Core sends the final
//!   assistant text (or the stored reminder text) as its own field, and it is
//!   used *literally*: no parsing, no whitespace normalisation. A reminder the
//!   user typed with three blank lines in it must arrive with three blank
//!   lines in it.
//! * [`CronCard::from_raw`] - safety net for an older core that still ships
//!   the `Cronjob Response:` frame and a tool transcript. [`parse`] strips it;
//!   it is lossy on purpose and is never used for a reminder that arrived
//!   through the structured path.

use serde_json::Value;
use std::time::Duration;

use crate::channels::discord::render::{self, container, separator, text_display};
use crate::channels::discord::text::utf16_len;

/// Body, title and plain-text budgets in UTF-16 code units — Discord's unit,
/// not `chars()`: 1900 astral emoji are 1900 chars and 3800 units, and a 400.
/// The body leaves room for the title and the footer inside one message's
/// 4000-unit text budget.
pub(crate) const MAX_BODY_UNITS: usize = 3400;
const MAX_TITLE_UNITS: usize = 80;
const MAX_PLAIN_UNITS: usize = 1900;
const REMINDER_ACCENT: u32 = 0xFEE75C;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Reminder,
    Task,
}

impl Kind {
    /// Wire value from `gray cron tick --json` (`"reminder"` / `"task"`).
    pub fn from_wire(s: Option<&str>) -> Self {
        match s {
            Some("reminder") => Kind::Reminder,
            _ => Kind::Task,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Ok,
    Failed,
}

impl Status {
    /// Wire value from core's real run outcome. Never guessed from text.
    pub fn from_wire(s: Option<&str>) -> Self {
        match s {
            Some("failed") | Some("error") => Status::Failed,
            _ => Status::Ok,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub job_name: Option<String>,
    pub body: String,
    /// Logging only, never shown in Discord.
    pub full_output: Option<String>,
}

const UNTRUSTED_OPEN: &str = "<untrusted-output>";
const UNTRUSTED_CLOSE: &str = "</untrusted-output>";

fn is_tool_marker(t: &str) -> bool {
    t.starts_with("[tool:")
}

fn is_result_marker(t: &str) -> bool {
    t.starts_with("[result:") || t.starts_with("[result-err:")
}

/// Safety net for an older core. The real fix is core not emitting this.
pub fn parse(raw: &str) -> Parsed {
    let mut name = None;
    let mut full_output = None;
    let mut out: Vec<&str> = Vec::new();
    let mut in_untrusted = false;
    let mut in_result = false;

    for line in raw.lines() {
        let t = line.trim();
        let norm = t.replace('_', "-");

        if in_untrusted {
            if norm.contains(UNTRUSTED_CLOSE) {
                in_untrusted = false;
                continue;
            }
            // A block cut open by a length cap never closes. The next
            // transcript marker ends it, so the final answer survives.
            if is_tool_marker(t) || is_result_marker(t) {
                in_untrusted = false;
            } else {
                continue;
            }
        }
        if in_result {
            // Old `[result:<raw output>]` payloads span lines and end at `]`.
            if t.ends_with(']') || is_tool_marker(t) {
                in_result = false;
            }
            continue;
        }
        if norm.contains(UNTRUSTED_OPEN) {
            if !norm.contains(UNTRUSTED_CLOSE) {
                in_untrusted = true;
            }
            continue;
        }
        if norm.contains(UNTRUSTED_CLOSE) {
            continue; // stray closer from an unbalanced transcript
        }
        if is_tool_marker(t) {
            continue;
        }
        if is_result_marker(t) {
            if !t.ends_with(']') {
                in_result = true;
            }
            continue;
        }

        if let Some(rest) = t.strip_prefix("Cronjob Response:") {
            name = Some(rest.trim().to_string());
            continue;
        }
        if t.starts_with("(job_id:") && t.ends_with(')') {
            continue;
        }
        let has_content = out.iter().any(|l| !l.trim().is_empty());
        if !has_content && t.len() >= 3 && t.chars().all(|c| c == '-') {
            continue;
        }

        if t.starts_with("To stop or manage this job") {
            continue;
        }
        if let Some(rest) = t.strip_prefix("Full output:") {
            full_output = Some(rest.trim().to_string());
            continue;
        }
        out.push(line);
    }

    Parsed {
        job_name: name.filter(|n| !n.is_empty()),
        body: collapse_blank_lines(&out.join("\n")),
        full_output,
    }
}

fn collapse_blank_lines(s: &str) -> String {
    let mut res = String::new();
    let mut blanks = 0;
    for line in s.trim().lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        res.push_str(line.trim_end());
        res.push('\n');
    }
    res.trim_end().to_string()
}

/// Cut to at most `max` UTF-16 units and close a code fence the cut opened.
fn truncate(s: &str, max: usize) -> String {
    if utf16_len(s) <= max {
        return s.to_string();
    }
    let mut cut = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = c.len_utf16();
        if used + w + 1 > max {
            break;
        }
        used += w;
        cut.push(c);
    }
    let mut out = format!("{}…", cut.trim_end());
    if out.matches("```").count() % 2 == 1 {
        out.push_str("\n```");
    }
    out
}

fn fmt_elapsed(d: Duration) -> String {
    // Round first, then pick the unit: 59.96s must not print as "60.0s".
    let tenths = (d.as_millis() + 50) / 100;
    if tenths < 600 {
        format!("{}.{}s", tenths / 10, tenths % 10)
    } else {
        let s = (tenths / 10) as u64;
        format!("{}m {}s", s / 60, s % 60)
    }
}

pub struct CronCard {
    pub kind: Kind,
    pub status: Status,
    pub parsed: Parsed,
    pub elapsed: Option<Duration>,
    pub schedule: Option<String>, // "once", "every 2h", ...
}

impl CronCard {
    /// Legacy path: `raw` may carry the old frame and a tool transcript.
    pub fn from_raw(raw: &str, kind: Kind, status: Status) -> Self {
        Self {
            kind,
            status,
            parsed: parse(raw),
            elapsed: None,
            schedule: None,
        }
    }

    /// Structured path: `text` is the final assistant message or the stored
    /// reminder text. Used literally (byte for byte, only length-capped).
    pub fn from_final_text(name: Option<String>, text: &str, kind: Kind, status: Status) -> Self {
        Self {
            kind,
            status,
            parsed: Parsed {
                job_name: name.filter(|n| !n.trim().is_empty()),
                body: text.to_string(),
                full_output: None,
            },
            elapsed: None,
            schedule: None,
        }
    }

    /// Card head shared by the single-shot and paged renders: title line,
    /// accent color, and the `-#` meta footer text.
    pub(crate) fn head(&self) -> (String, u32, String) {
        let name = truncate(
            &self
                .parsed
                .job_name
                .clone()
                .unwrap_or_else(|| "cron job".into())
                .replace('\n', " "),
            MAX_TITLE_UNITS,
        );
        let (icon, title, color) = match (self.status, self.kind) {
            (Status::Failed, _) => ("⚠️", format!("{name} failed"), render::DANGER_ACCENT),
            (_, Kind::Reminder) => ("⏰", "Reminder".to_string(), REMINDER_ACCENT),
            (_, Kind::Task) => ("🕒", name, render::INFO_ACCENT),
        };
        let mut meta = vec![match self.status {
            Status::Ok => "✅ done".to_string(),
            Status::Failed => "❌ failed".to_string(),
        }];
        if let Some(e) = self.elapsed {
            meta.push(fmt_elapsed(e));
        }
        if let Some(s) = &self.schedule {
            meta.push(s.clone());
        }
        (
            format!("{icon} {title}"),
            color,
            format!("-# {}", meta.join(" · ")),
        )
    }

    /// A body too long for one card becomes `pages[cur]` plus the footer
    /// and a `pg:` nav row; page turns keep the footer via the pager's
    /// trailing-component carry-over.
    pub fn paged_components(&self, token: &str, pages: &[String], cur: usize) -> Vec<Value> {
        let (title, color, meta) = self.head();
        let page = pages.get(cur).cloned().unwrap_or_default();
        let mut out = vec![
            crate::channels::discord::pager::container(&page, &title, color),
            separator(),
            text_display(meta),
        ];
        if let Some(row) = crate::channels::discord::pager::nav_row(token, cur, pages.len()) {
            out.push(row);
        }
        out
    }

    /// The component array (one Container), or `None` for an empty body.
    pub fn components(&self) -> Option<Vec<Value>> {
        if self.parsed.body.trim().is_empty() {
            return None;
        }

        let (title, color, meta) = self.head();
        let body = truncate(&self.parsed.body, MAX_BODY_UNITS);

        Some(vec![container(
            vec![
                text_display(format!("### {title}")),
                separator(),
                text_display(body),
                separator(),
                text_display(meta),
            ],
            Some(color),
        )])
    }

    /// Plain text that always fits Discord's 2000-unit limit.
    pub fn plain_text(&self) -> String {
        let b = self.parsed.body.trim();
        if b.is_empty() {
            "(no output)".to_string()
        } else {
            truncate(b, MAX_PLAIN_UNITS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::discord::render::validate_components;

    const SAMPLE: &str = "Cronjob Response: clean-room\n(job_id: 8d90cd866db9)\n-------------\n\n\n[tool:bash]\n\n[result:exit 0 · 0.0s · 5 lines]\n<untrusted-output>\ntotal 16\n---\n</untrusted-output>\n\nTime to clean your room.\n\nTo stop or manage this job, send me a new message (e.g. \"stop reminder clean-room\").\n\nFull output: /home/x/.config/gray-discord/conversations/a/cron/output/8d90cd866db9.md";

    /// The tree the transport will actually validate before sending.
    fn built(raw: &str, kind: Kind, status: Status) -> String {
        let c = CronCard::from_raw(raw, kind, status);
        let components = c.components().expect("non-empty body");
        validate_components(&components).expect("the card must pass the wire validator");
        serde_json::to_string(&components).unwrap()
    }

    #[test]
    fn strips_header_tools_footer() {
        let p = parse(SAMPLE);
        assert_eq!(p.job_name.as_deref(), Some("clean-room"));
        assert_eq!(p.body, "Time to clean your room.");
        assert!(p.full_output.unwrap().ends_with(".md"));
    }

    #[test]
    fn no_job_id_or_transcript_in_the_payload() {
        let s = built(SAMPLE, Kind::Reminder, Status::Ok);
        for bad in [
            "8d90cd866db9",
            "job_id",
            "Cronjob Response",
            "[tool:",
            "untrusted",
            "Full output",
            "supervise",
        ] {
            assert!(!s.contains(bad), "leaked {bad}");
        }
    }

    #[test]
    fn structured_text_is_not_parsed_or_normalised() {
        // Looks like transcript noise and has odd whitespace: must survive.
        let text = "[tool: x]\n\n\n  keep   me  \n---";
        let components = CronCard::from_final_text(None, text, Kind::Reminder, Status::Ok)
            .components()
            .unwrap();
        assert_eq!(components[0]["components"][2]["content"], text);
    }

    #[test]
    fn failure_is_red() {
        let c = CronCard::from_raw("boom", Kind::Task, Status::Failed)
            .components()
            .unwrap();
        assert_eq!(c[0]["accent_color"], render::DANGER_ACCENT);
    }

    #[test]
    fn accents_match_kind() {
        let r = CronCard::from_raw("x", Kind::Reminder, Status::Ok)
            .components()
            .unwrap();
        let t = CronCard::from_raw("x", Kind::Task, Status::Ok)
            .components()
            .unwrap();
        assert_eq!(r[0]["accent_color"], REMINDER_ACCENT);
        assert_eq!(t[0]["accent_color"], render::INFO_ACCENT);
    }

    #[test]
    fn shape_is_container_title_sep_body_sep_footer() {
        let mut c = CronCard::from_raw("clean my roo", Kind::Reminder, Status::Ok);
        c.elapsed = Some(Duration::from_millis(300));
        c.schedule = Some("once".into());
        let kids = c.components().unwrap();
        assert_eq!(kids[0]["type"], 17);
        let kids = kids[0]["components"].as_array().unwrap();
        let types: Vec<u64> = kids.iter().map(|k| k["type"].as_u64().unwrap()).collect();
        assert_eq!(types, vec![10, 14, 10, 14, 10]);
        assert_eq!(kids[0]["content"], "### ⏰ Reminder");
        assert_eq!(kids[2]["content"], "clean my roo");
        assert_eq!(kids[4]["content"], "-# ✅ done · 0.3s · once");
    }

    #[test]
    fn truncates_and_closes_fence() {
        let big = format!("```\n{}", "x".repeat(5000));
        let t = truncate(&big, 100);
        assert!(utf16_len(&t) <= 104, "got {}", utf16_len(&t));
        assert!(t.ends_with("```"));
    }

    #[test]
    fn truncation_counts_utf16_units_not_chars() {
        // 1500 astral emoji are 1500 chars and 3000 UTF-16 units.
        let s = "😀".repeat(1500);
        let t = truncate(&s, MAX_PLAIN_UNITS);
        assert!(
            utf16_len(&t) <= MAX_PLAIN_UNITS + 4,
            "got {}",
            utf16_len(&t)
        );
        assert!(utf16_len(&CronCard::from_raw(&s, Kind::Task, Status::Ok).plain_text()) <= 2000);
    }

    #[test]
    fn body_is_capped_so_total_text_stays_under_4000() {
        let c = CronCard::from_raw(&"y".repeat(10_000), Kind::Task, Status::Ok);
        let components = c.components().unwrap();
        let total: usize = components[0]["components"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|k| k["type"] == 10)
            .map(|k| utf16_len(k["content"].as_str().unwrap()))
            .sum();
        assert!(total < 4000, "total text {total}");
    }

    #[test]
    fn empty_body_has_no_card_and_the_fallback_still_fits() {
        let c = CronCard::from_raw(
            "Cronjob Response: x\n(job_id: abc)\n-----\n",
            Kind::Task,
            Status::Ok,
        );
        assert!(c.components().is_none());
        assert_eq!(c.plain_text(), "(no output)");
        assert!(utf16_len(&c.plain_text()) <= 2000);
    }

    #[test]
    fn unclosed_untrusted_block_does_not_swallow_the_answer() {
        // Core used to cut tool output at 2000 chars, dropping the closer.
        let raw = "[tool:bash]\n[result:exit 0]\n<untrusted-output>\ndrwxr-xr-x 2 u u 4096 supervise\n[tool:bash]\n[result:exit 0]\nAll done.";
        assert_eq!(parse(raw).body, "All done.");
    }

    #[test]
    fn stray_closer_and_underscore_spelling_are_dropped() {
        let raw = "</untrusted-output>\n<untrusted_output>\nls\n</untrusted_output>\nhello";
        assert_eq!(parse(raw).body, "hello");
    }

    #[test]
    fn multiline_untagged_result_payload_is_dropped() {
        let raw = "[result:total 16\ndrwxr-xr-x 2 u u 4096 x\n]\nthe answer";
        assert_eq!(parse(raw).body, "the answer");
    }

    #[test]
    fn elapsed_rounds_before_choosing_the_unit() {
        assert_eq!(fmt_elapsed(Duration::from_millis(300)), "0.3s");
        assert_eq!(fmt_elapsed(Duration::from_millis(59_960)), "1m 0s");
        assert_eq!(fmt_elapsed(Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn status_and_kind_wire_values() {
        assert_eq!(Kind::from_wire(Some("reminder")), Kind::Reminder);
        assert_eq!(Kind::from_wire(None), Kind::Task);
        assert_eq!(Status::from_wire(Some("failed")), Status::Failed);
        assert_eq!(Status::from_wire(Some("ok")), Status::Ok);
        assert_eq!(Status::from_wire(None), Status::Ok);
    }
}
