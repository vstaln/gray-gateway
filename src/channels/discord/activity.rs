//! Turn narration for chat surfaces.
//!
//! gray's `--json` progress rows are platform-agnostic: a tool name, a
//! redacted one-line detail, and (for completed tools) a bounded redacted
//! output. This module is the shared plumbing between the runner (which reads
//! those rows) and the gateway (which knows the channel): a bounded, scoped
//! sink plus pure renderers for the live status bubble and the persistent
//! tool-activity card.
//!
//! Both renderers follow the chat-bridge tool-progress feed: one line per call —
//! icon, tool, a bounded preview of the arguments, how long it took — and
//! **never the tool's output**. A chat surface is a status feed, not a
//! transcript; echoing results dumps file contents, memory snapshots and
//! credentials into the channel.
//!
//! The sink also carries gray's streamed `text` rows, in order with the tool
//! rows, so [`crate::channels::discord::stream`] can lay the turn out the way the bridge does. Text rows
//! stay out of the bounded history: they would crowd the tool rows the
//! optional end-of-turn card is built from.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::Value;

/// Bounded so a runaway turn cannot grow the queue without limit, but far
/// above what a turn queues between two frames: the gateway drains it four
/// times a second, and a dropped row is lost for good (a `text` delta is
/// append-only, so dropping one would garble the prose on screen).
pub const MAX_PENDING: usize = 4096;
/// Keep a bounded history for the final card even after the live rows have
/// been drained. This is intentionally larger than the pending queue so a
/// turn with many quick tools still produces a useful transcript.
const MAX_HISTORY: usize = 128;
/// The card lists actions, not transcripts: a handful of rows plus a count.
const MAX_CARD_ROWS: usize = 5;
/// Discord rejects a Text Display past 4000 chars (see render.rs); the card
/// stays well under.
const MAX_CARD_CHARS: usize = 3600;

/// Seconds between edits of a message showing tool blocks.
pub const TOOL_EDIT_GAP: f64 = 1.5;

/// Suggested cap for a tool preview line
/// (`display.tool_preview_length: 80`).
const MAX_PREVIEW_CHARS: usize = 80;

/// Seconds between edits of a prose message (Discord rate-limits edits).
/// Tool messages wait at least [`TOOL_EDIT_GAP`].
pub const MIN_EDIT_GAP: f64 = 0.8;

#[derive(Default)]
struct ScopeState {
    pending: VecDeque<Value>,
    history: VecDeque<Value>,
    /// When this turn started, for the card's elapsed tally. Set by `begin`.
    started: Option<Instant>,
    /// In-flight internal call ids -> when they started, so a line can carry
    /// the call's duration once it returns. gray's rows carry no clock.
    inflight: HashMap<String, Instant>,
}

#[derive(Default)]
pub struct SinkState {
    scopes: HashMap<String, ScopeState>,
}

pub type Sink = Arc<Mutex<SinkState>>;

pub fn sink() -> Sink {
    Arc::new(Mutex::new(SinkState::default()))
}

/// Narration on/off. Default on, same semantics as `typing_indicator`:
/// absent means enabled, only an explicit `false` turns it off.
pub fn enabled(config: &Value) -> bool {
    config
        .get("activity_indicator")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| !crate::channels::discord::stream::quiet(config))
}

/// The end-of-turn tally card (`⋯ 12.4s · ran 3 commands`). Off by default:
/// The bridge posts none, and the progress bubbles already stay in the channel
/// as the turn's record. `"activity_card": true` brings it back.
pub fn card_enabled(config: &Value) -> bool {
    config
        .get("activity_card")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Start a fresh history for one conversation. The daemon may have a stale
/// entry after a crash or a cancelled turn; never let it bleed into the next
/// turn's card.
pub fn begin(s: &Sink, scope: &str) {
    if let Ok(mut state) = s.lock() {
        state.scopes.insert(
            scope.to_string(),
            ScopeState {
                started: Some(Instant::now()),
                ..ScopeState::default()
            },
        );
    }
}

pub fn push(s: &Sink, row: Value) {
    push_for(s, "", row);
}

pub fn push_for(s: &Sink, scope: &str, row: Value) {
    let Ok(mut state) = s.lock() else {
        return;
    };
    let scope = state.scopes.entry(scope.to_string()).or_default();
    let mut row = row;
    stamp(&mut row, scope);
    if scope.pending.len() >= MAX_PENDING {
        scope.pending.pop_front();
    }
    scope.pending.push_back(row.clone());
    if matches!(
        row.get("phase").and_then(Value::as_str),
        Some("text") | Some("retract")
    ) {
        return;
    }
    if scope.history.len() >= MAX_HISTORY {
        scope.history.pop_front();
    }
    scope.history.push_back(row);
}

/// A narrated line carries how long the call took and the
/// card's tally carries how long the turn took. gray's rows have no clock,
/// so the sink measures between `tool_ran` and `tool_finished` for the same
/// internal call id.
fn stamp(row: &mut Value, scope: &mut ScopeState) {
    let phase = row.get("phase").and_then(Value::as_str).unwrap_or("");
    let call_id = row
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match phase {
        "tool_ran" => {
            if !call_id.is_empty() {
                scope.inflight.insert(call_id, Instant::now());
            }
        }
        "tool_finished" => {
            if !call_id.is_empty() {
                if let Some(started) = scope.inflight.remove(&call_id) {
                    row["elapsed_ms"] = millis(started.elapsed()).into();
                }
            }
            if let Some(turn) = scope.started {
                row["turn_ms"] = millis(turn.elapsed()).into();
            }
        }
        _ => {}
    }
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub fn drain(s: &Sink) -> Vec<Value> {
    drain_for(s, "")
}

pub fn drain_for(s: &Sink, scope: &str) -> Vec<Value> {
    let mut state = match s.lock() {
        Ok(state) => state,
        Err(_) => return Vec::new(),
    };
    state
        .scopes
        .get_mut(scope)
        .map(|scope| scope.pending.drain(..).collect())
        .unwrap_or_default()
}

/// Remove a completed turn and return its full bounded history. Removing the
/// scope makes final-card publication idempotent even if a caller flushes
/// twice.
pub fn finish(s: &Sink, scope: &str) -> Vec<Value> {
    let mut state = match s.lock() {
        Ok(state) => state,
        Err(_) => return Vec::new(),
    };
    state
        .scopes
        .remove(scope)
        .map(|scope| scope.history.into_iter().collect())
        .unwrap_or_default()
}

pub fn discard(s: &Sink, scope: &str) {
    if let Ok(mut state) = s.lock() {
        state.scopes.remove(scope);
    }
}

/// The runner's progress callback: push, never block, never fail a turn.
pub fn callback<'a>(s: Sink) -> crate::channels::discord::runner::ProgressFn<'a> {
    callback_for(s, String::new())
}

/// Scoped callback used by the daemon so concurrent channels cannot drain or
/// finalize one another's rows.
pub fn callback_for<'a>(
    s: Sink,
    scope: String,
) -> crate::channels::discord::runner::ProgressFn<'a> {
    Box::new(move |row: &Value| push_for(&s, &scope, row.clone()))
}

/// Tools whose work is nobody's receipt: they are the bulk of a turn (read and
/// search loops). The end-of-turn card lists none of them, though its tally
/// still counts them; the live feed shows every step.
fn is_quiet(tool: &str) -> bool {
    matches!(tool, "read" | "cat" | "find" | "grep" | "glob" | "ls")
}

/// Same test for a full progress row: `ls` through the shell is the same
/// nobody-status as the `ls` tool, so a `gray view` turn never also narrates
/// its `ls` neighbor. Counts are untouched — the tally still counts these.
fn is_quiet_row(tool: &str, detail: &str) -> bool {
    if is_quiet(tool) {
        return true;
    }
    if matches!(tool, "bash" | "shell") {
        let first = clean_command(detail)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        return matches!(
            first.as_str(),
            "ls" | "cat" | "find" | "grep" | "glob" | "read"
        );
    }
    false
}

/// The tool's name as a line starts with it, plain text (no icons). Shell
/// work reads the way gray's own transcript labels it — "Running" while
/// live, "Ran" once done — never "terminal". Any other tool goes by its
/// display `name` (see [`name_of`]).
fn label(tool: &str, name: &str) -> String {
    match tool {
        "bash" | "shell" => "Running".to_string(),
        "read" | "cat" => "Read".to_string(),
        "write" | "create" | "str_replace" => "Write".to_string(),
        "edit" | "apply_patch" => "Edit".to_string(),
        "" => "Working".to_string(),
        _ => name.to_string(),
    }
}

/// What to call a row's tool: gray's `label` (the plugin's manifest label,
/// or the wire name humanized), else the same humanizing done here for a
/// gray that sends no label. `discord_send` reads "Discord Send".
fn name_of(row: &Value) -> String {
    row.get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| humanize(row.get("tool").and_then(Value::as_str).unwrap_or("")))
}

/// gray's `humanize_tool_name`: `web_search` → `Web Search`; a single token
/// (`bash`, `custom`) is already displayable and stays as it is.
pub fn humanize(name: &str) -> String {
    if !name.contains(['_', '-']) {
        return name.to_string();
    }
    name.split(['_', '-'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line for one row, or `None` when the row adds nothing a reader
/// needs. A finished tool with no error is silence — the `ran` line above
/// it already said what happened.
fn line(row: &Value, elapsed: Option<u64>) -> Option<String> {
    let phase = row.get("phase").and_then(Value::as_str).unwrap_or("");
    let tool = row.get("tool").and_then(Value::as_str).unwrap_or("");
    let detail = row.get("detail").and_then(Value::as_str).unwrap_or("");
    let name = name_of(row);
    match phase {
        // The call has begun but has not reported its arguments yet: a bare
        // "Running" says nothing the status chip doesn't, and reads as a
        // duplicate beside the finished calls above it. The call appears
        // once its `tool_ran` lands, already carrying its command.
        "tool_started" => None,
        "tool_ran" => Some(ran_line(tool, &name, detail, elapsed)),
        "tool_finished" if row.get("error").and_then(Value::as_bool) == Some(true) => Some(
            format!("{} failed", if tool.is_empty() { "Tool" } else { &name }),
        ),
        "provider_retry" if !detail.is_empty() => {
            Some(format!("Provider retry: {}", one_line(detail)))
        }
        "compacted" => Some("Context compacted".to_string()),
        _ => None,
    }
}

/// A call reads "Running" while it runs and "Ran" once it has returned —
/// the feed stays in the channel after the turn, so it must read as done.
fn ran_line(tool: &str, name: &str, detail: &str, elapsed: Option<u64>) -> String {
    shell_line(tool, name, detail, elapsed, elapsed.is_some())
}

/// Whether `row` puts a line in the feed. A row that does not (a quiet
/// finish, a phase marker) never opens a new tool bubble on its own.
pub fn narrates(row: &Value) -> bool {
    line(row, None).is_some()
}

/// Receipt version of [`ran_line`]: the turn is over, so shell work reads as
/// completed — "Ran", the way gray's own transcript labels it.
fn card_line(tool: &str, name: &str, detail: &str, elapsed: Option<u64>) -> String {
    shell_line(tool, name, detail, elapsed, true)
}

fn shell_line(tool: &str, name: &str, detail: &str, elapsed: Option<u64>, done: bool) -> String {
    // A shell command earns cleaning (no redirections, no `;`-chain essay);
    // a file tool's detail is already a clean path.
    let arg = match tool {
        "bash" | "shell" => clean_command(detail),
        "" => String::new(),
        _ => one_line(detail),
    };
    let mut out = match tool {
        // No command means a background-job call (`action: output`/`kill`),
        // which would otherwise read as a bare, empty "Ran".
        "bash" | "shell" if arg.is_empty() && done => "Checked background job".to_string(),
        "bash" | "shell" if arg.is_empty() => "Checking background job".to_string(),
        "bash" | "shell" if done => "Ran".to_string(),
        _ => label(tool, name),
    };
    if !arg.is_empty() {
        out.push(' ');
        out.push_str(&preview(&arg));
    }
    match elapsed {
        Some(ms) => format!("{out} ({})", secs(ms)),
        None => out,
    }
}

fn preview(text: &str) -> String {
    format!("`{}`", capped(text, MAX_PREVIEW_CHARS))
}

fn secs(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// A shell row's command, whole: its first non-empty line, one line, capped at
/// [`MAX_COMMAND_CHARS`]. Nothing is shortened (no `… +N`), so a reader sees
/// exactly what ran.
fn clean_command(detail: &str) -> String {
    let first = detail
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    capped(&one_line(first), MAX_COMMAND_CHARS)
}

fn capped(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Duration keyed by the internal call id, so a line picks up its timing
/// once the call has returned.
fn elapsed_by_call(rows: &[Value]) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    for row in rows {
        if row.get("phase").and_then(Value::as_str) != Some("tool_finished") {
            continue;
        }
        if let (Some(id), Some(ms)) = (
            row.get("call_id").and_then(Value::as_str),
            row.get("elapsed_ms").and_then(Value::as_u64),
        ) {
            map.insert(id.to_string(), ms);
        }
    }
    map
}

fn elapsed_of(row: &Value, by_call: &HashMap<String, u64>) -> Option<u64> {
    row.get("call_id")
        .and_then(Value::as_str)
        .and_then(|id| by_call.get(id).copied())
}

/// Render one tool bubble's rows into its body: every distinct line, oldest
/// first (the accumulating progress bubble). `None` when nothing is
/// worth showing.
pub fn render(rows: &[Value]) -> Option<String> {
    let by_call = elapsed_by_call(rows);
    let mut lines: Vec<String> = Vec::new();
    for row in rows {
        let Some(text) = line(row, elapsed_of(row, &by_call)) else {
            continue;
        };
        // The same call reported twice (a tight loop) says nothing new.
        if lines.last() == Some(&text) {
            continue;
        }
        lines.push(text);
    }
    // Every step shows live, reads and searches included (the feed):
    // a long turn of `cat`/`grep` must not look stalled.
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

#[derive(Default)]
struct CardEntry {
    call_id: String,
    tool: String,
    name: String,
    detail: String,
    elapsed: Option<u64>,
    failed: bool,
}

/// Render the persistent end-of-turn card. The shape: a one-line tally of
/// what the turn did, then the actions themselves — never their output.
pub fn render_card(rows: &[Value]) -> Option<String> {
    let by_call = elapsed_by_call(rows);
    let mut entries: Vec<CardEntry> = Vec::new();
    let mut notices: Vec<String> = Vec::new();
    let mut turn_ms: Option<u64> = None;

    for row in rows {
        let phase = row.get("phase").and_then(Value::as_str).unwrap_or("");
        let tool = row
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let call_id = row
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match phase {
            "tool_ran" => entries.push(CardEntry {
                call_id,
                tool,
                name: name_of(row),
                detail: row
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                elapsed: elapsed_of(row, &by_call),
                ..CardEntry::default()
            }),
            "tool_finished" => {
                let failed = row.get("error").and_then(Value::as_bool) == Some(true);
                turn_ms = turn_ms.max(row.get("turn_ms").and_then(Value::as_u64));
                let index = entries.iter().rposition(|entry| {
                    (!call_id.is_empty() && entry.call_id == call_id)
                        || (call_id.is_empty() && entry.tool == tool)
                });
                if let Some(index) = index {
                    entries[index].failed |= failed;
                } else if failed {
                    entries.push(CardEntry {
                        call_id,
                        tool,
                        name: name_of(row),
                        failed,
                        ..CardEntry::default()
                    });
                }
            }
            "provider_retry" => {
                if let Some(detail) = row.get("detail").and_then(Value::as_str) {
                    if !detail.trim().is_empty() {
                        notices.push(format!("Provider retry: {}", one_line(detail)));
                    }
                }
            }
            "compacted" => notices.push("Context compacted".to_string()),
            _ => {}
        }
    }

    if entries.is_empty() && notices.is_empty() {
        return None;
    }

    let mut out = tally(&entries, turn_ms);
    let actions: Vec<&CardEntry> = entries
        .iter()
        .filter(|entry| !is_quiet_row(&entry.tool, &entry.detail))
        .collect();
    for entry in actions.iter().take(MAX_CARD_ROWS) {
        out.push('\n');
        out.push_str(&card_line(
            &entry.tool,
            &entry.name,
            &entry.detail,
            entry.elapsed,
        ));
        if entry.failed {
            out.push_str("\n  ↳ failed");
        }
    }
    let hidden = actions.len().saturating_sub(MAX_CARD_ROWS);
    if hidden > 0 {
        out.push_str(&format!("\n… +{hidden} more"));
    }
    for notice in notices.iter().take(4) {
        out.push('\n');
        out.push_str(notice);
    }

    Some(if out.chars().count() > MAX_CARD_CHARS {
        let mut bounded = out.chars().take(MAX_CARD_CHARS).collect::<String>();
        bounded.push('…');
        bounded
    } else {
        out
    })
}

/// The post-turn accounting line: `⋯ 12.4s · edited 2 files · read 4
/// files · ran 3 commands`. Counted from the same rows, so it is free.
fn tally(entries: &[CardEntry], turn_ms: Option<u64>) -> String {
    let parts = counts(entries.iter().map(|entry| entry.tool.as_str()));
    if parts.is_empty() {
        return String::new();
    }
    let head = match turn_ms {
        Some(ms) => format!("⋯ {}", secs(ms)),
        None => "⋯".to_string(),
    };
    format!("{head} · {}", parts.join(" · "))
}

/// The same accounting for a live footer: `ran 2 commands · edited 1 file`,
/// one count per call that reported its arguments. Empty for a turn that
/// has not touched a tool.
pub fn summary(rows: &[Value]) -> String {
    counts(
        rows.iter()
            .filter(|row| row.get("phase").and_then(Value::as_str) == Some("tool_ran"))
            .map(|row| row.get("tool").and_then(Value::as_str).unwrap_or("")),
    )
    .join(" · ")
}

fn counts<'a>(tools: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut ran = 0usize;
    let mut read = 0usize;
    let mut edited = 0usize;
    let mut other = 0usize;
    for tool in tools {
        match tool {
            "bash" | "shell" => ran += 1,
            "read" | "cat" | "find" | "grep" | "glob" | "ls" => read += 1,
            "write" | "create" | "str_replace" | "edit" | "apply_patch" => edited += 1,
            "" => {}
            _ => other += 1,
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if edited > 0 {
        parts.push(format!("edited {edited} file{}", plural(edited)));
    }
    if read > 0 {
        parts.push(format!("read {read} file{}", plural(read)));
    }
    if ran > 0 {
        parts.push(format!("ran {ran} command{}", plural(ran)));
    }
    if other > 0 {
        parts.push(format!("called {other} tool{}", plural(other)));
    }
    parts
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// How much of a turn's tool work a chat surface shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Each call: its command, a result line and a head/tail preview of its output.
    #[default]
    Verbose,
    /// Each call's command and result line; never its output.
    Commands,
    /// No tool lines at all.
    Off,
}

/// `progress_mode`: `"verbose"` (default), `"commands"` or `"off"`. Narration
/// switched off (`activity_indicator: false`, or the legacy `quiet: true`) is
/// `Off` whatever `progress_mode` says.
pub fn mode(config: &Value) -> Mode {
    if !enabled(config) {
        return Mode::Off;
    }
    match config.get("progress_mode").and_then(Value::as_str) {
        Some("commands") => Mode::Commands,
        Some("off") => Mode::Off,
        _ => Mode::Verbose,
    }
}

pub const FAILED_MARK: &str = "✗";

pub fn group_failed(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with(&format!("-# {FAILED_MARK} ")))
}

/// Output lines shown at each end of a preview.
const PREVIEW_EDGE: usize = 3;
/// Output of at most this many lines is shown whole.
const PREVIEW_WHOLE: usize = 2 * PREVIEW_EDGE;
/// A command (or a tool's argument) in a block, in characters.
const MAX_COMMAND_CHARS: usize = 240;
/// One output line in a preview, in characters.
const MAX_OUTPUT_LINE_CHARS: usize = 160;

/// One markdown block per tool call, oldest first. A block is the call's
/// command (or argument) in full, a `-# ⤷` result line with exit status,
/// duration and line count, and — for shell calls in `Verbose` mode — a fenced
/// head/tail preview of the output. Rows sharing a `call_id` are one call;
/// other rows (provider retries, compaction) become one subtext line each.
pub fn tool_blocks(rows: &[Value], mode: Mode) -> Vec<String> {
    if mode == Mode::Off {
        return Vec::new();
    }
    let mut blocks = Vec::new();
    let mut emitted: Vec<&str> = Vec::new();
    for row in rows {
        let phase = row.get("phase").and_then(Value::as_str).unwrap_or("");
        match phase {
            "tool_ran" | "tool_finished" => {
                let call = row.get("call_id").and_then(Value::as_str).unwrap_or("");
                if !call.is_empty() {
                    if emitted.contains(&call) {
                        continue;
                    }
                    emitted.push(call);
                }
                let ran = call_row(rows, call, row, "tool_ran");
                let fin = call_row(rows, call, row, "tool_finished");
                blocks.push(call_block(ran, fin, mode));
            }
            _ => {
                if let Some(text) = line(row, None) {
                    blocks.push(format!("-# {text}"));
                }
            }
        }
    }
    blocks
}

/// The last row of `phase` for `call`; an unnamed call is its own row.
fn call_row<'a>(rows: &'a [Value], call: &str, row: &'a Value, phase: &str) -> Option<&'a Value> {
    if call.is_empty() {
        return (row.get("phase").and_then(Value::as_str) == Some(phase)).then_some(row);
    }
    rows.iter().rev().find(|r| {
        r.get("phase").and_then(Value::as_str) == Some(phase)
            && r.get("call_id").and_then(Value::as_str) == Some(call)
    })
}

fn call_block(ran: Option<&Value>, fin: Option<&Value>, mode: Mode) -> String {
    let base = ran.or(fin).unwrap_or(&Value::Null);
    let tool = base.get("tool").and_then(Value::as_str).unwrap_or("");
    let shell = matches!(tool, "bash" | "shell");
    let detail = ran
        .and_then(|r| r.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let done = fin.map(|fin| finish_call(fin, shell));
    let mut out = if shell {
        let cmd = clean_command(detail);
        match (cmd.is_empty(), done.is_some()) {
            (true, true) => "**Checked** background job".to_string(),
            (true, false) => "**Checking** background job".to_string(),
            (false, true) => format!("**Ran** {}", code_span(&cmd)),
            (false, false) => format!("**Running** {}", code_span(&cmd)),
        }
    } else {
        let name = name_of(base);
        let name = if name.is_empty() {
            "Tool".to_string()
        } else {
            name
        };
        let arg = capped(&one_line(detail), MAX_COMMAND_CHARS);
        if arg.is_empty() {
            format!("**{name}**")
        } else {
            format!("**{name}** {}", code_span(&arg))
        }
    };
    if shell {
        let cwd = ran
            .and_then(|r| r.get("cwd"))
            .and_then(Value::as_str)
            .map(one_line)
            .filter(|cwd| !cwd.is_empty() && cwd != ".");
        if let Some(cwd) = cwd {
            out.push_str(&format!("  · {}", code_span(&cwd)));
        }
    }
    let Some(done) = done else {
        return out;
    };
    let mark = if done.failed { FAILED_MARK } else { "⤷" };
    out.push_str(&format!("\n-# {mark} {}", done.status));
    if shell && mode == Mode::Verbose && !done.lines.is_empty() {
        out.push('\n');
        out.push_str(&output_block(&done.lines));
    }
    out
}

/// A finished call as its result line shows it.
struct Finished<'a> {
    /// What follows `⤷` on the result line.
    status: String,
    /// A non-zero exit, or gray flagged the call as an error.
    failed: bool,
    /// The output body: no header, squeeze note or untrusted-output tags.
    lines: Vec<&'a str>,
}

fn finish_call(fin: &Value, shell: bool) -> Finished<'_> {
    let raw = fin.get("output").and_then(Value::as_str).unwrap_or("");
    let error = fin.get("error").and_then(Value::as_bool) == Some(true);
    // Our own timing. A shell header carries its own duration instead.
    let elapsed = fin.get("elapsed_ms").and_then(Value::as_u64).map(secs);
    if shell {
        let shown = split_shell_output(raw);
        let failed = error || shown.exit.is_some_and(|code| code != 0);
        let status = shown.summary.unwrap_or_else(|| {
            let word = if failed { "failed" } else { "done" };
            match &elapsed {
                Some(time) => format!("{word} · {time}"),
                None => word.to_string(),
            }
        });
        return Finished {
            status,
            failed,
            lines: shown.body,
        };
    }
    let mut status = match raw.lines().next() {
        Some(first) if !error => code_span(&capped(&one_line(first), MAX_OUTPUT_LINE_CHARS)),
        _ if error => "failed".to_string(),
        _ => "done".to_string(),
    };
    if let Some(time) = elapsed {
        status = format!("{status} · {time}");
    }
    Finished {
        status,
        failed: error,
        lines: Vec::new(),
    }
}

/// gray's header line opens a shell call's `output`; the body follows it.
const SQUEEZE_NOTE: &str = "squeezed by runs";
const UNTRUSTED_OPEN: &str = "<untrusted-output>";
const UNTRUSTED_CLOSE: &str = "</untrusted-output>";

/// A shell call's `output` as gray sends it: the header `exit N · time ·
/// lines · log …`, an optional squeeze note, then the body inside
/// `<untrusted-output>` tags. The tags are never shown.
struct ShellOutput<'a> {
    exit: Option<i64>,
    /// `exit N · time · lines`, as the result line shows it.
    summary: Option<String>,
    body: Vec<&'a str>,
}

fn split_shell_output(raw: &str) -> ShellOutput<'_> {
    let mut lines: Vec<&str> = raw.lines().collect();
    let mut exit = None;
    let mut summary = None;
    if let Some((code, text)) = lines.first().and_then(|line| shell_header(line)) {
        exit = Some(code);
        summary = Some(text);
        lines.remove(0);
        if lines
            .first()
            .is_some_and(|line| line.starts_with(SQUEEZE_NOTE))
        {
            lines.remove(0);
        }
    }
    if lines
        .first()
        .is_some_and(|line| line.trim() == UNTRUSTED_OPEN)
    {
        lines.remove(0);
    }
    if lines
        .last()
        .is_some_and(|line| line.trim() == UNTRUSTED_CLOSE)
    {
        lines.pop();
    }
    ShellOutput {
        exit,
        summary,
        body: lines,
    }
}

/// Parses gray's bash header into its exit code and a summary without the
/// log path. The masks note (`exit 0 (\`head\` masks …)`) is dropped with
/// the rest of the first field, so the summary is `exit 0 · 1.2s · 26 lines`.
fn shell_header(line: &str) -> Option<(i64, String)> {
    let mut parts = line.split(" · ");
    let head = parts.next()?.strip_prefix("exit ")?;
    let end = head
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(head.len());
    let rest = &head[end..];
    if !(rest.is_empty() || rest.starts_with(" (")) {
        return None;
    }
    let code: i64 = head[..end].parse().ok()?;
    let mut summary = format!("exit {code}");
    for part in parts.filter(|part| !part.starts_with("log ")) {
        summary.push_str(" · ");
        summary.push_str(&capped(&one_line(part), MAX_OUTPUT_LINE_CHARS));
    }
    Some((code, summary))
}

/// The head and tail of a shell call's output in one code block, with the
/// hidden middle counted between them. The fence is longer than any backtick
/// run in the output, so the output cannot close it.
fn output_block(lines: &[&str]) -> String {
    let clean: Vec<String> = lines
        .iter()
        .map(|line| capped(&strip_controls(line), MAX_OUTPUT_LINE_CHARS))
        .collect();
    let shown: Vec<String> = if clean.len() <= PREVIEW_WHOLE {
        clean
    } else {
        let mut shown = clean[..PREVIEW_EDGE].to_vec();
        shown.push(format!("… +{} lines", clean.len() - PREVIEW_WHOLE));
        shown.extend_from_slice(&clean[clean.len() - PREVIEW_EDGE..]);
        shown
    };
    let body = shown.join("\n");
    let fence = "`".repeat(longest_backtick_run(&body).max(2) + 1);
    format!("{fence}\n{body}\n{fence}")
}

/// Inline code that holds `text` whatever backticks it carries.
fn code_span(text: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(text) + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

fn longest_backtick_run(text: &str) -> usize {
    let (mut best, mut run) = (0, 0);
    for c in text.chars() {
        if c == '`' {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

/// Tabs become spaces; other control characters are dropped.
fn strip_controls(line: &str) -> String {
    line.replace('\t', "    ")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}
