//! A turn as a stream of Discord messages.
//!
//! With `GRAY_STREAM_TEXT=1`, gray's `--json` wire interleaves `text` rows
//! (the assistant's prose, numbered by segment, one segment per run of prose
//! between tool calls) with the tool progress rows. This module lays a turn
//! out the way a chat bridge does: every run of prose is its own message,
//! streamed in place, and every group of tool calls is its own progress
//! bubble below it. The next prose after a tool call starts a fresh message,
//! so the channel grows as the agent works instead of one message changing
//! out of sight:
//!
//! ```text
//! Let me check what's running on the box.            ← message 1 (prose)
//! -# Ran `gray ps` (0.3s)                             ← message 2 (tools)
//! -# Running `cargo test`
//! **Done / idle:** … ▉                               ← message 3 (prose)
//! ┃ -# Working · started 20 seconds ago · ran 2 commands [Stop]
//! ```
//!
//! The newest message carries the turn's status chip: a small Container
//! whose accent tracks the turn (dark grey while working, red on failure,
//! grey when stopped) with the Stop button while it runs. Its clock is a
//! Discord timestamp (`<t:…:R>`) that every client keeps current on its own,
//! so a quiet turn costs no edits. When a newer message appears the chip
//! moves down to it. A turn that finishes drops the chip outright — the
//! answer stands alone, the way the bridge leaves it — while a failed or
//! stopped turn keeps its line as the notice.
//!
//! The finished answer is the last message; the files it named (`MEDIA:`)
//! sit inside it. A tool group of more than a handful of lines folds to a
//! count plus its latest lines, with the full list in a spoiler below it.
//! A message too long for Discord continues in the next one.
//!
//! [`Timeline`] is the pure model. The gateway feeds it rows, asks it which
//! sends, edits and deletes are due ([`Timeline::plan`]), and reports back
//! what landed or failed. Clock and transport stay outside so the layout is
//! testable.

use serde_json::{Value, json};

/// The streaming cursor (`DEFAULT_STREAMING_CURSOR`).
pub const CURSOR: &str = " ▉";
/// Text per message, in UTF-16 units: the 2000-character message cap. A
/// message past it rolls over into a new one, never cut mid-block.
pub const CARD_TEXT: usize = 2000;
/// Flood-limit replies (HTTP 429) in a row before the turn stops editing and
/// sends whatever is left as new messages.
const FLOOD_STRIKES: u32 = 3;
/// `MAX_SPLIT_MESSAGES`: one runaway run of prose never floods the
/// channel.
pub const MAX_SPLIT: usize = 8;
/// Messages one turn may open. Past it, later steps share the last one.
pub const MAX_MESSAGES: usize = 30;
/// Discord's attachment cap per message.
pub const MAX_MEDIA: usize = 10;
/// Refused requests (HTTP 400) tolerated before a message is left as it is.
const STRIKES: u32 = 3;
/// Times a deleted newest message is posted again.
const REPOSTS: u32 = 2;
/// Longest wait between edits after flood-limit replies, in seconds.
const MAX_EDIT_INTERVAL: f64 = 10.0;

const WORKING: u32 = 0x4E5058;
const FAILED: u32 = 0xED4245;
const STOPPED: u32 = 0x80848E;

fn tool_group(text: &str) -> Value {
    let accent = if crate::channels::discord::activity::group_failed(text) {
        FAILED
    } else {
        crate::channels::discord::render::QUIET_ACCENT
    };
    json!({"type": 17, "accent_color": accent, "components": [{"type": 10, "content": text}]})
}

/// Quiet mode (`"quiet": true`): one durable post per turn. Flips the
/// defaults for `stream_replies`, `activity_indicator` and the queued ack
/// to off; an explicit key always wins over the meta flag. The typing
/// indicator stays — it is the only sign of life a quiet turn has.
pub fn quiet(config: &Value) -> bool {
    config
        .get("quiet")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Live replies on/off. Default on; `"stream_replies": false` keeps the
/// answer in one durable post at the end of the turn.
pub fn enabled(config: &Value) -> bool {
    config
        .get("stream_replies")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| !quiet(config))
}

/// Where the turn ended up; drives the accent and the status line.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Working,
    Done,
    /// A short reason: "failed", "timed out", "hit the budget".
    Failed(String),
    Stopped,
}

/// Why a send or edit did not land, as far as the turn cares.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Failure {
    /// Rate limited, a 5xx, a timeout or a dropped connection: try again
    /// after this many seconds.
    Busy(f64),
    /// Discord refused this payload (HTTP 400). The next frame differs, so
    /// it is retried with backoff, a few times.
    Refused,
    /// The message is gone (deleted by someone, HTTP 404).
    Gone,
    /// The bot may not post or edit here (HTTP 401/403).
    Forbidden,
}

#[derive(Debug, Clone)]
enum Block {
    Text {
        /// Append-only lines from `delta`s.
        stable: String,
        /// The provisional unfinished line from the latest row.
        tail: String,
        segment: u64,
        done: bool,
        /// The authoritative answer once the turn has finished. Already
        /// stripped of delivered `MEDIA:` tags.
        answer: Option<String>,
    },
    Tools {
        rows: Vec<Value>,
    },
}

/// One Discord message the timeline owns.
#[derive(Debug, Clone, Default)]
struct Posted {
    id: String,
    shown: String,
    /// The body text as shown: what a remainder is cut from once edits stop.
    text: String,
    at: f64,
    /// Refused edits in a row.
    strikes: u32,
    /// No edit before this time (backoff after a failure).
    retry_at: f64,
    /// Seconds between edits after flood-limit replies (0: the base gap).
    interval: f64,
    /// Left as it is for the rest of the turn.
    frozen: bool,
}

/// One message as it should look now.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub components: Vec<Value>,
    /// The message's body as plain markdown (no status chip): what the
    /// durable outbox records for it, and what the activity hook reports.
    pub text: String,
    /// Uploads this message references as `attachment://<name>`; a send or
    /// edit must carry them.
    pub files: Vec<String>,
    /// The last block of the turn this message shows.
    pub block: usize,
    pub pager: Option<Vec<String>>,
}

/// A file the finished answer named, shown inside its message.
#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    pub name: String,
    /// Images and videos go to the gallery; anything else is a File card.
    pub visual: bool,
}

impl Card {
    fn key(&self) -> String {
        format!(
            "{}{}",
            serde_json::to_string(&self.components).unwrap_or_default(),
            serde_json::to_string(&self.pager).unwrap_or_default()
        )
    }
}

/// One request the gateway should make, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Post { card: usize, body: Card },
    Edit { card: usize, id: String, body: Card },
    Delete { card: usize, id: String },
}

/// The finished answer as it landed: each of its messages' body and id.
#[derive(Debug, Clone, PartialEq)]
pub struct Landed {
    pub parts: Vec<(String, String)>,
}

/// One block's share of the layout before it is cut into messages.
struct Unit {
    text: String,
    block: usize,
    tools: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Timeline {
    blocks: Vec<Block>,
    cards: Vec<Posted>,
    /// Messages cannot be posted here at all (forbidden, or refused again
    /// and again). The turn is left alone from then on.
    lost: bool,
    /// Refused posts in a row, and no post before `post_retry`.
    post_strikes: u32,
    post_retry: f64,
    reposts: u32,
    /// Stream prose for this turn. Off for slash-command turns (their
    /// answer belongs to the interaction) and when `stream_replies` is off;
    /// `text` rows are then ignored and only tool bubbles show.
    text: bool,
    started: f64,
    /// How the turn ended, and when: the status line's time stops there.
    status: Option<(Status, f64)>,
    /// Quiet mode (`"quiet": true`): the whole turn is one live status
    /// card — the chip, edited in place — plus the durable answer post.
    quiet: bool,
    /// `custom_id` of the status chip's Stop button while the turn runs.
    stop: Option<String>,
    media: Vec<Media>,
    retracted: bool,
    /// How much tool work the chat shows (`progress_mode`).
    progress: crate::channels::discord::activity::Mode,
    /// Consecutive flood-limit replies to edits.
    flood: u32,
    /// Set after [`FLOOD_STRIKES`]: no more edits; the rest is posted anew.
    edits_stopped: bool,
    /// Posted messages already on screen when edits stopped — the frozen
    /// prefix the remainder goes out after. Pinned at the stop so fresh
    /// posts landing cannot move the cut.
    stop_prefix: Option<usize>,
}

impl Timeline {
    pub fn new(text: bool, started: f64) -> Self {
        Self {
            text,
            started,
            ..Self::default()
        }
    }

    /// Offer a Stop button in the status chip while the turn runs.
    pub fn with_stop(mut self, custom_id: String) -> Self {
        self.stop = Some(custom_id);
        self
    }

    /// Quiet mode: only the status chip renders, as the turn's one live
    /// card. Tool rows are still absorbed for its command tally.
    pub fn with_quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }

    /// How tool work shows: full blocks, commands only, or none.
    pub fn with_progress(mut self, progress: crate::channels::discord::activity::Mode) -> Self {
        self.progress = progress;
        self
    }

    /// Show the answer's files inside its message (at most [`MAX_MEDIA`]).
    pub fn attach(&mut self, media: Vec<Media>) {
        self.media = media.into_iter().take(MAX_MEDIA).collect();
    }

    /// Fold one progress row into the layout.
    pub fn absorb(&mut self, row: &Value) {
        if row.get("phase").and_then(Value::as_str) == Some("retract") {
            if let Some(segment) = row.get("segment").and_then(Value::as_u64) {
                self.retract(segment);
            }
            return;
        }
        if row.get("phase").and_then(Value::as_str) == Some("text") {
            if self.text {
                self.absorb_text(row);
            }
            return;
        }
        if let Some(Block::Tools { rows }) = self.blocks.last_mut() {
            rows.push(row.clone());
            return;
        }
        if !crate::channels::discord::activity::narrates(row) {
            return;
        }
        // A tool after prose closes it, even if gray's closing row was lost.
        self.close_text();
        self.blocks.push(Block::Tools {
            rows: vec![row.clone()],
        });
    }

    fn absorb_text(&mut self, row: &Value) {
        let segment = row.get("segment").and_then(Value::as_u64).unwrap_or(0);
        let delta = row.get("delta").and_then(Value::as_str).unwrap_or("");
        let new_tail = row.get("tail").and_then(Value::as_str).unwrap_or("");
        let closes = row.get("done").and_then(Value::as_bool).unwrap_or(false);
        let index = self
            .blocks
            .iter()
            .rposition(|block| matches!(block, Block::Text { segment: s, .. } if *s == segment));
        let index = match index {
            Some(index) => index,
            None => {
                self.close_text();
                self.blocks.push(Block::Text {
                    segment,
                    stable: String::new(),
                    tail: String::new(),
                    done: false,
                    answer: None,
                });
                self.blocks.len() - 1
            }
        };
        if let Block::Text {
            stable, tail, done, ..
        } = &mut self.blocks[index]
        {
            stable.push_str(delta);
            *tail = new_tail.to_string();
            *done |= closes;
        }
    }

    fn retract(&mut self, segment: u64) {
        let before = self.blocks.len();
        self.blocks.retain(
            |block| !matches!(block, Block::Text { segment: s, answer: None, .. } if *s == segment),
        );
        if self.blocks.len() == before {
            return;
        }
        self.retracted = true;
        let mut merged: Vec<Block> = Vec::with_capacity(self.blocks.len());
        for block in std::mem::take(&mut self.blocks) {
            match (merged.last_mut(), block) {
                (Some(Block::Tools { rows }), Block::Tools { rows: more }) => rows.extend(more),
                (_, block) => merged.push(block),
            }
        }
        self.blocks = merged;
    }

    fn close_text(&mut self) {
        for block in &mut self.blocks {
            if let Block::Text { done, .. } = block {
                *done = true;
            }
        }
    }

    /// Whether any prose has been streamed this turn.
    pub fn has_text(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(block, Block::Text { .. }))
    }

    /// Hand the finished turn's authoritative answer (prose only, `MEDIA:`
    /// tags already taken out) to the turn. When the turn's last message is
    /// the prose it streamed as, that message becomes the answer in place;
    /// otherwise (the agent's last act was a tool call, or this gray streams
    /// no prose) the answer is the next new message below the tool lines.
    /// `false` when the caller must deliver the answer itself: prose
    /// streaming is off for this turn, the turn cannot post here, or nothing
    /// was shown before the answer (it is then one plain durable post).
    pub fn adopt(&mut self, prose: &str) -> bool {
        if self.lost || !self.text || prose.trim().is_empty() {
            return false;
        }
        match self.blocks.last_mut() {
            None => return false,
            Some(Block::Text { done, answer, .. }) => {
                *done = true;
                *answer = Some(prose.to_string());
            }
            Some(Block::Tools { .. }) => {
                self.close_text();
                self.blocks.push(Block::Text {
                    segment: u64::MAX,
                    stable: String::new(),
                    tail: String::new(),
                    done: true,
                    answer: Some(prose.to_string()),
                });
            }
        }
        true
    }

    /// Record how the turn ended at `now`. The next plan settles the turn.
    pub fn settle(&mut self, status: Status, now: f64) {
        self.status = Some((status, now));
    }

    /// The messages this turn should show at `now`.
    pub fn render(&self, now: f64) -> Vec<Card> {
        let (status, now) = self.status.clone().unwrap_or((Status::Working, now));
        let live = status == Status::Working;
        let budget = CARD_TEXT;
        let mut units: Vec<Unit> = Vec::new();
        let mut tool_rows: Vec<Value> = Vec::new();
        for (index, block) in self.blocks.iter().enumerate() {
            let texts: Vec<String> = match block {
                Block::Tools { rows } => {
                    tool_rows.extend(rows.iter().cloned());
                    pack(
                        crate::channels::discord::activity::tool_blocks(rows, self.progress),
                        budget,
                    )
                }
                Block::Text {
                    stable,
                    tail,
                    done,
                    answer,
                    ..
                } => {
                    let prose = match answer {
                        Some(answer) => answer.clone(),
                        None => crate::channels::discord::media_tags::strip_for_display(&format!(
                            "{stable}{tail}"
                        )),
                    };
                    let prose = crate::channels::discord::text::sanitize(&prose);
                    let prose = prose.trim_end();
                    if prose.trim().is_empty() {
                        Vec::new()
                    } else if live && !done {
                        vec![format!("{prose}{CURSOR}")]
                    } else {
                        vec![prose.to_string()]
                    }
                }
            };
            let tools = matches!(block, Block::Tools { .. });
            units.extend(texts.into_iter().map(|text| Unit {
                text,
                block: index,
                tools,
            }));
        }
        if self.quiet {
            // The one live card: just the chip. A finished turn leaves
            // nothing behind — the durable answer post stands alone.
            if status == Status::Done {
                return Vec::new();
            }
            let summary = crate::channels::discord::activity::summary(&tool_rows);
            let elapsed = (now - self.started).max(0.0);
            let mut text = match &status {
                Status::Working => format!("Working · started {} ago", duration(elapsed)),
                Status::Failed(reason) => {
                    format!("{} after {}", capitalized(reason), duration(elapsed))
                }
                Status::Stopped => format!("Stopped after {}", duration(elapsed)),
                Status::Done => unreachable!(),
            };
            if !summary.is_empty() {
                text.push_str(" · ");
                text.push_str(&summary);
            }
            return vec![Card {
                components: vec![self.chip(&status, &summary, now)],
                text,
                files: Vec::new(),
                block: self.blocks.len().saturating_sub(1),
                pager: None,
            }];
        }
        if units.is_empty() {
            return Vec::new();
        }
        // A turn of dozens of steps shares one last message rather than
        // flooding the channel.
        if units.len() > MAX_MESSAGES {
            let tail = units.split_off(MAX_MESSAGES - 1);
            let block = tail.last().map_or(0, |unit| unit.block);
            let text = tail
                .iter()
                .map(|unit| unit.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            units.push(Unit {
                text,
                block,
                tools: false,
            });
        }
        // A finished turn drops the chip: the answer stands alone.
        let chip = (status != Status::Done).then(|| {
            self.chip(
                &status,
                &crate::channels::discord::activity::summary(&tool_rows),
                now,
            )
        });
        let mut cards: Vec<Card> = Vec::new();
        for unit in units {
            let (chunks, overflow) = split_block(&unit.text, budget);
            for chunk in chunks {
                let components = if unit.tools {
                    vec![tool_group(&chunk)]
                } else {
                    vec![json!({"type": 10, "content": chunk.clone()})]
                };
                cards.push(Card {
                    components,
                    text: chunk,
                    files: Vec::new(),
                    block: unit.block,
                    pager: None,
                });
            }
            if let Some(rest) = overflow {
                let pages = crate::channels::discord::pager::paginate(
                    &rest,
                    crate::channels::discord::pager::PAGE_LIMIT,
                );
                let first = pages.first().cloned().unwrap_or_default();
                cards.push(Card {
                    components: vec![crate::channels::discord::pager::container(
                        &first,
                        "",
                        crate::channels::discord::render::QUIET_ACCENT,
                    )],
                    text: first,
                    files: Vec::new(),
                    block: unit.block,
                    pager: Some(pages),
                });
            }
        }
        if let Some(last) = cards.last_mut() {
            last.components.extend(self.media_components());
            last.files = self.media.iter().map(|media| media.name.clone()).collect();
            last.components.extend(chip);
        }
        cards
    }

    /// The answer's files: images and videos in one gallery, the rest as
    /// File cards, all referencing uploads that travel with the request.
    fn media_components(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let visuals: Vec<Value> = self
            .media
            .iter()
            .filter(|media| media.visual)
            .map(|media| json!({"media": {"url": format!("attachment://{}", media.name)}}))
            .collect();
        if !visuals.is_empty() {
            out.push(json!({"type": 12, "items": visuals}));
        }
        for media in self.media.iter().filter(|media| !media.visual) {
            out.push(json!({"type": 13, "file": {"url": format!("attachment://{}", media.name)}}));
        }
        out
    }

    /// The status chip: an accented Container with the status line and the
    /// Stop button while the turn runs. Never called for `Status::Done` —
    /// a finished turn leaves no chip behind.
    fn chip(&self, status: &Status, summary: &str, now: f64) -> Value {
        let elapsed = (now - self.started).max(0.0);
        let mut line = match status {
            // A Discord timestamp: "started 20 seconds ago", kept current by
            // every client with no edits from us.
            Status::Working => {
                format!("Working · started <t:{}:R>", self.started.max(0.0) as i64)
            }
            Status::Failed(reason) => {
                format!("{} after {}", capitalized(reason), duration(elapsed))
            }
            Status::Stopped => format!("Stopped after {}", duration(elapsed)),
            Status::Done => unreachable!("a finished turn has no chip"),
        };
        if !summary.is_empty() {
            line.push_str(" · ");
            line.push_str(summary);
        }
        if matches!(status, Status::Failed(_) | Status::Stopped) {
            line.push_str(" · actions may already have happened");
        }
        let text = json!({"type": 10, "content": format!("-# {line}")});
        let first = match (&self.stop, status) {
            (Some(custom_id), Status::Working) => json!({
                "type": 9,
                "components": [text],
                "accessory": {
                    "type": 2,
                    "style": 4,
                    "label": "Stop",
                    "custom_id": custom_id,
                },
            }),
            _ => text,
        };
        let accent = match status {
            Status::Working => WORKING,
            Status::Failed(_) => FAILED,
            Status::Stopped => STOPPED,
            Status::Done => unreachable!(),
        };
        json!({"type": 17, "accent_color": accent, "components": [first]})
    }

    /// The requests due now. Posts go strictly in message order; an edit
    /// waits `gap` seconds after the message's last change (and out any
    /// backoff) unless the turn is `finishing`, when everything settles at
    /// once. Tool messages edit no faster than [`crate::channels::discord::activity::TOOL_EDIT_GAP`].
    pub fn plan(&self, now: f64, gap: f64, finishing: bool) -> Vec<Op> {
        if self.lost {
            return Vec::new();
        }
        let cards = self.rendered(now);
        let mut ops = Vec::new();
        for (index, body) in cards.iter().enumerate() {
            match self.cards.get(index) {
                Some(posted) if posted.frozen || self.edits_stopped => {}
                Some(posted) => {
                    let wait = self.gap_for(body, gap).max(posted.interval);
                    let due = finishing || (now - posted.at >= wait && now >= posted.retry_at);
                    if posted.shown != body.key() && due {
                        ops.push(Op::Edit {
                            card: index,
                            id: posted.id.clone(),
                            body: body.clone(),
                        });
                    }
                }
                None => {
                    if finishing || now >= self.post_retry {
                        ops.push(Op::Post {
                            card: index,
                            body: body.clone(),
                        });
                    }
                }
            }
        }
        // The turn shrank (the final answer is shorter than its preview):
        // retire the messages it no longer fills.
        if finishing || self.retracted {
            for (index, posted) in self.cards.iter().enumerate().skip(cards.len()) {
                ops.push(Op::Delete {
                    card: index,
                    id: posted.id.clone(),
                });
            }
        }
        ops
    }

    /// Seconds between edits of `body`: tool messages wait longer than prose.
    fn gap_for(&self, body: &Card, gap: f64) -> f64 {
        match self.blocks.get(body.block) {
            Some(Block::Tools { .. }) => gap.max(crate::channels::discord::activity::TOOL_EDIT_GAP),
            _ => gap,
        }
    }

    /// The turn's messages as they should look on screen at `now`: the
    /// rendered cards, or — once edits have stopped — the frozen prefix
    /// plus the remainder as new messages. `plan` publishes this view, so
    /// `landed`, `newest_shown` and `answer_ids` must check against it too.
    pub fn rendered(&self, now: f64) -> Vec<Card> {
        let cards = self.render(now);
        if self.edits_stopped {
            self.after_stop(cards)
        } else {
            cards
        }
    }

    /// Once edits have stopped, the frozen messages stay as they are, and
    /// what they have not shown yet goes out as new messages: the last
    /// frozen message's text is a prefix of what it now renders, and the
    /// rest of the turn follows it.
    fn after_stop(&self, cards: Vec<Card>) -> Vec<Card> {
        let k = self.stop_prefix.unwrap_or(self.cards.len());
        if k == 0 || cards.len() < k {
            return cards;
        }
        let shown = self.cards[k - 1].text.as_str();
        let now_text = cards[k - 1].text.as_str();
        let mut rest = now_text.strip_prefix(shown).unwrap_or(now_text).to_string();
        for card in &cards[k..] {
            rest.push('\n');
            rest.push_str(&card.text);
        }
        // The remainder ends on the turn's last block — the answer once it
        // is adopted — and its messages are tagged so, whatever block the
        // frozen prefix card belonged to.
        let block = cards.last().map(|card| card.block).unwrap_or_default();
        let extras: Vec<Value> = cards
            .last()
            .map(|card| card.components.iter().skip(1).cloned().collect())
            .unwrap_or_default();
        let mut out: Vec<Card> = cards.iter().take(k).cloned().collect();
        let (chunks, overflow) = split_block(&rest, CARD_TEXT);
        for chunk in chunks.into_iter().chain(overflow) {
            out.push(Card {
                components: vec![json!({"type": 10, "content": chunk.clone()})],
                text: chunk,
                files: Vec::new(),
                block,
                pager: None,
            });
        }
        if out.len() > k || cards.len() > k {
            if let Some(last) = out.last_mut() {
                last.components.extend(extras);
            }
        }
        out
    }

    /// A post landed as message `id`.
    pub fn posted(&mut self, card: usize, id: &str, body: &Card, now: f64) {
        self.post_strikes = 0;
        self.flood = 0;
        if card == self.cards.len() {
            self.cards.push(Posted {
                id: id.to_string(),
                shown: body.key(),
                text: body.text.clone(),
                at: now,
                ..Posted::default()
            });
        }
    }

    /// An edit landed.
    pub fn edited(&mut self, card: usize, body: &Card, now: f64) {
        self.flood = 0;
        if let Some(posted) = self.cards.get_mut(card) {
            posted.shown = body.key();
            posted.text = body.text.clone();
            posted.at = now;
            posted.strikes = 0;
            posted.interval = 0.0;
        }
    }

    /// A post did not land. A busy Discord is waited out; a refused post is
    /// retried a few times (the next frame differs); a forbidden channel
    /// ends the turn's messages.
    pub fn post_failed(&mut self, failure: Failure, now: f64) {
        match failure {
            Failure::Busy(secs) => self.post_retry = now + secs.clamp(0.5, 30.0),
            Failure::Refused => {
                self.post_strikes += 1;
                if self.post_strikes >= STRIKES {
                    self.lost = true;
                } else {
                    self.post_retry = now + backoff(self.post_strikes);
                }
            }
            Failure::Gone | Failure::Forbidden => self.lost = true,
        }
    }

    /// An edit did not land. Only that message is affected: a busy Discord
    /// is waited out, a refused edit is retried with backoff and then left
    /// as it is, and a deleted newest message is posted again. Flood-limit
    /// replies double the edit interval (honouring `retry_after`, at most
    /// [`MAX_EDIT_INTERVAL`]); after [`FLOOD_STRIKES`] in a row the turn
    /// stops editing.
    pub fn edit_failed(&mut self, card: usize, failure: Failure, now: f64) {
        let newest = card + 1 == self.cards.len();
        let reposts = self.reposts;
        if matches!(failure, Failure::Busy(_)) {
            self.flood += 1;
            if self.flood >= FLOOD_STRIKES && !self.edits_stopped {
                self.edits_stopped = true;
                self.stop_prefix = Some(self.cards.len());
            }
        }
        let Some(posted) = self.cards.get_mut(card) else {
            return;
        };
        match failure {
            Failure::Busy(secs) => {
                let base = crate::channels::discord::activity::MIN_EDIT_GAP;
                posted.interval = (posted.interval.max(base) * 2.0)
                    .max(secs)
                    .min(MAX_EDIT_INTERVAL);
                posted.retry_at = now + posted.interval;
            }
            Failure::Refused => {
                posted.strikes += 1;
                if posted.strikes >= STRIKES {
                    posted.frozen = true;
                } else {
                    posted.retry_at = now + backoff(posted.strikes);
                }
            }
            Failure::Gone if newest && reposts < REPOSTS => {
                self.reposts += 1;
                self.cards.truncate(card);
            }
            Failure::Gone | Failure::Forbidden => posted.frozen = true,
        }
    }

    /// A message was deleted (by us, at the end of the turn).
    pub fn deleted(&mut self, card: usize) {
        self.cards.truncate(card);
    }

    /// Give the turn up: nothing more is sent or edited.
    pub fn lose(&mut self) {
        self.lost = true;
    }

    /// Whether the newest message is on screen exactly as rendered at `now`.
    fn newest_shown(&self, now: f64) -> Option<(Card, String)> {
        if self.lost {
            return None;
        }
        let cards = self.rendered(now);
        let index = cards.len().checked_sub(1)?;
        let posted = self.cards.get(index)?;
        let card = cards.into_iter().nth(index)?;
        (posted.shown == card.key()).then(|| (card, posted.id.clone()))
    }

    /// The index of the adopted answer's block, if there is one.
    fn answer_block(&self) -> Option<usize> {
        match self.blocks.last() {
            Some(Block::Text {
                answer: Some(_), ..
            }) => Some(self.blocks.len() - 1),
            _ => None,
        }
    }

    /// The adopted answer, if every message of it is on screen exactly as
    /// rendered. `None` means the caller must deliver the answer itself.
    pub fn landed(&self, now: f64) -> Option<Landed> {
        let answer = self.answer_block()?;
        if self.lost {
            return None;
        }
        let cards = self.rendered(now);
        if self.cards.len() > cards.len() {
            return None;
        }
        let prefix = self.stop_prefix.unwrap_or(usize::MAX);
        let mut parts = Vec::new();
        for (index, card) in cards.iter().enumerate() {
            if card.block != answer {
                continue;
            }
            let posted = self.cards.get(index)?;
            // Equal text with only the chip or a pager mint behind is the
            // answer on screen already; a message carrying files must match
            // exactly so they are known delivered.
            if posted.shown == card.key() || (card.files.is_empty() && posted.text == card.text) {
                parts.push((card.text.clone(), posted.id.clone()));
                continue;
            }
            // The frozen prefix's last card may lag: the tail it could not
            // show went out as the fresh messages after it, which carry the
            // answer and are checked as their own cards.
            if !(self.edits_stopped && index + 1 == prefix && cards.len() > prefix) {
                return None;
            }
        }
        (!parts.is_empty()).then_some(Landed { parts })
    }

    /// The message whose status chip shows how the turn ended, if it is on
    /// screen: a failed or stopped turn needs no separate notice when its
    /// chip says so. A finished turn has no chip, so `None`.
    pub fn status_card(&self, now: f64) -> Option<String> {
        match self.status.as_ref()?.0 {
            Status::Done => None,
            _ => self.newest_shown(now).map(|(_, id)| id),
        }
    }

    /// The answer's messages on screen, for retracting a preview whose
    /// answer did not land before the durable resend.
    pub fn answer_ids(&self) -> Vec<String> {
        let Some(answer) = self.answer_block() else {
            return Vec::new();
        };
        let cards = self.rendered(0.0);
        self.cards
            .iter()
            .enumerate()
            .filter(|(index, _)| cards.get(*index).is_none_or(|card| card.block == answer))
            .map(|(_, posted)| posted.id.clone())
            .collect()
    }
}

/// Pack whole tool blocks into messages of at most `budget` units, joined
/// by a blank line. A block is never cut, unless it alone exceeds the budget.
fn pack(blocks: Vec<String>, budget: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for block in blocks {
        let block = if crate::channels::discord::text::utf16_len(&block) > budget {
            crate::channels::discord::text::prefix_within_limit(&block, budget).to_string()
        } else {
            block
        };
        let joined = if current.is_empty() {
            block.clone()
        } else {
            format!("{current}\n\n{block}")
        };
        if crate::channels::discord::text::utf16_len(&joined) <= budget {
            current = joined;
        } else {
            out.push(std::mem::take(&mut current));
            current = block;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Seconds to wait after the `strikes`-th refusal: 2, 4, 8 … capped at 30.
fn backoff(strikes: u32) -> f64 {
    2f64.powi(strikes.min(5) as i32).min(30.0)
}

/// "timed out" → "Timed out".
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `4.1s`, `1m 12s`.
fn duration(secs: f64) -> String {
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let whole = secs as u64;
    format!("{}m {:02}s", whole / 60, whole % 60)
}

/// The pressed message, flipped to "stopping…" for the interaction's own
/// UPDATE_MESSAGE response: grey accent, the Stop section replaced by a
/// line. The worker settles the turn properly on its next frame. `None`
/// when the button is not on this message.
pub fn stopping(components: &Value, pressed: &str) -> Option<Vec<Value>> {
    let mut out = components.as_array()?.clone();
    let mut found = false;
    for top in &mut out {
        strip_nulls(top);
        if top.get("type").and_then(Value::as_u64) != Some(17)
            || top.get("spoiler").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let mut here = false;
        let Some(children) = top.get_mut("components").and_then(Value::as_array_mut) else {
            continue;
        };
        for child in children.iter_mut() {
            let is_stop = child.get("type").and_then(Value::as_u64) == Some(9)
                && child
                    .pointer("/accessory/custom_id")
                    .and_then(Value::as_str)
                    == Some(pressed);
            if is_stop {
                *child = json!({"type": 10, "content": "-# Stopping…"});
                here = true;
            }
        }
        if here {
            top["accent_color"] = json!(STOPPED);
            found = true;
        }
    }
    found.then_some(out)
}

/// Discord (and twilight) echo optional fields as `null`; the validator
/// reads a present key as set, so drop them before sending a tree back.
fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, field| !field.is_null());
            for field in map.values_mut() {
                strip_nulls(field);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// Cut one block's text into messages of at most `budget` UTF-16 units
/// (an overflowing preview is sealed the same way), so text never moves
/// between messages as it grows: a cut lands on the last newline that keeps
/// at least half the room, and a code fence open at the cut is closed there
/// and reopened in the next message. At most `MAX_SPLIT - 1` chunks;
/// whatever is left past them is returned as the second tuple element so a
/// pager can hold it instead of dropping it.
pub fn split_block(text: &str, budget: usize) -> (Vec<String>, Option<String>) {
    let mut chunks: Vec<String> = Vec::new();
    let mut rest = text.to_string();
    while !rest.trim().is_empty() {
        if crate::channels::discord::text::utf16_len(&rest) <= budget {
            chunks.push(rest);
            break;
        }
        if chunks.len() + 1 == MAX_SPLIT {
            let overflow = rest.trim().to_string();
            chunks.retain(|chunk| !chunk.trim().is_empty());
            return (chunks, (!overflow.is_empty()).then_some(overflow));
        }
        let (head, tail) = split_at(&rest, budget);
        chunks.push(head);
        rest = tail;
    }
    chunks.retain(|chunk| !chunk.trim().is_empty());
    (chunks, None)
}

/// Cut `text` to fit `room`, returning the head and what remains.
fn split_at(text: &str, room: usize) -> (String, String) {
    // Room for a closing fence below the cut.
    let budget = room.saturating_sub(4).max(2);
    let head = crate::channels::discord::text::prefix_within_limit(text, budget);
    let cut = match head.rfind('\n') {
        Some(at) if at > 0 && at >= head.len() / 2 => at,
        _ => head.len(),
    };
    let (closed, open) = close_fence(&text[..cut]);
    let rest = text[cut..].trim_start_matches('\n');
    let rest = match open {
        Some(fence) => format!("{fence}\n{rest}"),
        None => rest.to_string(),
    };
    (closed, rest)
}

/// Close a code fence left open at the end of `chunk`; returns the chunk
/// and the fence line to reopen, if any.
fn close_fence(chunk: &str) -> (String, Option<String>) {
    let mut open: Option<&str> = None;
    for line in chunk.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            open = match open {
                Some(_) => None,
                None => Some(trimmed),
            };
        }
    }
    match open {
        Some(fence) => (format!("{chunk}\n```"), Some(fence.to_string())),
        None => (chunk.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(segment: u64, delta: &str, tail: &str, done: bool) -> Value {
        json!({"phase": "text", "segment": segment, "delta": delta, "tail": tail, "done": done})
    }

    fn ran(id: &str, command: &str) -> Value {
        json!({"phase": "tool_ran", "call_id": id, "tool": "bash", "detail": command})
    }

    /// Apply every op as if Discord accepted it.
    fn land(timeline: &mut Timeline, now: f64, finishing: bool) -> Vec<Op> {
        let ops = timeline.plan(now, 1.0, finishing);
        for op in &ops {
            match op {
                Op::Post { card, body } => {
                    let id = format!("m{card}");
                    timeline.posted(*card, &id, body, now);
                }
                Op::Edit { card, body, .. } => timeline.edited(*card, body, now),
                Op::Delete { card, .. } => timeline.deleted(*card),
            }
        }
        ops
    }

    /// The status chip: the last component of the newest message.
    fn chip(card: &Card) -> &Value {
        card.components.last().unwrap()
    }

    fn status_line(card: &Card) -> String {
        let first = &chip(card)["components"][0];
        match first["type"].as_u64() {
            Some(9) => first["components"][0]["content"]
                .as_str()
                .unwrap()
                .to_string(),
            _ => first["content"].as_str().unwrap().to_string(),
        }
    }

    #[test]
    fn prose_streams_into_its_own_message_with_the_status_chip() {
        let mut t = Timeline::new(true, 0.0).with_stop("turn:stop:t".into());
        t.absorb(&text(0, "", "Hello", false));
        let ops = land(&mut t, 0.0, false);
        let [Op::Post { body, .. }] = &ops[..] else {
            panic!("{ops:?}")
        };
        crate::channels::discord::render::validate_components(&body.components).unwrap();
        assert_eq!(body.text, "Hello ▉");
        assert_eq!(body.components[0]["type"], 10, "prose is plain text");
        assert_eq!(chip(body)["type"], 17);
        assert_eq!(chip(body)["accent_color"], json!(WORKING));
        let stop = &chip(body)["components"][0];
        assert_eq!(stop["type"], 9, "the status line carries Stop");
        assert_eq!(stop["accessory"]["custom_id"], "turn:stop:t");

        t.absorb(&text(0, "Hello world\n", "and", false));
        assert!(t.plan(0.5, 1.0, false).is_empty(), "inside the edit gap");
        let ops = land(&mut t, 1.0, false);
        assert!(
            matches!(&ops[..], [Op::Edit { body, .. }] if body.text == "Hello world\nand ▉"),
            "{ops:?}"
        );
    }

    #[test]
    fn each_step_of_the_turn_is_a_new_message() {
        let mut t = Timeline::new(true, 0.0).with_stop("turn:stop:t".into());
        t.absorb(&text(0, "Let me check.", "", false));
        land(&mut t, 0.0, false);
        t.absorb(&ran("a", "cargo test"));
        let ops = land(&mut t, 2.0, false);
        let [
            Op::Edit {
                card: 0,
                body: prose,
                ..
            },
            Op::Post {
                card: 1,
                body: tools,
            },
        ] = &ops[..]
        else {
            panic!("{ops:?}")
        };
        assert_eq!(prose.text, "Let me check.", "closed: cursor gone");
        assert_eq!(prose.components.len(), 1, "the chip moved on");
        assert_eq!(tools.text, "**Running** `cargo test`");
        assert_eq!(
            status_line(tools),
            "-# Working · started <t:0:R> · ran 1 command"
        );

        t.absorb(&text(1, "", "All", false));
        let ops = land(&mut t, 4.0, false);
        assert!(
            matches!(&ops[..], [Op::Edit { card: 1, .. }, Op::Post { card: 2, body }] if body.text == "All ▉"),
            "the next prose opens a third message: {ops:?}"
        );
        assert_eq!(t.cards.len(), 3);
    }

    #[test]
    fn the_live_clock_never_costs_an_edit() {
        let mut t = Timeline::new(true, 1_700_000_000.0);
        t.absorb(&text(0, "", "Hi", false));
        land(&mut t, 1_700_000_000.0, false);
        assert_eq!(
            status_line(&t.render(1_700_000_000.0)[0]),
            "-# Working · started <t:1700000000:R>",
            "Discord keeps the relative time current itself"
        );
        assert!(
            t.plan(1_700_000_600.0, 1.0, false).is_empty(),
            "ten idle minutes, no edits"
        );
    }

    #[test]
    fn tool_calls_are_listed_in_full_not_folded() {
        let mut t = Timeline::new(true, 0.0);
        for i in 0..9 {
            t.absorb(&ran(&format!("c{i}"), &format!("step {i}")));
        }
        let card = &t.render(0.0)[0];
        crate::channels::discord::render::validate_components(&card.components).unwrap();
        let expected: Vec<String> = (0..9).map(|i| format!("**Running** `step {i}`")).collect();
        assert_eq!(card.text, expected.join("\n\n"));
    }

    #[test]
    fn a_finished_turn_drops_the_chip_and_the_answer_stands_alone() {
        let mut t = Timeline::new(true, 0.0).with_stop("turn:stop:t".into());
        t.absorb(&ran("a", "cargo test"));
        t.absorb(&text(0, "", "Draft", false));
        land(&mut t, 0.0, false);
        assert_eq!(t.cards.len(), 2);
        assert!(t.adopt("Final answer"));
        t.settle(Status::Done, 4.1);
        let ops = land(&mut t, 9.0, true);
        let [Op::Edit { card: 1, body, .. }] = &ops[..] else {
            panic!("{ops:?}")
        };
        assert!(
            body.components.iter().all(|c| c["type"] == 10),
            "no chip, no actions: {:?}",
            body.components
        );
        assert!(!body.key().contains("turn:"));
        assert_eq!(t.status_card(9.0), None, "a done turn keeps no notice");
        let landed = t.landed(30.0).unwrap();
        assert_eq!(
            landed.parts,
            vec![("Final answer".to_string(), "m1".to_string())],
            "only the answer's messages are the answer"
        );
    }

    #[test]
    fn an_answer_after_tool_lines_is_a_new_message_below_them() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, "Checking", "", true));
        t.absorb(&ran("a", "ls -la"));
        land(&mut t, 0.0, false);
        assert!(t.adopt("Answer"));
        t.settle(Status::Done, 1.0);
        let ops = land(&mut t, 1.0, true);
        assert!(
            matches!(&ops[..], [Op::Edit { card: 1, .. }, Op::Post { card: 2, body }] if body.text == "Answer"),
            "{ops:?}"
        );
        assert_eq!(
            t.landed(1.0).unwrap().parts,
            vec![("Answer".to_string(), "m2".to_string())]
        );
    }

    #[test]
    fn a_finished_turn_never_carries_buttons() {
        let mut t = Timeline::new(true, 0.0).with_stop("turn:stop:s".into());
        t.absorb(&text(0, "", "Hi", false));
        land(&mut t, 0.0, false);
        t.adopt("Hi there");
        t.settle(Status::Done, 2.0);
        land(&mut t, 2.0, true);
        let done = t.render(2.0);
        crate::channels::discord::render::validate_components(&done[0].components).unwrap();
        let flat = serde_json::to_string(&done[0].components).unwrap();
        assert!(!flat.contains("custom_id"), "nothing to press: {flat}");
    }

    #[test]
    fn the_answer_files_sit_inside_its_message() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, "", "Here", false));
        land(&mut t, 0.0, false);
        t.adopt("Here it is:");
        t.attach(vec![
            Media {
                name: "chart.png".into(),
                visual: true,
            },
            Media {
                name: "report.pdf".into(),
                visual: false,
            },
        ]);
        t.settle(Status::Done, 1.0);
        let ops = land(&mut t, 1.0, true);
        let [Op::Edit { body, .. }] = &ops[..] else {
            panic!("{ops:?}")
        };
        crate::channels::discord::render::validate_components(&body.components).unwrap();
        assert_eq!(body.files, vec!["chart.png", "report.pdf"]);
        assert_eq!(body.components[1]["type"], 12, "gallery after the prose");
        assert_eq!(
            body.components[1]["items"][0]["media"]["url"],
            "attachment://chart.png"
        );
        assert_eq!(body.components[2]["type"], 13, "then the file card");
        assert_eq!(body.components.len(), 3, "no chip after the files");
        assert!(t.landed(1.0).is_some());
    }

    #[test]
    fn stop_flips_the_pressed_message_at_once() {
        let mut t = Timeline::new(true, 0.0).with_stop("turn:stop:s".into());
        t.absorb(&text(0, "", "Working", false));
        let mut echoed = json!(t.render(0.0)[0].components);
        // Discord echoes optional fields back as null.
        echoed[0]["id"] = Value::Null;
        let flipped = stopping(&echoed, "turn:stop:s").unwrap();
        crate::channels::discord::render::validate_components(&flipped).unwrap();
        assert_eq!(flipped.last().unwrap()["accent_color"], json!(STOPPED));
        let text = serde_json::to_string(&flipped).unwrap();
        assert!(text.contains("Stopping…") && !text.contains("turn:stop:s"));
        assert!(stopping(&echoed, "turn:stop:other").is_none());
    }

    #[test]
    fn a_stopped_turn_says_so_in_its_chip() {
        let mut t = Timeline::new(true, 0.0).with_stop("turn:stop:t".into());
        t.absorb(&text(0, "", "Working on", false));
        land(&mut t, 0.0, false);
        t.settle(Status::Stopped, 12.0);
        land(&mut t, 12.5, true);
        let card = &t.render(99.0)[0];
        assert_eq!(chip(card)["accent_color"], json!(STOPPED));
        assert_eq!(
            status_line(card),
            "-# Stopped after 12.0s · actions may already have happened"
        );
        assert_eq!(card.text, "Working on", "the cursor is gone");
        assert_eq!(t.status_card(99.0).as_deref(), Some("m0"));
    }

    #[test]
    fn prose_rows_are_ignored_when_streaming_is_off() {
        let mut t = Timeline::new(false, 0.0);
        t.absorb(&text(0, "secret plan", "", true));
        assert!(!t.has_text());
        assert!(t.plan(0.0, 1.0, true).is_empty());
        assert!(!t.adopt("answer"), "the answer belongs to someone else");
    }

    #[test]
    fn a_lost_turn_is_left_alone_and_never_lands() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, "", "Hi", false));
        t.lose();
        assert!(t.plan(5.0, 1.0, true).is_empty());
        assert!(!t.adopt("Hi there"));
        assert!(t.landed(5.0).is_none());
    }

    #[test]
    fn a_refused_edit_backs_off_then_leaves_only_that_message() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, "", "One", false));
        land(&mut t, 0.0, false);
        t.absorb(&text(0, "One two\n", "", false));
        let [Op::Edit { card: 0, .. }] = &t.plan(1.0, 1.0, false)[..] else {
            panic!()
        };
        t.edit_failed(0, Failure::Refused, 1.0);
        assert!(t.plan(2.0, 1.0, false).is_empty(), "backing off");
        assert_eq!(t.plan(3.0, 1.0, false).len(), 1, "then tried again");
        t.edit_failed(0, Failure::Refused, 3.0);
        t.edit_failed(0, Failure::Refused, 30.0);
        assert!(t.plan(60.0, 1.0, false).is_empty(), "left as it is");

        // The turn goes on: the next step still lands as a new message.
        t.absorb(&ran("a", "cargo build"));
        let ops = land(&mut t, 61.0, false);
        assert!(matches!(&ops[..], [Op::Post { card: 1, .. }]), "{ops:?}");
    }

    #[test]
    fn a_busy_discord_is_waited_out_not_given_up() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, "", "Hi", false));
        let [Op::Post { .. }] = &t.plan(0.0, 1.0, false)[..] else {
            panic!()
        };
        t.post_failed(Failure::Busy(5.0), 0.0);
        assert!(t.plan(1.0, 1.0, false).is_empty());
        assert_eq!(t.plan(5.0, 1.0, false).len(), 1);
        for _ in 0..10 {
            t.post_failed(Failure::Busy(1.0), 5.0);
        }
        assert_eq!(t.plan(9.0, 1.0, false).len(), 1, "never lost to a busy API");
    }

    #[test]
    fn a_deleted_newest_message_is_posted_again() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, "", "Hi", false));
        land(&mut t, 0.0, false);
        t.absorb(&text(0, "Hi there\n", "", false));
        t.edit_failed(0, Failure::Gone, 1.0);
        let ops = land(&mut t, 1.0, false);
        assert!(matches!(&ops[..], [Op::Post { card: 0, .. }]), "{ops:?}");
    }

    #[test]
    fn a_forbidden_channel_ends_the_turns_messages() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, "", "Hi", false));
        t.post_failed(Failure::Forbidden, 0.0);
        assert!(t.plan(99.0, 1.0, true).is_empty());
    }

    #[test]
    fn a_long_block_fills_messages_in_order_and_balances_fences() {
        let line = "x".repeat(99);
        let mut body = String::from("```rust\n");
        for _ in 0..50 {
            body.push_str(&line);
            body.push('\n');
        }
        body.push_str("```\nafter");
        let (chunks, overflow) = split_block(&body, CARD_TEXT);
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert!(overflow.is_none());
        assert!(chunks[0].ends_with("\n```"), "the cut closes its fence");
        assert!(chunks[1].starts_with("```rust\n"), "and reopens it");
        for chunk in &chunks {
            assert!(crate::channels::discord::text::utf16_len(chunk) <= CARD_TEXT);
        }
    }

    #[test]
    fn only_the_newest_message_carries_the_chip() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, &"word ".repeat(1000), "", false));
        let cards = t.render(0.0);
        assert_eq!(cards.len(), 3, "5000 chars at 2000 per message");
        for card in &cards {
            crate::channels::discord::render::validate_components(&card.components).unwrap();
        }
        for card in &cards[..2] {
            assert!(
                card.components.iter().all(|c| c["type"] == 10),
                "no chip: {:?}",
                card.components
            );
        }
        assert!(status_line(&cards[2]).starts_with("-# Working · started"));
    }

    #[test]
    fn a_runaway_block_is_capped() {
        let (chunks, overflow) = split_block(&"word ".repeat(20_000), CARD_TEXT);
        assert_eq!(chunks.len(), MAX_SPLIT - 1);
        let overflow = overflow.expect("the rest parks behind a pager");
        assert!(!overflow.contains("(truncated)"));
        let pages = crate::channels::discord::pager::paginate(
            &overflow,
            crate::channels::discord::pager::PAGE_LIMIT,
        );
        assert!(pages.len() > 1);
    }

    #[test]
    fn a_turn_of_many_steps_stops_opening_messages() {
        let mut t = Timeline::new(true, 0.0);
        for step in 0..40u64 {
            t.absorb(&text(step, &format!("step {step}"), "", true));
            t.absorb(&ran(&format!("c{step}"), &format!("make {step}")));
        }
        let cards = t.render(0.0);
        assert_eq!(cards.len(), MAX_MESSAGES);
        let last = cards.last().unwrap();
        crate::channels::discord::render::validate_components(&last.components).unwrap();
        assert!(
            last.text.ends_with("**Running** `make 39`"),
            "{}",
            last.text
        );
    }

    #[test]
    fn a_shorter_answer_retires_extra_messages() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, &"y".repeat(5000), "", false));
        land(&mut t, 0.0, false);
        assert_eq!(t.cards.len(), 3);
        t.adopt("short");
        t.settle(Status::Done, 0.1);
        let ops = land(&mut t, 0.1, true);
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::Delete { card: 1, .. }))
        );
        assert_eq!(t.landed(0.1).unwrap().parts.len(), 1);
    }

    #[test]
    fn an_answer_posted_fresh_after_edits_stop_still_lands() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, "", "working on it", false));
        land(&mut t, 0.0, false);
        // Discord flood-limits the edits; the turn stops touching the card.
        for _ in 0..FLOOD_STRIKES {
            t.edit_failed(0, Failure::Busy(1.0), 0.0);
        }
        t.adopt("the answer");
        t.settle(Status::Done, 0.1);
        let ops = land(&mut t, 0.1, true);
        assert!(
            ops.iter().any(|op| matches!(op, Op::Post { .. })),
            "the remainder went out as new messages: {ops:?}"
        );
        // The answer is on screen in its fresh message: not a durable repost.
        let landed = t.landed(0.1).expect("the answer is on screen");
        assert!(
            landed
                .parts
                .iter()
                .any(|(text, _)| text.contains("the answer"))
        );
        // A second finish stays stable: nothing more to send, still landed.
        assert!(t.plan(0.2, 1.0, true).is_empty());
        assert!(t.landed(0.2).is_some());
    }

    #[test]
    fn streamed_media_tags_stay_out_of_the_preview() {
        let mut t = Timeline::new(true, 0.0);
        t.absorb(&text(0, "Chart:\nMEDIA:/tmp/a.png\n", "", false));
        assert_eq!(t.render(0.0)[0].text, "Chart: ▉");
    }
}
