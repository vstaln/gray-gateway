//! Owner-only gateway with durable generation and delivery workers.
//!
//! Discord gateway loop.

use crate::channels::discord::component_input::{
    NormalizedInteraction, normalize, normalize_autocomplete,
};
use crate::channels::discord::component_media::FileStore;
use crate::channels::discord::component_state::NewEvent;
use crate::channels::discord::durable::{OutboxPart, Store};
use crate::channels::discord::runner::{RunError, RunInput};
use crate::channels::discord::transport::{Rest, TransportError};
use serde_json::Value;
use std::path::{Path, PathBuf};
use twilight_gateway::{EventTypeFlags, Intents, Shard, ShardId, StreamExt};
use twilight_model::application::interaction::InteractionType;
use twilight_model::gateway::event::Event;

struct NormalizedRoute<'a> {
    raw: &'a Value,
    interaction_token: &'a str,
    app_id: &'a str,
    rest: &'a Rest,
    store: &'a Store,
    file_store: Option<&'a FileStore>,
    capacity: u64,
    is_dm: bool,
}

async fn route_normalized_interaction(route: NormalizedRoute<'_>) -> Result<(), String> {
    let NormalizedRoute {
        raw,
        interaction_token,
        app_id,
        rest,
        store,
        file_store,
        capacity,
        is_dm,
    } = route;
    let is_autocomplete = raw.get("type").and_then(Value::as_u64) == Some(4);
    if is_autocomplete {
        let values = normalize_autocomplete(raw).map_err(|error| error.to_string())?;
        let resolved = store
            .resolve_state(&values.state_token, &values.user_id, &values.channel_id)
            .map_err(|error| error.to_string())?;
        let conversation = crate::channels::discord::session::conversation_key(
            &values.channel_id,
            &values.user_id,
            is_dm,
        );
        let payload = serde_json::json!({"values": values.values});
        let _receipt = store
            .accept_event(NewEvent {
                interaction_id: values.interaction_id.clone(),
                token: values.state_token.clone(),
                user_id: values.user_id.clone(),
                channel_id: values.channel_id.clone(),
                kind: "autocomplete".to_string(),
                payload,
                conversation,
                interaction_token: Some(interaction_token.to_string()),
                app_id: Some(app_id.to_string()),
                capacity,
            })
            .map_err(|error| error.to_string())?;
        let choices = autocomplete_choices(&values.values, &resolved.state);
        return rest
            .autocomplete_response(&values.interaction_id, interaction_token, &choices)
            .await
            .map_err(|error| error.to_string());
    }

    let mut normalized = normalize(raw).map_err(|error| error.to_string())?;
    let _resolved = store
        .resolve_state(
            &normalized.values().state_token,
            &normalized.values().user_id,
            &normalized.values().channel_id,
        )
        .map_err(|error| error.to_string())?;
    let attachments = normalized.values().attachments.clone();
    let mut managed_files = Vec::new();
    if !attachments.is_empty() {
        let file_store = file_store.ok_or_else(|| "managed media is unavailable".to_string())?;
        for attachment in attachments {
            let file = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                file_store.import_url_authenticated(
                    &normalized.values().user_id,
                    &attachment.url,
                    Some(interaction_token),
                ),
            )
            .await
            .map_err(|_| "managed attachment import timed out".to_string())?
            .map_err(|error| error.to_string())?;
            managed_files.push(file.id);
        }
    } else if !normalized.values().files.is_empty() {
        return Err("managed media is unavailable".to_string());
    }
    normalized.values_mut().files = managed_files;
    let values = normalized.values();
    let mut payload = serde_json::json!({"values": values.values});
    if !values.files.is_empty() {
        payload["files"] = serde_json::json!(values.files);
    }
    let conversation = crate::channels::discord::session::conversation_key(
        &values.channel_id,
        &values.user_id,
        is_dm,
    );
    let _receipt = store
        .accept_event(NewEvent {
            interaction_id: values.interaction_id.clone(),
            token: values.state_token.clone(),
            user_id: values.user_id.clone(),
            channel_id: values.channel_id.clone(),
            kind: normalized.kind().to_string(),
            payload,
            conversation,
            interaction_token: Some(interaction_token.to_string()),
            app_id: Some(app_id.to_string()),
            capacity,
        })
        .map_err(|error| error.to_string())?;
    match normalized {
        NormalizedInteraction::ModalSubmit(_) => rest
            .defer_update(&values.interaction_id, interaction_token)
            .await
            .map_err(|error| error.to_string()),
        NormalizedInteraction::Button(_) | NormalizedInteraction::Select(_) => rest
            .defer_v2(&values.interaction_id, interaction_token, true)
            .await
            .map_err(|error| error.to_string()),
        NormalizedInteraction::Autocomplete(_) => unreachable!("autocomplete handled above"),
    }
}

fn autocomplete_choices(values: &Value, state: &Value) -> Vec<(String, String)> {
    let query = values
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    state
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| {
            let name = option.get("label").and_then(Value::as_str)?;
            let value = option.get("value").and_then(Value::as_str)?;
            if !query.is_empty()
                && !name.to_ascii_lowercase().contains(&query)
                && !value.to_ascii_lowercase().contains(&query)
            {
                return None;
            }
            Some((name.to_string(), value.to_string()))
        })
        .take(25)
        .collect()
}

pub fn jobs_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("jobs.json")
}

pub fn open_store(config_path: &Path) -> Result<Store, String> {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let store = Store::new(&parent.join("queue.sqlite"))?;
    let legacy = jobs_path(config_path);
    store.migrate_jobs(&legacy)?;
    Ok(store)
}

/// Registration JSON for every slash command the bridge serves: the
/// bridge-owned rows from `commands::COMMANDS` merged with whatever
/// `gray cmd` advertises (or the full static table when `cmd` is absent).
/// The hash of this body is what Discord sees, so a changed merge
/// re-registers exactly once.
pub fn slash_commands_json(
    registry: Option<&[crate::channels::discord::graycmd::CmdInfo]>,
) -> Value {
    crate::channels::discord::graycmd::registration_json(registry)
}

pub type ReactionHook = std::sync::Arc<dyn Fn(&str, &str, &str) + Send + Sync>;
pub type TypingHook = std::sync::Arc<dyn Fn(u64) + Send + Sync>;
pub type ClockFn = std::sync::Arc<dyn Fn() -> f64 + Send + Sync>;
/// Activity hook: (activity text, whether this was an edit of the live bubble).
/// A final card uses the same hook with `false` because it is a new message.
pub type ActivityHook = std::sync::Arc<dyn Fn(&str, bool) + Send + Sync>;

const BUSY_BACKOFF_SECS: f64 = 3.0;
const QUEUED_ACK_SECS: f64 = 30.0;

pub fn steer_command(text: &str) -> Option<String> {
    let rest = text.trim_start().strip_prefix("/steer")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim().to_string())
}

pub fn busy_input_steer(config: &Value) -> bool {
    config.get("busy_input_mode").and_then(Value::as_str) == Some("steer")
}

pub fn queued_notice(
    store: &Store,
    acks: &mut std::collections::HashMap<String, f64>,
    conversation: &str,
    id: &str,
) -> Option<String> {
    let ahead = store.ahead_in_conversation(conversation, id).ok()?;
    if ahead <= 0 {
        return None;
    }
    let now = crate::channels::discord::durable::now_secs();
    if acks
        .get(conversation)
        .is_some_and(|at| now - at < QUEUED_ACK_SECS)
    {
        return None;
    }
    acks.retain(|_, at| now - *at < QUEUED_ACK_SECS);
    acks.insert(conversation.to_string(), now);
    Some(format!("Queued — {ahead} ahead."))
}

fn activity_key(channel: &str, conversation: &str) -> String {
    format!("{channel}\u{0}{conversation}")
}

/// Final delivery that Discord refused, carried off the turn's worker (see
/// `Runtime::settle_job`).
struct SettleJob {
    channel: String,
    /// The timeline's own slot (see `settle_slot`).
    slot: String,
    uploads: Vec<crate::channels::discord::media_tags::Upload>,
    /// The answer goes out durably instead: its preview is retracted, and its
    /// messages are left out of the retries.
    durable_answer: bool,
    /// The wait Discord asked for when the card was refused, if it was.
    first_wait: Option<f64>,
    /// The activity card, sent once the turn's messages have settled.
    card: Option<String>,
}

/// Retries for one refused message before a turn is left as it stands.
const SETTLE_ROUNDS: u32 = 10;
/// Longest a turn's own Discord send waits on a rate limit. Past it the send
/// reports busy and the settle path retries, so the worker never sleeps on
/// Discord's limits.
const TURN_RATE_BUDGET: std::time::Duration = std::time::Duration::from_millis(750);

static SETTLE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A slot of its own for a settling timeline: settling never touches the
/// channel's slot, which the next turn uses.
fn settle_slot(key: &str) -> String {
    let seq = SETTLE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("settle\u{0}{seq}\u{0}{key}")
}

/// The `value` of the modal input `custom_id` anywhere in a submitted
/// modal's components (a Label wraps its input one level down).
fn find_value(value: &Value, custom_id: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if map.get("custom_id").and_then(Value::as_str) == Some(custom_id)
                && let Some(text) = map.get("value").and_then(Value::as_str)
            {
                return Some(text.to_string());
            }
            map.values().find_map(|child| find_value(child, custom_id))
        }
        Value::Array(items) => items.iter().find_map(|child| find_value(child, custom_id)),
        _ => None,
    }
}

/// How a turn's card settled.
#[derive(Debug, Default)]
pub struct Settled {
    /// The answer streamed into the card and is on screen as rendered.
    pub landed: Option<crate::channels::discord::stream::Landed>,
    /// The card whose footer shows how the turn ended, when it is on screen.
    pub status_card: Option<String>,
    /// How many of the offered uploads the landed card shows; the rest still
    /// need delivering.
    pub media_shown: usize,
}

/// The `custom_id` of a turn card's Stop button, minted per turn.
#[derive(Debug, Clone, Default)]
pub struct TurnButtons {
    pub stop: Option<String>,
}

/// What a failed turn-message request means for the turn (the edit
/// failure classes): rate limits, 5xx, timeouts and dropped connections are
/// waited out; a refused payload is retried with the next frame; a missing
/// message or a forbidden channel is final.
fn failure(error: &TransportError) -> crate::channels::discord::stream::Failure {
    use crate::channels::discord::stream::Failure;
    match error {
        TransportError::RateLimited(after) => Failure::Busy(after.unwrap_or(1.0)),
        TransportError::Net(_) => Failure::Busy(2.0),
        TransportError::Http(429, _) => Failure::Busy(1.0),
        TransportError::Http(code, _) if *code >= 500 => Failure::Busy(2.0),
        TransportError::Http(404, _) => Failure::Gone,
        TransportError::Auth(_) | TransportError::Forbidden(_) => Failure::Forbidden,
        TransportError::Http(_, _) | TransportError::Invalid(_) => Failure::Refused,
    }
}

/// One line on the daemon's stderr for a turn message that did not land,
/// with Discord's own reason when it gave one. Never message content.
fn log_failure(what: &str, channel: &str, error: &TransportError) {
    let detail = match error {
        TransportError::Http(code, detail) => format!("HTTP {code}: {detail}"),
        TransportError::Net(detail) => format!("network: {detail}"),
        TransportError::RateLimited(after) => format!("rate limited, retry after {after:?}s"),
        TransportError::Invalid(detail) => format!("not sent: {detail}"),
        TransportError::Auth(_) => "unauthorized".to_string(),
        TransportError::Forbidden(_) => "forbidden".to_string(),
    };
    crate::channels::discord::eprintln_stamped!(
        "[discord] turn message {what} failed in channel {channel}: {detail}"
    );
}

#[derive(Clone)]
pub struct Runtime<R, D> {
    pub config: Value,
    pub config_path: PathBuf,
    pub store: Store,
    pub deliver: D,
    pub runner: R,
    pub rest: Option<Rest>,
    pub reaction_hook: Option<ReactionHook>,
    pub typing_hook: Option<TypingHook>,
    pub activity_hook: Option<ActivityHook>,
    pub clock: Option<ClockFn>,
    /// Rows the runner has read but nobody has narrated yet.
    pub activity: Option<crate::channels::discord::activity::Sink>,
    last_typing: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, f64>>>,
    /// The live turn per channel/conversation: prose messages and tool
    timelines: std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<String, crate::channels::discord::stream::Timeline>,
        >,
    >,
    busy_backoff: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, f64>>>,
    /// Final deliveries Discord refused, waiting for `dispatch_settles`.
    settle: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<SettleJob>>>,
}

impl<R, D> Runtime<R, D> {
    pub fn new(config: Value, config_path: PathBuf, store: Store, deliver: D, runner: R) -> Self {
        Self {
            config,
            config_path,
            store,
            deliver,
            runner,
            rest: None,
            reaction_hook: None,
            typing_hook: None,
            activity_hook: None,
            clock: None,
            activity: None,
            last_typing: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            timelines: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            busy_backoff: Default::default(),
            settle: Default::default(),
        }
    }

    /// Wire the runner's progress sink: gray's rows in, the live bubble and
    /// final tool card out. Without it the plugin still runs (typing only).
    pub fn with_activity(mut self, sink: crate::channels::discord::activity::Sink) -> Self {
        self.activity = Some(sink);
        self
    }

    pub fn with_activity_hook(mut self, hook: ActivityHook) -> Self {
        self.activity_hook = Some(hook);
        self
    }

    pub fn with_rest(mut self, rest: Rest) -> Self {
        self.rest = Some(rest);
        self
    }

    pub fn with_reaction_hook(mut self, hook: ReactionHook) -> Self {
        self.reaction_hook = Some(hook);
        self
    }

    pub fn with_typing_hook(mut self, hook: TypingHook) -> Self {
        self.typing_hook = Some(hook);
        self
    }

    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = Some(clock);
        self
    }

    pub fn now_secs(&self) -> f64 {
        self.clock
            .as_ref()
            .map(|c| c())
            .unwrap_or_else(crate::channels::discord::durable::now_secs)
    }

    /// The Discord typing indicator, on by default.
    ///
    /// Same key
    /// name, same default (on), same gate placement (the adapter refuses
    /// before any typing RPC, so turning it off kills the whole path rather
    /// than one loop of it). Set `"typing_indicator": false` in config.json
    /// and the bot never pokes `/channels/<id>/typing`, so Discord drops the
    /// bubble instead of showing it through most turns — the 8s throttle
    /// below re-pokes it for as long as work is in progress, which is what
    /// reads as "always typing" from the other side.
    pub fn typing_enabled(&self) -> bool {
        self.config
            .get("typing_indicator")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    pub async fn report_progress(&self, channel: &str) {
        if !self.typing_enabled() {
            return;
        }
        let now = self.now_secs();
        let mut map = self.last_typing.lock().await;
        let last = map.get(channel).copied().unwrap_or(-10.0);
        if now - last >= 8.0 {
            map.insert(channel.to_string(), now);
            drop(map);
            if let Ok(ch) = channel.parse::<u64>() {
                if let Some(ref rest) = self.rest {
                    rest.typing(ch).await;
                }
                if let Some(ref hook) = self.typing_hook {
                    hook(ch);
                }
            }
        }
    }

    /// Where a relative `MEDIA:` path resolves: the configured workdir, the
    /// same root the delivery worker uses.
    fn media_cwd(&self) -> PathBuf {
        PathBuf::from(
            self.config
                .get("workdir")
                .and_then(Value::as_str)
                .unwrap_or("."),
        )
    }

    fn media_roots(&self) -> Vec<PathBuf> {
        crate::channels::discord::media_tags::roots_from_config(&self.config)
    }

    /// The gray home for one conversation (the same layout `run_gray`
    /// builds), or `None` when the layout cannot be derived.
    pub fn conversation_home(&self, conversation: &str) -> Option<std::path::PathBuf> {
        let base = self.config_path.parent()?;
        Some(
            base.join("conversations")
                .join(crate::channels::discord::runner::hex_sha256(
                    conversation.as_bytes(),
                )),
        )
    }

    /// The live session id for a conversation, when a turn has created one.
    fn session_id(&self, conversation: &str) -> Option<String> {
        let home = self.conversation_home(conversation)?;
        let state = std::fs::read(home.join("session.json")).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&state).ok()?;
        v.get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    /// Narrate whatever the agent just did: tool lines in a live bubble, and
    /// (with streamed prose) the reply itself, all edited in place.
    /// Best-effort throughout; live narration can never fail a turn.
    pub async fn report_activity(&self, channel: &str) {
        self.report_activity_for(channel, "", false).await
    }

    /// Start a fresh card and row history for one turn. Prose streams
    /// only when `text` is set: a slash-command turn answers through its
    /// interaction, so its card carries tool lines only.
    pub async fn begin_activity(&self, channel: &str, conversation: &str) {
        self.begin_turn(
            channel,
            conversation,
            crate::channels::discord::stream::enabled(&self.config),
            TurnButtons::default(),
        )
        .await
    }

    /// `buttons` carries the card's Stop `custom_id`, when one was minted.
    pub async fn begin_turn(
        &self,
        channel: &str,
        conversation: &str,
        text: bool,
        buttons: TurnButtons,
    ) {
        if let Some(ref sink) = self.activity {
            crate::channels::discord::activity::begin(sink, conversation);
        }
        let key = activity_key(channel, conversation);
        // A new turn always gets a fresh timeline (`started` = now); any
        // leftover one for this conversation is replaced, never reused.
        let mut timeline = crate::channels::discord::stream::Timeline::new(text, self.now_secs())
            .with_quiet(crate::channels::discord::stream::quiet(&self.config))
            .with_progress(crate::channels::discord::activity::mode(&self.config));
        if let Some(stop) = buttons.stop {
            timeline = timeline.with_stop(stop);
        }
        self.timelines.lock().await.insert(key, timeline);
    }

    /// `force` settles the turn as done: every pending edit goes out
    /// regardless of the edit gap, the cursor comes off, and the timeline is
    /// retired.
    pub async fn report_activity_at(&self, channel: &str, force: bool) {
        self.report_activity_for(channel, "", force).await
    }

    /// Activity rows are scoped by conversation, not merely channel. A
    /// daemon can run several channels concurrently; one global queue would
    /// let a fast turn drain or finalize another turn's terminal output.
    pub async fn report_activity_for(&self, channel: &str, conversation: &str, force: bool) {
        if force {
            self.finish_turn(
                channel,
                conversation,
                None,
                &[],
                crate::channels::discord::stream::Status::Done,
            )
            .await;
        } else {
            self.absorb_rows(channel, conversation).await;
            self.publish_timeline(channel, conversation, false, &[])
                .await;
        }
    }

    /// Keep one turn's messages current until `stop` is signalled: gray's
    /// rows in, sends and edits out, about four frames a second. Runs beside
    /// the gray child rather than between reads of its output (the bridge's
    /// stream consumer is its own task for the same reason): a slow or
    /// rate-limited Discord request must never stall the agent or the rows
    /// queued behind it. Returns after the request in flight has finished,
    /// so a post is never cut off halfway and sent twice.
    pub async fn drive_turn(&self, channel: &str, conversation: &str, stop: &tokio::sync::Notify) {
        let mut every = tokio::time::interval(std::time::Duration::from_millis(250));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = stop.notified() => return,
                _ = every.tick() => {}
            }
            self.report_progress(channel).await;
            self.report_activity_for(channel, conversation, false).await;
        }
    }

    /// Settle the turn's card with `status`. With `answer` (prose, `MEDIA:`
    /// tags already taken out) the streamed reply becomes that answer in
    /// place, with `uploads` (the files those tags named) shown inside the
    /// card; `landed` says where it landed, or is `None` when the caller
    /// must post it durably. The worker makes one attempt; a refused card
    /// settles in the background (see `settle_job`), so Discord's waits
    /// never hold the worker or the next turn.
    pub async fn finish_turn(
        &self,
        channel: &str,
        conversation: &str,
        answer: Option<&str>,
        uploads: &[crate::channels::discord::media_tags::Upload],
        status: crate::channels::discord::stream::Status,
    ) -> Settled {
        self.absorb_rows(channel, conversation).await;
        let key = activity_key(channel, conversation);
        let slot = settle_slot(&key);
        let now = self.now_secs();
        let shown = uploads
            .len()
            .min(crate::channels::discord::stream::MAX_MEDIA);
        // The timeline moves to its own slot, so the channel is free for the
        // next turn at once.
        let adopted = {
            let mut timelines = self.timelines.lock().await;
            let Some(mut timeline) = timelines.remove(&key) else {
                drop(timelines);
                if let Some(card) = self.take_activity_card(conversation) {
                    self.card_later(channel, &slot, card);
                }
                return Settled::default();
            };
            let adopted = answer.is_some_and(|answer| timeline.adopt(answer));
            if adopted {
                timeline.attach(
                    uploads[..shown]
                        .iter()
                        .map(|upload| crate::channels::discord::stream::Media {
                            name: upload.name.clone(),
                            visual: upload.is_visual(),
                        })
                        .collect(),
                );
            }
            timeline.settle(status, now);
            timelines.insert(slot.clone(), timeline);
            adopted
        };
        // One attempt: whether the answer landed decides how the item completes.
        let busy = self.publish_at(channel, &slot, true, uploads, false).await;
        let (status_card, landed) = match self.timelines.lock().await.get(&slot) {
            Some(timeline) => (timeline.status_card(now), timeline.landed(now)),
            None => (None, None),
        };
        let durable = adopted && landed.is_none();
        let card = self.take_activity_card(conversation);
        if busy.is_some() || durable {
            self.settle_later(SettleJob {
                channel: channel.to_string(),
                slot,
                uploads: uploads.to_vec(),
                durable_answer: durable,
                first_wait: busy,
                card,
            });
        } else {
            self.timelines.lock().await.remove(&slot);
            if let Some(card) = card {
                self.card_later(channel, &slot, card);
            }
        }
        let mut settled = Settled {
            landed: None,
            status_card,
            media_shown: 0,
        };
        if !adopted {
            return settled;
        }
        if durable {
            crate::channels::discord::eprintln_stamped!(
                "[discord] the answer did not land in place in channel {channel}; posting it durably"
            );
            settled.status_card = None;
            return settled;
        }
        settled.landed = landed;
        if settled.landed.is_some() {
            settled.media_shown = shown;
        }
        settled
    }

    /// The turn's activity card text, when it shows one. Takes the turn's
    /// rows out of the sink; no Discord I/O.
    fn take_activity_card(&self, conversation: &str) -> Option<String> {
        let sink = self.activity.as_ref()?;
        let history = crate::channels::discord::activity::finish(sink, conversation);
        if crate::channels::discord::activity::enabled(&self.config)
            && crate::channels::discord::activity::card_enabled(&self.config)
        {
            crate::channels::discord::activity::render_card(&history)
        } else {
            None
        }
    }

    /// Queue a turn's activity card for `dispatch_settles`, off the worker.
    fn card_later(&self, channel: &str, slot: &str, card: String) {
        self.settle_later(SettleJob {
            channel: channel.to_string(),
            slot: slot.to_string(),
            uploads: Vec::new(),
            durable_answer: false,
            first_wait: None,
            card: Some(card),
        });
    }

    /// Queue a turn's remaining delivery for `dispatch_settles`.
    fn settle_later(&self, job: SettleJob) {
        self.settle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(job);
    }

    /// Run every queued settle inline, oldest first. The daemon never
    /// calls this — `dispatch_settles` drains the queue beside its workers —
    /// but a runtime driven without it (tests, one-shot turns) still needs
    /// refused previews retracted and turn cards sent.
    pub async fn drain_settles(&self) {
        loop {
            let next = self
                .settle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            match next {
                Some(job) => self.settle_job(job).await,
                None => return,
            }
        }
    }

    /// Finish a turn's delivery off its worker: retract the preview of an
    /// answer resent durably, retry the card while Discord asks for waits,
    /// then send the activity card and free the slot.
    async fn settle_job(&self, job: SettleJob) {
        if job.durable_answer {
            let ids = self
                .timelines
                .lock()
                .await
                .get(&job.slot)
                .map(|timeline| timeline.answer_ids())
                .unwrap_or_default();
            for id in ids {
                self.retract_message(&job.channel, &id).await;
            }
        }
        let mut wait = job.first_wait;
        let mut rounds = 0;
        while let Some(seconds) = wait {
            if rounds == SETTLE_ROUNDS {
                crate::channels::discord::eprintln_stamped!(
                    "[discord] the turn's card in channel {} did not settle; leaving it as is",
                    job.channel
                );
                break;
            }
            rounds += 1;
            tokio::time::sleep(std::time::Duration::from_secs_f64(seconds.clamp(0.5, 30.0))).await;
            wait = self
                .publish_at(
                    &job.channel,
                    &job.slot,
                    true,
                    &job.uploads,
                    job.durable_answer,
                )
                .await;
        }
        self.timelines.lock().await.remove(&job.slot);
        if let Some(card) = &job.card {
            self.publish_activity_card(&job.channel, card).await;
        }
    }

    /// Delete one message, waiting out Discord's rate limit a bounded number
    /// of times.
    async fn retract_message(&self, channel: &str, id: &str) {
        let (Some(rest), Ok(ch)) = (self.rest.as_ref(), channel.parse::<u64>()) else {
            return;
        };
        for _ in 0..SETTLE_ROUNDS {
            match rest.delete_message(ch, id).await {
                Ok(()) => return,
                Err(error) => match failure(&error) {
                    crate::channels::discord::stream::Failure::Busy(wait) => {
                        tokio::time::sleep(std::time::Duration::from_secs_f64(
                            wait.clamp(0.5, 30.0),
                        ))
                        .await;
                    }
                    _ => {
                        log_failure("delete", channel, &error);
                        return;
                    }
                },
            }
        }
    }

    /// Move the rows the runner has read into this turn's timeline.
    async fn absorb_rows(&self, channel: &str, conversation: &str) {
        let Some(sink) = self.activity.clone() else {
            return;
        };
        let rows = crate::channels::discord::activity::drain_for(&sink, conversation);
        let narrate = crate::channels::discord::activity::enabled(&self.config);
        let quiet = crate::channels::discord::stream::quiet(&self.config);
        let key = activity_key(channel, conversation);
        let now = self.now_secs();
        let mut timelines = self.timelines.lock().await;
        let timeline = timelines.entry(key).or_insert_with(|| {
            crate::channels::discord::stream::Timeline::new(false, now)
                .with_quiet(quiet)
                .with_progress(crate::channels::discord::activity::mode(&self.config))
        });
        for row in rows {
            // Narration off hides tool lines; quiet still absorbs them —
            // its status card shows the tally, just never the lines.
            if !narrate
                && !quiet
                && !matches!(
                    row.get("phase").and_then(Value::as_str),
                    Some("text") | Some("retract")
                )
            {
                continue;
            }
            timeline.absorb(&row);
        }
    }

    /// Send whatever the timeline says is due, in order. A message that
    /// shows files carries the matching `uploads` with its request. Returns
    /// how long to wait when Discord was busy and the frame stopped early.
    async fn publish_timeline(
        &self,
        channel: &str,
        conversation: &str,
        finishing: bool,
        uploads: &[crate::channels::discord::media_tags::Upload],
    ) -> Option<f64> {
        self.publish_at(
            channel,
            &activity_key(channel, conversation),
            finishing,
            uploads,
            false,
        )
        .await
    }

    /// `publish_timeline` for the timeline at `key`. With `hold_answer` the
    /// turn's answer is left out: it is being sent durably instead.
    async fn publish_at(
        &self,
        channel: &str,
        key: &str,
        finishing: bool,
        uploads: &[crate::channels::discord::media_tags::Upload],
        hold_answer: bool,
    ) -> Option<f64> {
        let files_for = |body: &crate::channels::discord::stream::Card| -> Vec<crate::channels::discord::media_tags::Upload> {
            body.files
                .iter()
                .filter_map(|name| uploads.iter().find(|upload| &upload.name == name))
                .cloned()
                .collect()
        };
        let Ok(ch) = channel.parse::<u64>() else {
            return None;
        };
        let now = self.now_secs();
        let rest = self
            .rest
            .as_ref()
            .map(|rest| rest.with_budget(TURN_RATE_BUDGET));
        let (ops, held) = match self.timelines.lock().await.get(key) {
            Some(timeline) => {
                let ops = timeline.plan(
                    now,
                    crate::channels::discord::activity::MIN_EDIT_GAP,
                    finishing,
                );
                // The answer is the turn's last block: its messages are the
                // ones in the newest block.
                let held = hold_answer
                    .then(|| timeline.rendered(now).iter().map(|card| card.block).max())
                    .flatten();
                (ops, held)
            }
            None => return None,
        };
        let ops: Vec<crate::channels::discord::stream::Op> = ops
            .into_iter()
            .filter(|op| match op {
                crate::channels::discord::stream::Op::Post { body, .. }
                | crate::channels::discord::stream::Op::Edit { body, .. } => {
                    Some(body.block) != held
                }
                crate::channels::discord::stream::Op::Delete { .. } => true,
            })
            .collect();
        let components_for = |body: &crate::channels::discord::stream::Card| -> Vec<Value> {
            let mut components = body.components.clone();
            if let Some(pages) = &body.pager
                && let Ok(token) = crate::channels::discord::pager::mint(
                    &self.store,
                    "*",
                    channel,
                    pages,
                    "",
                    crate::channels::discord::render::QUIET_ACCENT,
                )
                && let Some(row) = crate::channels::discord::pager::nav_row(&token, 0, pages.len())
            {
                components.push(row);
            }
            components
        };
        let mut busy: Option<f64> = None;
        for op in ops {
            match op {
                crate::channels::discord::stream::Op::Post { card, body } => {
                    let files = files_for(&body);
                    let components = components_for(&body);
                    let sent = match rest {
                        Some(ref rest) if files.is_empty() => {
                            rest.send_v2(ch, &components, None).await
                        }
                        Some(ref rest) => rest.send_v2_uploads(ch, &components, &files, None).await,
                        // No REST (tests, dry runs): still narrate via the hook.
                        None => Ok(format!("hook-{card}")),
                    };
                    let at = self.now_secs();
                    let mut timelines = self.timelines.lock().await;
                    let timeline = timelines.get_mut(key)?;
                    match sent {
                        Ok(id) => timeline.posted(card, &id, &body, at),
                        Err(error) => {
                            // Messages keep channel order, so nothing after
                            // this one goes out this frame.
                            log_failure("post", channel, &error);
                            let failure = failure(&error);
                            timeline.post_failed(failure, at);
                            if let crate::channels::discord::stream::Failure::Busy(wait) = failure {
                                return Some(wait);
                            }
                            return None;
                        }
                    }
                    drop(timelines);
                    // Discord drops the typing bubble when the bot posts;
                    // the next frame pokes it again (the bridge restores it the
                    // same way after each progress message).
                    if !finishing {
                        self.last_typing.lock().await.remove(channel);
                    }
                    if let Some(ref hook) = self.activity_hook {
                        hook(&body.text, false);
                    }
                }
                crate::channels::discord::stream::Op::Edit { card, id, body } => {
                    let files = files_for(&body);
                    let components = components_for(&body);
                    let outcome = match rest {
                        Some(ref rest) if files.is_empty() => {
                            rest.edit_v2(ch, &id, &components).await
                        }
                        Some(ref rest) => rest.edit_v2_uploads(ch, &id, &components, &files).await,
                        None => Ok(()),
                    };
                    let at = self.now_secs();
                    let mut timelines = self.timelines.lock().await;
                    let timeline = timelines.get_mut(key)?;
                    match outcome {
                        Ok(()) => timeline.edited(card, &body, at),
                        // Only this message is affected: the turn goes on,
                        // and its next step is a new message anyway.
                        Err(error) => {
                            log_failure("edit", channel, &error);
                            let failure = failure(&error);
                            timeline.edit_failed(card, failure, at);
                            if let crate::channels::discord::stream::Failure::Busy(wait) = failure {
                                busy = Some(busy.map_or(wait, |seen: f64| seen.max(wait)));
                            }
                            continue;
                        }
                    }
                    drop(timelines);
                    if let Some(ref hook) = self.activity_hook {
                        hook(&body.text, true);
                    }
                }
                crate::channels::discord::stream::Op::Delete { card, id } => {
                    if let Some(ref rest) = self.rest
                        && let Err(error) = rest.delete_message(ch, &id).await
                    {
                        log_failure("delete", channel, &error);
                    }
                    if let Some(timeline) = self.timelines.lock().await.get_mut(key) {
                        timeline.deleted(card);
                    }
                }
            }
        }
        busy
    }

    async fn publish_activity_card(&self, channel: &str, text: &str) {
        let Ok(ch) = channel.parse::<u64>() else {
            return;
        };
        let text = crate::channels::discord::text::sanitize(text);
        let components = match crate::channels::discord::render::tool_card(&text) {
            Ok(components) => components,
            Err(_) => return,
        };
        if let Some(ref rest) = self.rest
            && rest.send_v2(ch, &components, None).await.is_err()
        {
            return;
        }
        if let Some(ref hook) = self.activity_hook {
            hook(&text, false);
        }
    }

    pub async fn add_reaction(&self, channel: &str, message_id: &str, emoji: &str) {
        let enabled = self
            .config
            .get("reactions")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !enabled {
            return;
        }
        if let Ok(ch) = channel.parse::<u64>() {
            if let Some(ref rest) = self.rest {
                rest.add_reaction(ch, message_id, emoji).await;
            }
            if let Some(ref hook) = self.reaction_hook {
                hook("add", message_id, emoji);
            }
        }
    }

    pub async fn remove_reaction(&self, channel: &str, message_id: &str, emoji: &str) {
        let enabled = self
            .config
            .get("reactions")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !enabled {
            return;
        }
        if let Ok(ch) = channel.parse::<u64>() {
            if let Some(ref rest) = self.rest {
                rest.remove_reaction(ch, message_id, emoji).await;
            }
            if let Some(ref hook) = self.reaction_hook {
                hook("remove", message_id, emoji);
            }
        }
    }
}

impl<R, D, FutR, FutD> Runtime<R, D>
where
    R: Fn(&Value, &Path, &str, &RunInput) -> FutR,
    FutR: std::future::Future<Output = Result<String, RunError>>,
    D: Fn(OutboxPart) -> FutD,
    FutD: std::future::Future<Output = Result<String, String>>,
{
    pub async fn generate_one(&self) -> Result<bool, String> {
        let skip: Vec<String> = {
            let now = self.now_secs();
            let mut backoff = self.busy_backoff.lock().unwrap_or_else(|e| e.into_inner());
            backoff.retain(|_, until| *until > now);
            backoff.keys().cloned().collect()
        };
        let item = match self.store.claim_except(&skip)? {
            Some(it) => it,
            None => return Ok(false),
        };

        self.add_reaction(&item.channel, &item.id, "👀").await;
        self.report_progress(&item.channel).await;
        // A new turn gets a new timeline; the prior turn's messages stay in
        // the channel history instead of being edited underneath. A slash
        // command's answer belongs to its interaction, so only ordinary
        // turns stream their prose into the channel.
        let stream_text = crate::channels::discord::stream::enabled(&self.config)
            && item.interaction_token.is_none();
        // The card's Stop button: an opaque token bound to this channel and
        // turn, good for as long as the turn may run.
        let ttl = self
            .config
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(crate::channels::discord::runner::DEFAULT_HARD_CAP_SECS)
            .saturating_add(120);
        let mint = |kind: &str, resource: &str, ttl: u64| {
            self.store
                .component_state_create(&format!("turn_{kind}"), "*", &item.channel, resource, ttl)
                .ok()
                .map(|token| format!("turn:{kind}:{token}"))
        };
        let buttons = TurnButtons {
            stop: mint("stop", &item.id, ttl),
        };
        self.begin_turn(&item.channel, &item.conversation, stream_text, buttons)
            .await;
        // Bind this conversation's cron jobs to this channel. The chat id
        // is the live session when we have one (so a cron reply continues
        // in context), else the conversation key.
        if let Some(home) = self.conversation_home(&item.conversation) {
            let chat = self
                .session_id(&item.conversation)
                .unwrap_or_else(|| item.conversation.clone());
            crate::channels::discord::cron::write_route(
                &home,
                &item.channel,
                &chat,
                crate::channels::discord::cron::route_kind(&item.conversation),
            );
        }

        let input = match item.input_json.as_deref() {
            Some(encoded) => match serde_json::from_str(encoded) {
                Ok(input) => RunInput::Structured(input),
                Err(_) => {
                    // The turn never ran. Settle its card and fail the item
                    // here: a bare `?` would leave the card "Working" and
                    // kill the worker.
                    let settled = self
                        .finish_turn(
                            &item.channel,
                            &item.conversation,
                            None,
                            &[],
                            crate::channels::discord::stream::Status::Failed(
                                "queued input is invalid".to_string(),
                            ),
                        )
                        .await;
                    self.fail_turn(
                        &item.id,
                        "agent_failed",
                        Some("queued input is invalid"),
                        &settled,
                    )?;
                    self.remove_reaction(&item.channel, &item.id, "👀").await;
                    self.add_reaction(&item.channel, &item.id, "❌").await;
                    return Ok(true);
                }
            },
            None => RunInput::Text(item.prompt.clone()),
        };
        let mut run_fut = Box::pin((self.runner)(
            &self.config,
            &self.config_path,
            &item.conversation,
            &input,
        ));

        // The turn's messages are driven beside the run, never between its
        // reads: a slow Discord request must not stall gray's output.
        let stop_messages = tokio::sync::Notify::new();
        let messages = self.drive_turn(&item.channel, &item.conversation, &stop_messages);
        tokio::pin!(messages);
        let mut messages_done = false;

        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
        ticker.tick().await;

        let result = loop {
            tokio::select! {
                res = &mut run_fut => {
                    break Some(res);
                }
                _ = &mut messages, if !messages_done => {
                    messages_done = true;
                }
                _ = ticker.tick() => {
                    if let Ok(Some(cur)) = self.store.get(&item.id)
                        && cur.cancel {
                            break None;
                        }
                }
            }
        };
        // Let the request in flight finish (bounded by the REST timeouts),
        // then settle the turn from here.
        stop_messages.notify_one();
        if !messages_done {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(45), &mut messages).await;
        }
        let Some(result) = result else {
            // Stopped: end the gray child (and its process group) first.
            drop(run_fut);
            let settled = self
                .finish_turn(
                    &item.channel,
                    &item.conversation,
                    None,
                    &[],
                    crate::channels::discord::stream::Status::Stopped,
                )
                .await;
            self.fail_turn(&item.id, "cancelled", None, &settled)?;
            self.remove_reaction(&item.channel, &item.id, "👀").await;
            self.add_reaction(&item.channel, &item.id, "❌").await;
            return Ok(true);
        };
        if matches!(result, Err(RunError::Busy)) {
            self.timelines
                .lock()
                .await
                .remove(&activity_key(&item.channel, &item.conversation));
            if let Some(ref sink) = self.activity {
                crate::channels::discord::activity::discard(sink, &item.conversation);
            }
            self.store.requeue(&item.id)?;
            let until = self.now_secs() + BUSY_BACKOFF_SECS;
            self.busy_backoff
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(item.conversation.clone(), until);
            return Ok(true);
        }

        // Settle the turn. The answer is its last message: the streamed
        // prose becomes it in place (the bridge's final edit), or it is posted
        // below the tool lines, with the files its `MEDIA:` tags named shown
        // inside it; any it cannot hold go through the outbox.
        let (prose, media) = match &result {
            Ok(answer) if stream_text => {
                let (prose, media) = crate::channels::discord::media_tags::extract(
                    answer,
                    &self.media_cwd(),
                    &self.media_roots(),
                );
                (Some(prose), media)
            }
            _ => (None, Vec::new()),
        };
        let status = match &result {
            Ok(_) => crate::channels::discord::stream::Status::Done,
            Err(RunError::Budget(_)) => {
                crate::channels::discord::stream::Status::Failed("hit the budget".to_string())
            }
            Err(RunError::Timeout) => {
                crate::channels::discord::stream::Status::Failed("hit the time limit".to_string())
            }
            Err(RunError::Stalled(secs)) => {
                crate::channels::discord::stream::Status::Failed(format!(
                    "No activity for {} — stopped",
                    crate::channels::discord::runner::human_duration(*secs)
                ))
            }
            Err(RunError::RequestLimit(n)) => crate::channels::discord::stream::Status::Failed(
                format!("hit the {n}-request limit"),
            ),
            Err(e) => crate::channels::discord::stream::Status::Failed(
                e.reason().unwrap_or_else(|| "failed".to_string()),
            ),
        };
        let failure_reason = match (&result, &status) {
            (Err(RunError::Stalled(secs)), _) => Some(format!(
                "no output from the agent for {}",
                crate::channels::discord::runner::human_duration(*secs)
            )),
            (_, crate::channels::discord::stream::Status::Failed(reason)) => Some(reason.clone()),
            _ => None,
        };
        let loaded = crate::channels::discord::media_tags::load_pairs(&media);
        let uploads: Vec<crate::channels::discord::media_tags::Upload> =
            loaded.iter().map(|(_, upload)| upload.clone()).collect();
        let settled = self
            .finish_turn(
                &item.channel,
                &item.conversation,
                prose.as_deref(),
                &uploads,
                status,
            )
            .await;
        match result {
            Ok(answer) => {
                let receipt = serde_json::json!({});
                let leftover: Vec<String> = loaded
                    .iter()
                    .skip(settled.media_shown)
                    .map(|(path, _)| format!("MEDIA:{}", path.display()))
                    .collect();
                match settled.landed {
                    Some(landed) if !landed.parts.is_empty() || !leftover.is_empty() => {
                        let media = (!leftover.is_empty()).then(|| leftover.join("\n"));
                        self.store.complete_streamed(
                            &item.id,
                            &receipt,
                            &landed.parts,
                            media.as_deref(),
                        )?;
                        if media.is_none() {
                            self.remove_reaction(&item.channel, &item.id, "👀").await;
                            self.add_reaction(&item.channel, &item.id, "✅").await;
                        }
                    }
                    _ => self.store.complete(&item.id, &answer, &receipt)?,
                }
            }
            Err(RunError::Budget(_)) => {
                self.fail_turn(
                    &item.id,
                    "budget_blocked",
                    failure_reason.as_deref(),
                    &settled,
                )?;
                self.remove_reaction(&item.channel, &item.id, "👀").await;
                self.add_reaction(&item.channel, &item.id, "❌").await;
            }
            Err(RunError::Timeout) => {
                self.fail_turn(&item.id, "timeout", failure_reason.as_deref(), &settled)?;
                self.remove_reaction(&item.channel, &item.id, "👀").await;
                self.add_reaction(&item.channel, &item.id, "❌").await;
            }
            Err(RunError::Stalled(_)) => {
                self.fail_turn(&item.id, "stalled", failure_reason.as_deref(), &settled)?;
                self.remove_reaction(&item.channel, &item.id, "👀").await;
                self.add_reaction(&item.channel, &item.id, "❌").await;
            }
            Err(_) => {
                self.fail_turn(
                    &item.id,
                    "agent_failed",
                    failure_reason.as_deref(),
                    &settled,
                )?;
                self.remove_reaction(&item.channel, &item.id, "👀").await;
                self.add_reaction(&item.channel, &item.id, "❌").await;
            }
        }
        Ok(true)
    }

    /// Record a failed turn. When its card's footer already says how it
    /// ended, that card is the notice; otherwise the notice is posted.
    fn fail_turn(
        &self,
        id: &str,
        code: &str,
        reason: Option<&str>,
        settled: &Settled,
    ) -> Result<(), String> {
        self.store
            .fail_reason(id, code, reason, settled.status_card.as_deref())
    }

    pub async fn deliver_one(&self) -> Result<bool, String> {
        let part = match self.store.next_delivery(self.now_secs())? {
            Some(p) => p,
            None => return Ok(false),
        };
        match (self.deliver)(part.clone()).await {
            Ok(msg_id) if !msg_id.trim().is_empty() => {
                self.store.ack(&part.id, part.part, &msg_id)?;
                if let Ok(Some(row)) = self.store.get(&part.id)
                    && row.state == "sent"
                {
                    self.remove_reaction(&part.channel, &part.id, "👀").await;
                    self.add_reaction(&part.channel, &part.id, "✅").await;
                }
            }
            _ => {
                self.store
                    .delivery_failed(&part, "delivery_failed", self.now_secs())?;
            }
        }
        Ok(true)
    }
}

/// Questions a plugin asks mid-turn (gray's `host/ask`), shown in the
/// channel the turn came from. The channel is the route written just before
/// the turn starts; a conversation without one (nothing to show it in)
/// leaves questions unanswered, as in any headless run.
fn ask_handler(
    rest: &Rest,
    store: &Store,
    config_path: &Path,
    conversation: &str,
) -> Option<crate::channels::discord::runner::AskFn> {
    let home = config_path.parent()?.join("conversations").join(
        crate::channels::discord::runner::hex_sha256(conversation.as_bytes()),
    );
    let rest = rest.clone();
    let store = store.clone();
    Some(std::sync::Arc::new(move |questions: Value, turn_over| {
        let rest = rest.clone();
        let store = store.clone();
        let home = home.clone();
        Box::pin(async move {
            let channel = crate::channels::discord::cron::read_route(&home)
                .and_then(|route| route.get("route")?.as_str()?.parse::<u64>().ok());
            let questions = crate::channels::discord::ask::from_host(&questions);
            match channel {
                Some(channel) if !questions.is_empty() => {
                    crate::channels::discord::ask::ask(
                        &rest,
                        &store,
                        channel,
                        &questions,
                        std::time::Duration::from_secs(crate::channels::discord::ask::ASK_SECS),
                        std::time::Duration::from_millis(500),
                        &turn_over,
                    )
                    .await
                }
                _ => serde_json::json!({}),
            }
        })
    }))
}

impl<R, D, FutR, FutD> Runtime<R, D>
where
    R: Fn(&Value, &Path, &str, &RunInput) -> FutR + Send + Sync + Clone + 'static,
    FutR: std::future::Future<Output = Result<String, RunError>> + Send + 'static,
    D: Fn(OutboxPart) -> FutD + Send + Sync + Clone + 'static,
    FutD: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    /// Hands each queued settle to a task of its own, so one turn's card
    /// retries never hold up another turn's.
    async fn dispatch_settles(self: std::sync::Arc<Self>) -> Result<(), String> {
        loop {
            let next = self
                .settle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            match next {
                Some(job) => {
                    let runtime = self.clone();
                    tokio::spawn(async move { runtime.settle_job(job).await });
                }
                None => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            }
        }
    }

    pub async fn run(&self) -> Result<(), String> {
        self.store.recover()?;
        let concurrency = self
            .config
            .get("concurrency")
            .and_then(Value::as_u64)
            .unwrap_or(4) as usize;
        let channel_id = self
            .config
            .get("channel_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let mut set = tokio::task::JoinSet::new();
        let rt = std::sync::Arc::new(self.clone());
        for _ in 0..concurrency {
            let r = rt.clone();
            set.spawn(async move {
                loop {
                    match r.generate_one().await {
                        Ok(true) => {}
                        Ok(false) => {
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        }
                        Err(e) => return Err(e),
                    }
                }
            });
        }
        let r = rt.clone();
        set.spawn(async move {
            loop {
                match r.deliver_one().await {
                    Ok(true) => {}
                    Ok(false) => {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
        });
        let r = rt.clone();
        set.spawn(async move { r.dispatch_settles().await });
        let r = rt.clone();
        let ch = channel_id.clone();
        set.spawn(async move {
            loop {
                let _ = r
                    .store
                    .enqueue_due(&ch, crate::channels::discord::durable::now_secs());
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });

        if let Some(res) = set.join_next().await {
            set.abort_all();
            match res {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(e.to_string()),
            }
        } else {
            Ok(())
        }
    }
}

pub async fn run(config_path: &Path) -> Result<(), String> {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(parent)
        .map_err(|_| "cannot create config directory".to_string())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }

    // A pidfile next to the config: the /gateway row reads it to tell a
    // connected daemon from a stale state file. Removed on every exit path
    // (drop guard), so a crashed run never leaves a green light behind.
    let pid_path = parent.join("daemon.json");
    {
        let stamp = serde_json::json!({
            "pid": std::process::id(),
            "started_at": crate::channels::discord::durable::now_secs()
        });
        let _ = crate::channels::discord::config::atomic_json(&pid_path, &stamp);
    }
    struct RemovePid(std::path::PathBuf);
    impl Drop for RemovePid {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove_pid = RemovePid(pid_path);

    let host_lock_dir = host_lock_dir();
    let _host_lock = acquire_host_lock(&host_lock_dir, "discord", config_path)?;
    let Some(lock_file) =
        crate::channels::discord::runner::open_lock_file(&parent.join("gateway.lock"))
            .map_err(|_| "cannot open gateway lock".to_string())?
    else {
        return Err("Another gateway is running".to_string());
    };
    let _lock = lock_file;

    let mut config = crate::channels::discord::config::load_config(config_path)?;
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .ok_or_else(|| "gray_home is missing from config".to_string())?;
    let provider_bytes = std::fs::read(Path::new(gray_home).join("config.json"))
        .map_err(|_| "gray provider configuration is missing".to_string())?;
    let provider: Value = serde_json::from_slice(&provider_bytes)
        .map_err(|_| "Invalid gray config.json".to_string())?;
    let model = provider
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "provider model is missing".to_string())?;
    let model = config
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or(model);
    // Budget gates only when a policy exists (gray's setup writes none);
    // `budget set` stays the opt-in accounting path.
    config["budget_required"] =
        Value::Bool(crate::channels::discord::budget::gate(&config, model)?);

    let store = open_store(config_path)?;
    let media_root = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("media");
    let conversation_root = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("conversations");
    let file_store = FileStore::new(store.clone(), media_root, conversation_root).ok();

    let token = config
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| "token missing".to_string())?
        .to_string();
    // Ownerless is a legal bootstrap state (ownerless bootstrap): nobody is
    // admitted, and every human DM draws a pairing reply until the operator
    // approves a code. An empty owner admits nobody — never someone else.
    let owner_id = config
        .get("owner_id")
        .and_then(Value::as_str)
        .filter(|o| !o.is_empty())
        .unwrap_or_default()
        .to_string();
    let allowed: Vec<String> = config
        .get("allowed_users")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // Sliding-window burst guard per user:
    // without it one eager DMer is twenty concurrent gray processes.
    let limiter = std::sync::Arc::new(std::sync::Mutex::new(
        crate::channels::discord::ratelimit::RateLimiter::new(
            config
                .get("rate_limit_capacity")
                .and_then(Value::as_u64)
                .unwrap_or(8) as usize,
            config
                .get("rate_limit_window_secs")
                .and_then(Value::as_u64)
                .unwrap_or(60),
        ),
    ));
    let workdir = config
        .get("workdir")
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_string();

    let rest = Rest::new(crate::channels::discord::transport::API_BASE, &token);
    let mut bot_id = rest.login().await.unwrap_or_default();
    let initial_app = rest.application().await.ok().map(|(id, _)| id);

    let intents = Intents::GUILD_MESSAGES | Intents::DIRECT_MESSAGES | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, token.clone(), intents);

    let rest_del = rest.clone();
    let media_root = PathBuf::from(&workdir);
    let media_roots = crate::channels::discord::media_tags::roots_from_config(&config);
    let deliver = move |part: OutboxPart| {
        let rest = rest_del.clone();
        let media_root = media_root.clone();
        let media_roots = media_roots.clone();
        Box::pin(async move {
            // `MEDIA:<path>` tags: strip them from the prose and
            // upload the files as a V2 gallery/file message after the text.
            let (prose, media) = if part.document_json.is_none() {
                crate::channels::discord::media_tags::extract(
                    &part.content,
                    &media_root,
                    &media_roots,
                )
            } else {
                (part.content.clone(), Vec::new())
            };
            if !media.is_empty() {
                let text = crate::channels::discord::text::sanitize(&prose);
                let mut first = None;
                if !text.trim().is_empty() {
                    let mut text_part = part.clone();
                    text_part.content = prose.clone();
                    first = Some(deliver_text(&rest, text_part).await?);
                }
                let ch: u64 = part
                    .channel
                    .parse()
                    .map_err(|_| "Invalid channel ID".to_string())?;
                let uploads = crate::channels::discord::media_tags::load(&media);
                for (index, batch) in uploads
                    .chunks(crate::channels::discord::media_tags::MAX_UPLOADS_PER_MESSAGE)
                    .enumerate()
                {
                    let components = crate::channels::discord::media_tags::components(batch, None);
                    let nonce = crate::channels::discord::runner::hex_sha256(
                        format!("{}:{}:media:{index}", part.id, part.part).as_bytes(),
                    )[..24]
                        .to_string();
                    let id = rest
                        .send_v2_uploads(ch, &components, batch, Some(&nonce))
                        .await
                        .map_err(|e| e.to_string())?;
                    first.get_or_insert(id);
                }
                return first.ok_or_else(|| "Delivery returned no message ID".to_string());
            }
            deliver_text(&rest, part).await
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
    };

    async fn deliver_text(rest: &Rest, part: OutboxPart) -> Result<String, String> {
        let text = crate::channels::discord::text::sanitize(&part.content);
        let v2 = part.render.as_deref() == Some("v2");
        if let Some(document_json) = part.document_json.as_deref() {
            let Ok(document) = serde_json::from_str::<Value>(document_json) else {
                return Err("stored component document is invalid".to_string());
            };
            if document
                .get("components")
                .and_then(Value::as_array)
                .is_none()
            {
                return Err("stored component document has no components".to_string());
            }
            if part.part == 0
                && let Some(ref token) = part.interaction_token
            {
                let app_id = part.app_id.as_deref().unwrap_or("");
                return rest
                    .edit_original_stored_document(app_id, token, &document)
                    .await
                    .map_err(|error| error.to_string());
            }
            let ch: u64 = part
                .channel
                .parse()
                .map_err(|_| "Invalid channel ID".to_string())?;
            return rest
                .send_stored_document(ch, &document, None)
                .await
                .map_err(|error| error.to_string());
        }
        if let Some(ref token) = part.interaction_token {
            let app_id = part.app_id.as_deref().unwrap_or("");
            if v2 {
                if part.part == 0 {
                    let components = crate::channels::discord::render::text_message(&text)
                        .map_err(|e| e.to_string())?;
                    rest.edit_original_v2(app_id, token, &components, true)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    let ids = rest
                        .followup_v2(app_id, token, &text)
                        .await
                        .map_err(|e| e.to_string())?;
                    ids.into_iter()
                        .next()
                        .ok_or_else(|| "Delivery returned no message ID".to_string())
                }
            } else {
                let ids = rest
                    .followup(app_id, token, &text)
                    .await
                    .map_err(|e| e.to_string())?;
                ids.into_iter()
                    .next()
                    .ok_or_else(|| "Delivery returned no message ID".to_string())
            }
        } else {
            let ch: u64 = part
                .channel
                .parse()
                .map_err(|_| "Invalid channel ID".to_string())?;
            let nonce = crate::channels::discord::runner::hex_sha256(
                format!("{}:{}", part.id, part.part).as_bytes(),
            )[..24]
                .to_string();
            if v2 {
                rest.send_text_v2(ch, &text, Some(&nonce))
                    .await
                    .map_err(|e| e.to_string())
            } else {
                rest.send(ch, &text, Some(&nonce))
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    }

    // One narration sink per daemon: the runner pushes gray's progress
    // rows, the Runtime drains them into the channel's status bubble.
    let activity = crate::channels::discord::activity::sink();
    let runner_sink = activity.clone();
    let ask_rest = rest.clone();
    let ask_store = store.clone();
    let model_store = store.clone();
    let runner = move |cfg: &Value, pth: &Path, conv: &str, input: &RunInput| {
        let cfg = cfg.clone();
        let pth = pth.to_path_buf();
        let conv = conv.to_string();
        let input = input.clone();
        let sink = runner_sink.clone();
        let ask = ask_handler(&ask_rest, &ask_store, &pth, &conv);
        let store = model_store.clone();
        Box::pin(async move {
            let model = store
                .meta_get(&crate::channels::discord::commands::model_key(&conv))
                .ok()
                .flatten()
                .filter(|m| !m.trim().is_empty());
            let opts = crate::channels::discord::runner::RunOpts {
                model,
                progress: Some(crate::channels::discord::activity::callback_for(
                    sink,
                    conv.clone(),
                )),
                ask,
                ..crate::channels::discord::runner::default_opts()
            };
            crate::channels::discord::runner::run_gray_input(&cfg, &pth, &conv, input, opts).await
        })
    };

    let runtime = Runtime::new(
        config.clone(),
        config_path.to_path_buf(),
        store.clone(),
        deliver,
        runner,
    )
    .with_rest(rest.clone())
    .with_activity(activity);

    let runtime_task = runtime.run();
    tokio::pin!(runtime_task);

    let exe_watch = crate::channels::discord::exe_watch::ExeWatch::capture();
    let mut exe_tick = tokio::time::interval(std::time::Duration::from_secs(30));
    exe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Chat-bound cron. Its own task: firing a job spawns a gray turn, and
    // that must never stall shard events (a typing indicator that stops
    // updating mid-turn is a broken gateway).
    let cron_rest = rest.clone();
    let cron_store = store.clone();
    let cron_bin = std::path::PathBuf::from(
        config
            .get("gray_bin")
            .and_then(Value::as_str)
            .unwrap_or("gray"),
    );
    let cron_conversations = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("conversations");
    let cron_task = async move {
        let mut every = tokio::time::interval(crate::channels::discord::cron::TICK_EVERY);
        // A slow tick must not burst-fire the backlog.
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            every.tick().await;
            for home in crate::channels::discord::cron::routable_homes(&cron_conversations) {
                for delivery in crate::channels::discord::cron::tick_home(&cron_bin, &home).await {
                    let Ok(ch) = delivery.channel.parse::<u64>() else {
                        continue;
                    };
                    let _ = crate::channels::discord::cron::post_delivery(
                        &cron_rest,
                        &cron_store,
                        ch,
                        &delivery,
                    )
                    .await;
                }
            }
        }
    };
    tokio::pin!(cron_task);
    // Always-on: what gray's gateway produced on its own reaches Discord.
    let pump_task = crate::channels::discord::always_on::pump(
        std::path::PathBuf::from(
            config
                .get("gray_home")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.gray")))
                .unwrap_or_default(),
        ),
        config_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("conversations"),
        rest.clone(),
    );
    tokio::pin!(pump_task);

    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    let mut app_id = initial_app;
    let mut queued_acks: std::collections::HashMap<String, f64> = std::collections::HashMap::new();

    let sigterm_recv = async {
        #[cfg(unix)]
        if let Some(ref mut s) = sigterm {
            s.recv().await;
            return;
        }
        #[allow(unreachable_code)]
        std::future::pending::<()>().await
    };
    tokio::pin!(sigterm_recv);

    loop {
        tokio::select! {
            res = &mut runtime_task => {
                return res;
            }
            _ = &mut pump_task => {
                return Err("always-on pump stopped".to_string());
            }
            _ = &mut cron_task => {
                // The cron task never returns; if it does, the daemon is
                // broken rather than idle.
                return Err("cron task stopped".to_string());
            }
            _ = &mut sigterm_recv => {
                break;
            }
            _ = exe_tick.tick() => {
                let idle = store
                    .running_channels()
                    .map(|c| c.is_empty())
                    .unwrap_or(false);
                if idle && exe_watch.as_ref().is_some_and(|w| w.replaced()) {
                    crate::channels::discord::eprintln_stamped!("[discord] binary replaced; exiting for restart");
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
            event = shard.next_event(EventTypeFlags::MESSAGE_CREATE | EventTypeFlags::INTERACTION_CREATE | EventTypeFlags::READY) => {
                match event {
                    Some(Ok(Event::Ready(ready))) => {
                        if owner_id.is_empty() {
                            crate::channels::discord::println_stamped!("Discord connected; nobody admitted yet (pairing replies only).");
                        } else {
                            crate::channels::discord::println_stamped!("Discord connected; durable owner-only queue enabled.");
                        }
                        {
                            // Pin the identity for gray's /gateway row: files
                            // only, no socket, no token. Written after the
                            // gateway is up so a half-connected daemon leaves
                            // the previous value in place.
                            let state_path = config_path
                                .parent()
                                .unwrap_or_else(|| Path::new("."))
                                .join("state.json");
                            let bot = format!(
                                "{}#{}",
                                ready.user.name, ready.user.discriminator
                            );
                            let state = serde_json::json!({
                                "bot": bot,
                                "bot_id": bot_id,
                                "connected_at": crate::channels::discord::durable::now_secs()
                            });
                            let _ = crate::channels::discord::config::atomic_json(&state_path, &state);
                        }
                        let r_app_id = ready.application.id.to_string();
                        bot_id = ready.user.id.to_string();
                        app_id = Some(r_app_id.clone());

                        // Messages sent while the daemon was down never
                        // reach the shard — a sleeping hosted seat is the
                        // common case — so replay them off the REST cursor
                        // before live traffic resumes. Inbox dedupe covers
                        // anything Discord also delivered live.
                        let backfilled = crate::channels::discord::backfill::run(
                            &rest,
                            &store,
                            &config,
                            &token,
                            std::path::Path::new(&workdir),
                            &owner_id,
                            &allowed,
                            &bot_id,
                            config
                                .get("queue_capacity")
                                .and_then(Value::as_u64)
                                .unwrap_or(1000),
                        )
                        .await;
                        if backfilled > 0 {
                            crate::channels::discord::eprintln_stamped!(
                                "[discord] backfilled {backfilled} missed message(s)"
                            );
                        }

                        // `gray cmd` discovers what the binary answers
                        // headlessly; an old gray's failed fetch caches as
                        // a negative and the table stays static.
                        let registry = crate::channels::discord::graycmd::fetch_registry(&config).await;
                        crate::channels::discord::graycmd::cache_registry(&store, registry.as_deref());
                        let cmds = slash_commands_json(registry.as_deref());
                        let cmds_str = serde_json::to_string(&cmds).unwrap_or_default();
                        let hash = crate::channels::discord::runner::hex_sha256(cmds_str.as_bytes());
                        let cached = store.meta_get("slash_commands_hash").ok().flatten();
                        if cached.as_deref() == Some(&hash) {
                            // Unchanged, skip registration
                        } else if let Err(e) = rest.register_commands(&r_app_id, &cmds).await {
                            crate::channels::discord::eprintln_stamped!("[discord] slash command registration failed: {e}");
                        } else {
                            let _ = store.meta_set("slash_commands_hash", &hash);
                        }
                    }
                    Some(Ok(Event::MessageCreate(msg))) => {
                        let m = &msg.0;
                        let author_id = m.author.id.to_string();
                        let channel_id = m.channel_id.to_string();
                        let is_dm = m.guild_id.is_none();
                        // Reconnect-backfill cursor: stamped on every seen
                        // message, admitted or not, so replay only ever
                        // picks up what arrived while we were down.
                        crate::channels::discord::backfill::note_message(&store, &channel_id, &m.id.to_string());
                        // Receipt log: every message the gateway sees, by ID
                        // and surface. Never the content — this is the only
                        // way to tell "never DM'd" from "event never arrived".
                        crate::channels::discord::eprintln_stamped!("[discord] message from {} (dm: {is_dm})", m.author.id.get());
                        let prompt = crate::channels::discord::policy::incoming(
                            &author_id,
                            &owner_id,
                            m.author.bot,
                            is_dm,
                            &m.content,
                            &bot_id,
                            &allowed,
                            !m.attachments.is_empty(),
                        );
                        if let Some(prompt) = prompt {
                            // A question card is waiting in this channel:
                            // the message is its answer, not a new turn.
                            if crate::channels::discord::ask::answer_typed(&store, &rest, &channel_id, &prompt).await {
                                continue;
                            }
                            let capacity = config
                                .get("queue_capacity")
                                .and_then(Value::as_u64)
                                .unwrap_or(1000);
                            let msg_id = m.id.to_string();
                            let conv = crate::channels::discord::session::conversation_key(
                                &channel_id,
                                &author_id,
                                is_dm,
                            );
                            // Burst guard: an over-eager user is told to slow
                            // down instead of forking gray per message.
                            let allowed_now = limiter
                                .lock()
                                .map(|mut l| l.allow(&author_id))
                                .unwrap_or(true);
                            if !allowed_now {
                                let wait = limiter
                                    .lock()
                                    .map(|mut l| l.retry_after_secs(&author_id))
                                    .unwrap_or(1);
                                if let Ok(ch) = channel_id.parse::<u64>() {
                                    let _ = rest
                                        .send_text_v2(
                                            ch,
                                            &format!("Too fast — try again in {wait}s."),
                                            None,
                                        )
                                        .await;
                                }
                                continue;
                            }
                            // Attachments the user sent: saved, then named in
                            // the prompt so gray's own tools can read them.
                            // Voice notes are transcribed first, so the model
                            // reads the words rather than reaching for a file.
                            let atts: Vec<crate::channels::discord::attachments::AttachmentRef<'_>> = m
                                .attachments
                                .iter()
                                .map(|a| crate::channels::discord::attachments::AttachmentRef {
                                    url: &a.url,
                                    filename: &a.filename,
                                    size: a.size,
                                    content_type: a.content_type.as_deref(),
                                })
                                .collect();
                            let mut prompt = crate::channels::discord::attachments::enrich_prompt(
                                &config,
                                &token,
                                std::path::Path::new(&workdir),
                                &msg_id,
                                prompt,
                                &atts,
                            )
                            .await;
                            let steer_text = steer_command(&prompt);
                            let explicit = steer_text.is_some();
                            if let Some(text) = steer_text {
                                prompt = text;
                            }
                            let notice = if explicit && prompt.trim().is_empty() {
                                Some("Usage: /steer <text>".to_string())
                            } else if (explicit || busy_input_steer(&config))
                                && crate::channels::discord::runner::steer(&conv, &prompt).await
                            {
                                Some("Steering the current turn.".to_string())
                            } else {
                                match store.enqueue(&msg_id, &channel_id, &prompt, Some(&conv), capacity) {
                                    Err(_) => Some("Queue full or message invalid; this message was not accepted.".to_string()),
                                    Ok(true) if explicit => Some("Nothing running — sent as a new message.".to_string()),
                                    Ok(true) if !crate::channels::discord::stream::quiet(&config) => {
                                        queued_notice(&store, &mut queued_acks, &conv, &msg_id)
                                    }
                                    Ok(true) => None,
                                    Ok(false) => None,
                                }
                            };
                            if let (Some(notice), Ok(ch)) = (notice, channel_id.parse::<u64>()) {
                                let _ = rest.send_text_v2(ch, &notice, None).await;
                            }
                        } else if let Some(paired) =
                            crate::channels::discord::pairing::reply_for(&store, &author_id, is_dm, m.author.bot)
                        {
                            // Unknown human DMing the bot: tell them their own
                            // ID and mint a code — the owner approves it with
                            // `gray discord pairing approve discord <code> --as <user_id>`.
                            if let Ok(ch) = channel_id.parse::<u64>() {
                                let embed =
                                    crate::channels::discord::pairing::unconfigured_embed(&author_id, &paired.code);
                                match crate::channels::discord::render::from_embed(&embed, &[]) {
                                    Ok(components) => match rest.send_v2(ch, &components, None).await {
                                        Ok(_) => crate::channels::discord::eprintln_stamped!("[discord] pairing reply sent"),
                                        Err(e) => crate::channels::discord::eprintln_stamped!("[discord] pairing reply failed: {e}"),
                                    },
                                    Err(e) => crate::channels::discord::eprintln_stamped!("[discord] pairing render failed: {e}"),
                                }
                            }
                        }
                    }
                    Some(Ok(Event::InteractionCreate(ic))) => {
                        let interaction = &ic.0;
                        let user_id = interaction
                            .author_id()
                            .map(|id| id.to_string())
                            .unwrap_or_default();
                        if !crate::channels::discord::policy::is_allowed_user(&user_id, &owner_id, &allowed) {
                            continue;
                        }
                        #[allow(deprecated)]
                        let channel_id = interaction
                            .channel
                            .as_ref()
                            .map(|c| c.id.to_string())
                            .or_else(|| interaction.channel_id.map(|c| c.to_string()))
                            .unwrap_or_default();

                        let int_id = interaction.id.to_string();
                        // One context object for every handler; twilight
                        // never travels past this line.
                        let effective_app = app_id
                            .clone()
                            .unwrap_or_else(|| interaction.application_id.to_string());
                        let ctx = crate::channels::discord::command_dispatch::Ctx {
                            user_id: &user_id,
                            channel_id: &channel_id,
                            is_dm: interaction.guild_id.is_none(),
                            int_id: &int_id,
                            int_token: &interaction.token,
                            app_id: &effective_app,
                            rest: &rest,
                            store: &store,
                            config: &config,
                            config_path,
                        };
                        let is_component = matches!(
                            &interaction.data,
                            Some(
                                twilight_model::application::interaction::InteractionData::MessageComponent(_)
                                    | twilight_model::application::interaction::InteractionData::ModalSubmit(_)
                            )
                        );
                        let is_autocomplete = interaction.kind == InteractionType::ApplicationCommandAutocomplete;
                        // The turn card's Stop button is plugin-owned, not a
                        // typed agent component: handle it before that router.
                        if let Some(twilight_model::application::interaction::InteractionData::MessageComponent(component)) = &interaction.data {
                            let id = component.custom_id.as_str();
                            if id.starts_with("turn:stop:") {
                                let pressed = interaction
                                    .message
                                    .as_ref()
                                    .and_then(|message| serde_json::to_value(&message.components).ok());
                                crate::channels::discord::command_dispatch::stop_button(&ctx, id, pressed.as_ref()).await;
                                continue;
                            }
                            if id.starts_with("ask:") {
                                crate::channels::discord::command_dispatch::ask_press(&ctx, id, &component.values, None).await;
                                continue;
                            }
                            if id.starts_with("pg:") {
                                let pressed = interaction
                                    .message
                                    .as_ref()
                                    .and_then(|message| serde_json::to_value(&message.components).ok());
                                crate::channels::discord::pager::press(&ctx, id, pressed.as_ref()).await;
                                continue;
                            }
                            if id.starts_with("gc:") {
                                let is_button = matches!(
                                    component.component_type,
                                    twilight_model::channel::message::component::ComponentType::Button
                                );
                                crate::channels::discord::graycmd::component(&ctx, id, &component.values, is_button).await;
                                continue;
                            }
                        }
                        if let Some(twilight_model::application::interaction::InteractionData::ModalSubmit(modal)) = &interaction.data
                            && modal.custom_id.starts_with("ask:") {
                                let raw = serde_json::to_value(&modal.components).unwrap_or(Value::Null);
                                let typed = find_value(&raw, "note");
                                crate::channels::discord::command_dispatch::ask_press(&ctx, &modal.custom_id, &[], typed.as_deref()).await;
                                continue;
                            }
                        let autocomplete_cmd = match &interaction.data {
                            Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(cmd)) => {
                                (is_autocomplete && matches!(cmd.name.as_str(), "run" | "skills"))
                                    .then_some(cmd.name.as_str())
                            }
                            _ => None,
                        };
                        if let Some(ac_command) = autocomplete_cmd {
                            let cmd = match &interaction.data {
                                Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(cmd)) => cmd,
                                _ => continue,
                            };
                            let query = cmd
                                .options
                                .iter()
                                .find_map(|o| match &o.value {
                                    twilight_model::application::interaction::application_command::CommandOptionValue::Focused(v, _) if o.name == "name" => Some(v.clone()),
                                    _ => None,
                                })
                                .unwrap_or_default();
                            let reg = crate::channels::discord::graycmd::registry(&ctx).await;
                            let choices = if ac_command == "skills" {
                                crate::channels::discord::graycmd::skills_autocomplete(reg.as_deref(), &query)
                            } else {
                                crate::channels::discord::graycmd::run_autocomplete(reg.as_deref(), &query)
                            };
                            let _ = rest
                                .autocomplete_response(&int_id, &interaction.token, &choices)
                                .await;
                            continue;
                        }
                        if is_component || is_autocomplete {
                            let raw = serde_json::to_value(interaction)
                                .map_err(|_| "interaction could not be normalized".to_string())?;
                            let capacity = config
                                .get("queue_capacity")
                                .and_then(Value::as_u64)
                                .unwrap_or(1000);
                            let result = route_normalized_interaction(NormalizedRoute {
                                raw: &raw,
                                interaction_token: &interaction.token,
                                app_id: &effective_app,
                                rest: &rest,
                                store: &store,
                                file_store: file_store.as_ref(),
                                capacity,
                                is_dm: interaction.guild_id.is_none(),
                            })
                            .await;
                            if let Err(error) = result {
                                let legacy_id = raw
                                    .pointer("/data/custom_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let legacy_handled = if legacy_id.starts_with("cron:remove:") {
                                    match &interaction.data {
                                        Some(twilight_model::application::interaction::InteractionData::MessageComponent(_component)) => {
                                            crate::channels::discord::command_dispatch::button(&ctx, legacy_id).await;
                                            true
                                        }
                                        Some(twilight_model::application::interaction::InteractionData::ModalSubmit(_component)) => {
                                            crate::channels::discord::command_dispatch::modal(&ctx, legacy_id).await;
                                            true
                                        }
                                        _ => false,
                                    }
                                } else {
                                    false
                                };
                                if !legacy_handled {
                                    if is_autocomplete {
                                        let _ = rest
                                            .autocomplete_response(&int_id, &interaction.token, &[])
                                            .await;
                                    } else {
                                        let error_components = crate::channels::discord::render::error_card(
                                            "This component is no longer available.",
                                        )
                                        .unwrap_or_else(|_| {
                                            vec![serde_json::json!({
                                                "type": 10,
                                                "content": "This component is no longer available."
                                            })]
                                        });
                                        let _ = rest
                                            .interaction_v2(
                                                &int_id,
                                                &interaction.token,
                                                &error_components,
                                                false,
                                            )
                                            .await;
                                    }
                                    crate::channels::discord::eprintln_stamped!("[discord] component interaction rejected: {error}");
                                }
                            }
                            continue;
                        }

                        if let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(ref cmd)) = interaction.data {
                        // Flatten what Discord nests: a sub-command arrives as
                        // one option that carries its own options. Handlers
                        // read flat name/value pairs, so the parsing of
                        // twilight's shape lives here and nowhere else.
                        let (sub, args): (Option<String>, Vec<(&str, &str)>) = cmd
                            .options
                            .first()
                            .map(|opt| match &opt.value {
                                twilight_model::application::interaction::application_command::CommandOptionValue::SubCommand(inner) => (
                                    Some(opt.name.clone()),
                                    inner
                                        .iter()
                                        .filter_map(|o| match &o.value {
                                            twilight_model::application::interaction::application_command::CommandOptionValue::String(v) => Some((o.name.as_str(), v.as_str())),
                                            _ => None,
                                        })
                                        .collect(),
                                ),
                                _ => (None, Vec::new()),
                            })
                            .unwrap_or((None, Vec::new()));
                        // Top-level string options (the plain `/ask`).
                        let mut args = args;
                        for o in &cmd.options {
                            if let twilight_model::application::interaction::application_command::CommandOptionValue::String(v) = &o.value {
                                args.push((o.name.as_str(), v.as_str()));
                            }
                        }
                        // Registry commands (`model`, `help`, `usage`, …)
                        // route to `gray cmd`; bridge-owned commands answer
                        // locally and never wait on a `cmd list` spawn.
                        let request = if crate::channels::discord::graycmd::BRIDGE_OWNED.contains(&cmd.name.as_str()) {
                            crate::channels::discord::commands::parse(cmd.name.as_str(), sub.as_deref(), &args)
                        } else {
                            let cmd_registry = crate::channels::discord::graycmd::registry(&ctx).await;
                            crate::channels::discord::graycmd::route(
                                cmd_registry.as_deref(),
                                cmd.name.as_str(),
                                sub.as_deref(),
                                &args,
                            )
                            .or_else(|| {
                                crate::channels::discord::commands::parse(cmd.name.as_str(), sub.as_deref(), &args)
                            })
                        };
                        match request {
                            Some(request) => crate::channels::discord::command_dispatch::handle(&ctx, request).await,
                            None => {
                                // An interaction we did not register (or a
                                // sub-command that vanished under us).
                                crate::channels::discord::eprintln_stamped!(
                                    "[discord] unparseable slash command: {} {sub:?}",
                                    cmd.name.as_str()
                                );
                                crate::channels::discord::command_dispatch::unknown(&ctx).await;
                            }
                        }
                        }
                        // Message-component and modal events are handled above
                        // through the typed state/event path. The old raw
                        // custom-id handlers remain available for legacy rows
                        // but are never selected for new interactions.
                    }
                    Some(Err(e)) => {
                        crate::channels::discord::eprintln_stamped!("[discord] gateway error: {e}");
                    }
                    None => {
                        // The stream only ends on a fatal close (bad token,
                        // disabled privileged intent, or dropped network).
                        // Say so: an exit-0 silence would restart-loop under
                        // a supervisor with empty logs.
                        crate::channels::discord::eprintln_stamped!(
                            "[discord] gateway closed the connection (token, Message Content Intent, or network); exiting"
                        );
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn host_lock_dir() -> PathBuf {
    for key in ["GRAY_HOST_LOCK_DIR", "XDG_STATE_HOME"] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim();
            if !t.is_empty() {
                return if key == "GRAY_HOST_LOCK_DIR" {
                    PathBuf::from(t)
                } else {
                    PathBuf::from(t).join("gray").join("locks")
                };
            }
        }
    }
    #[cfg(windows)]
    if let Ok(v) = std::env::var("LOCALAPPDATA") {
        let t = v.trim();
        if !t.is_empty() {
            return PathBuf::from(t).join("gray").join("locks");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("gray")
            .join("locks");
    }
    std::env::temp_dir().join("gray-locks")
}

fn acquire_host_lock(dir: &Path, role: &str, config_path: &Path) -> Result<std::fs::File, String> {
    let mut db = std::fs::DirBuilder::new();
    db.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        db.mode(0o700);
    }
    db.create(dir)
        .map_err(|e| format!("cannot create host lock dir: {e}"))?;
    let lock_path = dir.join(format!("{role}.lock"));
    let Some(f) = crate::channels::discord::runner::open_lock_file(&lock_path)
        .map_err(|e| format!("cannot open host lock: {e}"))?
    else {
        let rec_path = dir.join(format!("{role}.json"));
        if let Ok(body) = std::fs::read_to_string(&rec_path)
            && let Ok(v) = serde_json::from_str::<Value>(&body)
            && let Some(pid) = v.get("pid").and_then(Value::as_u64)
        {
            return Err(format!(
                "Another {role} gateway is already running (pid {pid})"
            ));
        }
        return Err(format!("Another {role} gateway is already running"));
    };
    let rec = serde_json::json!({
        "pid": std::process::id(),
        "config": config_path.display().to_string(),
    });
    let _ = std::fs::write(dir.join(format!("{role}.json")), rec.to_string());
    Ok(f)
}

#[cfg(test)]
mod component_tests {
    use super::autocomplete_choices;
    use serde_json::json;

    #[test]
    fn autocomplete_choices_are_filtered_and_bounded() {
        let state = json!({"options": [
            {"label": "Gray Cloud", "value": "gray"},
            {"label": "Local", "value": "local"},
            {"label": "Other", "value": "other"}
        ]});
        let choices = autocomplete_choices(&json!({"query": "gr"}), &state);
        assert_eq!(
            choices,
            vec![("Gray Cloud".to_string(), "gray".to_string())]
        );
    }

    #[test]
    fn a_label_wrapped_text_box_survives_twilight() {
        // Discord's modal submit for the question card's text box.
        let submit = serde_json::json!({
            "custom_id": "ask:abc123:0:note",
            "components": [{
                "type": 18, "id": 1,
                "component": {"type": 4, "id": 2, "custom_id": "note", "value": "the hotfix branch"}
            }]
        });
        let data: twilight_model::application::interaction::modal::ModalInteractionData =
            serde_json::from_value(submit).unwrap();
        let raw = serde_json::to_value(&data.components).unwrap();
        assert_eq!(
            super::find_value(&raw, "note").as_deref(),
            Some("the hotfix branch")
        );
    }
}
