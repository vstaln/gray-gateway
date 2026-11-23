//! Slash-command handlers.
//!
//! One entry point ([`handle`]), one place that knows how to answer a
//! Discord interaction. Everything here is the adapter around the pure
//! logic in [`crate::channels::discord::commands`]: that module decides what a request *means*,
//! this one executes it and renders the reply.
//!
//! Two reply rules that are easy to get wrong:
//!
//! - Discord accepts exactly **one** initial response per interaction. A
//!   callback that took longer than 3 seconds is dropped, so anything that
//!   shells out acknowledges first and answers through the webhook.
//! - Ephemerality is decided by the *deferral*, not the followup.

use crate::channels::discord::commands::{self, JobRow, Request};
use crate::channels::discord::durable::Store;
use crate::channels::discord::render;
use crate::channels::discord::transport::Rest;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a delegated gray CLI call may take. gray's own memory CLI is
/// file-only and answers in milliseconds; 10s is a guard against a wedged
/// binary, not a budget.
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything a handler needs, borrowed so nothing is cloned per reply.
pub struct Ctx<'a> {
    pub user_id: &'a str,
    pub channel_id: &'a str,
    pub is_dm: bool,
    pub int_id: &'a str,
    pub int_token: &'a str,
    pub app_id: &'a str,
    pub rest: &'a Rest,
    pub store: &'a Store,
    pub config: &'a Value,
    pub config_path: &'a Path,
}

impl<'a> Ctx<'a> {
    /// The gray home this channel's turns run in. Same derivation as the
    /// runner's, so a `/memory` call sees exactly what the agent in this
    /// channel sees.
    pub(crate) fn conversation_home(&self) -> PathBuf {
        let base = self
            .config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let home = base
            .join("conversations")
            .join(crate::channels::discord::runner::hex_sha256(
                self.conversation().as_bytes(),
            ));
        ensure_private_dir(&home);
        home
    }

    /// DMs use their channel; guild messages are isolated per user. Discord
    /// supplies a thread's own channel id for messages inside that thread,
    /// so native threads already get separate sessions.
    pub fn conversation(&self) -> String {
        crate::channels::discord::session::conversation_key(
            self.channel_id,
            self.user_id,
            self.is_dm,
        )
    }

    /// The configured home channel, which owns every schedule written before
    /// `/cron` existed.
    fn home_channel(&self) -> &str {
        self.config
            .get("channel_id")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

/// Create the conversation home at 0700 where the platform has a mode
/// knob. `DirBuilder::mode` does not exist on Windows, and this crate is
/// built there too.
fn ensure_private_dir(path: &Path) {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let _ = builder.create(path);
}

/// Which gray this channel answers with, per `meta` then gray's own config.
fn current_model(ctx: &Ctx<'_>) -> (Option<String>, Option<String>) {
    let override_model = ctx
        .store
        .meta_get(&commands::model_key(&ctx.conversation()))
        .ok()
        .flatten()
        .filter(|m| !m.trim().is_empty());
    let provider_model = ctx
        .config
        .get("gray_home")
        .and_then(Value::as_str)
        .and_then(|home| std::fs::read(Path::new(home).join("config.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_string))
        .filter(|m| !m.trim().is_empty());
    (override_model, provider_model)
}

/// Run `gray <argv>` in this channel's home and take its text. gray owns the
/// format; the bridge renders what it printed, so there is no second parser
/// to keep in step with it.
pub(crate) async fn gray_cli(ctx: &Ctx<'_>, argv: &[String]) -> Result<(String, String), String> {
    let home = ctx.conversation_home();
    gray_cli_in(ctx, &home, argv, CLI_TIMEOUT).await
}

/// The same call against an explicit home — `gray cmd` routes by scope.
pub(crate) async fn gray_cli_in(
    ctx: &Ctx<'_>,
    home: &Path,
    argv: &[String],
    timeout: Duration,
) -> Result<(String, String), String> {
    let gray_bin = ctx
        .config
        .get("gray_bin")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if gray_bin.is_empty() {
        return Err("gray executable is missing".to_string());
    }
    crate::channels::discord::graycmd::spawn(&gray_bin, home, argv, timeout).await
}

fn rendered(ctx: &Ctx<'_>, embed: &Value, buttons: &[Value]) -> Vec<Value> {
    let (title, body, accent) = render::merged(embed);
    if body.chars().count() > crate::channels::discord::render::MAX_DISPLAY_CHARS {
        let pages = crate::channels::discord::pager::paginate(
            &body,
            crate::channels::discord::pager::PAGE_LIMIT,
        );
        if pages.len() > 1
            && let Ok(token) = crate::channels::discord::pager::mint(
                ctx.store,
                ctx.user_id,
                ctx.channel_id,
                &pages,
                &title,
                accent,
            )
        {
            let mut comps =
                crate::channels::discord::pager::components(&pages, 0, &token, &title, accent);
            comps.extend(buttons.iter().cloned());
            return comps;
        }
    }
    match render::from_embed(embed, buttons) {
        Ok(components) => components,
        Err(e) => {
            crate::channels::discord::eprintln_stamped!("[discord] invalid command render: {e}");
            vec![json!({"type": 10, "content": "Gray could not render this response."})]
        }
    }
}

/// Answer in the same call as the interaction (the 3-second path).
pub(crate) async fn answer(ctx: &Ctx<'_>, embed: &Value, buttons: &[Value], public: bool) {
    let components = rendered(ctx, embed, buttons);
    if let Err(e) = ctx
        .rest
        .interaction_v2(ctx.int_id, ctx.int_token, &components, public)
        .await
    {
        crate::channels::discord::eprintln_stamped!("[discord] command reply failed: {e}");
    }
}

pub(crate) async fn edit_deferred(ctx: &Ctx<'_>, embed: &Value, buttons: &[Value], public: bool) {
    let components = rendered(ctx, embed, buttons);
    if let Err(e) = ctx
        .rest
        .edit_original_v2(ctx.app_id, ctx.int_token, &components, public)
        .await
    {
        crate::channels::discord::eprintln_stamped!("[discord] command original edit failed: {e}");
    }
}

/// Acknowledge first, then edit the original response. `public` is fixed by
/// the deferral because Discord does not let a later edit change visibility.
async fn answer_deferred(
    ctx: &Ctx<'_>,
    public: bool,
    render: impl std::future::Future<Output = Value>,
) {
    if let Err(e) = ctx.rest.defer_v2(ctx.int_id, ctx.int_token, public).await {
        crate::channels::discord::eprintln_stamped!("[discord] command defer failed: {e}");
        return;
    }
    let embed = render.await;
    edit_deferred(ctx, &embed, &[], public).await;
}

async fn gateway_command(ctx: &Ctx<'_>, args: Option<&str>) {
    let args: Vec<&str> = args.unwrap_or("status").split_whitespace().collect();
    let verb = args.first().copied().unwrap_or("status");
    let args = if args.is_empty() {
        vec!["status"]
    } else {
        args
    };
    if !matches!(verb, "status" | "on" | "off" | "reset-policy")
        || (verb != "reset-policy" && args.len() > 1)
        || args
            .iter()
            .any(|arg| *arg == "--check" || arg.starts_with("--last-activity"))
    {
        let usage =
            "/gateway args: reset-policy [none|idle|daily|both] [--idle-minutes N] [--at-hour H]";
        answer(ctx, &commands::embed("Gateway", usage, &[]), &[], false).await;
        return;
    }
    if (verb == "reset-policy" && args.len() > 1) || matches!(verb, "on" | "off") {
        let owner = crate::channels::discord::config::load_config(ctx.config_path)
            .ok()
            .and_then(|config| {
                config
                    .get("owner_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if owner.as_deref() != Some(ctx.user_id) {
            answer(
                ctx,
                &commands::embed(
                    "Gateway",
                    "Only the bot owner can change gateway settings.",
                    &[],
                ),
                &[],
                false,
            )
            .await;
            return;
        }
    }
    answer_deferred(ctx, false, async {
        let Some(home) = ctx.config.get("gray_home").and_then(Value::as_str) else {
            return commands::embed("Gateway", "Gateway home is unavailable.", &[]);
        };
        let argv = std::iter::once("gateway")
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let text = match gray_cli_in(ctx, Path::new(home), &argv, CLI_TIMEOUT).await {
            Ok((out, _)) if !out.trim().is_empty() => out.trim().to_string(),
            Ok((_, err)) if !err.trim().is_empty() => err.trim().chars().take(1000).collect(),
            _ => "Gray Gateway did not report a result.".to_string(),
        };
        commands::embed("Gateway", text, &[])
    })
    .await;
}

/// Execute one parsed request. Every arm ends in a reply, so a request that
/// falls through the match would be a silent no-op the user cannot see —
/// which is why `handle` has no other exit.
pub async fn handle(ctx: &Ctx<'_>, request: Request) {
    match request {
        Request::Help { command } => {
            let embed = match command.as_deref() {
                Some(name) => commands::help_for(name).unwrap_or_else(|| {
                    commands::embed(
                        "No such command",
                        format!("`/{name}` is not a command here. `/help` lists them all."),
                        &[],
                    )
                }),
                None => commands::help_embed(),
            };
            answer(ctx, &embed, &[], true).await;
        }
        Request::CronList => {
            let jobs = match ctx.store.schedules() {
                Ok(j) => j,
                Err(e) => return answer(ctx, &commands::embed("Cron", e, &[]), &[], false).await,
            };
            let targets: Vec<String> = jobs.iter().map(|j| j.channel.clone()).collect();
            let rows: Vec<JobRow> = jobs
                .iter()
                .map(|j| (j.id.clone(), j.prompt.clone(), j.interval, j.status.clone()))
                .collect();
            let mine: Vec<JobRow> =
                commands::jobs_for_channel(&rows, &targets, ctx.channel_id, ctx.home_channel())
                    .into_iter()
                    .cloned()
                    .collect();
            let mut state_tokens = Vec::new();
            for job in mine
                .iter()
                .take(crate::channels::discord::commands::MAX_BUTTONS)
            {
                match ctx.store.component_state_create(
                    "cron_remove",
                    ctx.user_id,
                    ctx.channel_id,
                    &job.0,
                    86_400,
                ) {
                    Ok(token) => state_tokens.push(token),
                    Err(e) => {
                        crate::channels::discord::eprintln_stamped!(
                            "[discord] could not create cron button state: {e}"
                        );
                        state_tokens.clear();
                        break;
                    }
                }
            }
            let (embed, buttons) = commands::cron_view(&mine, ctx.channel_id, &state_tokens);
            answer(ctx, &embed, &buttons, true).await;
        }
        Request::CronAdd { every, prompt } => {
            let interval =
                match commands::parse_interval(&every) {
                    Some(i) if i >= 60 => i,
                    _ => return answer(
                        ctx,
                        &commands::embed(
                            "Cron",
                            format!(
                                "`{every}` is not an interval. Use `30m`, `2h`, `1d` — 60s or more."
                            ),
                            &[],
                        ),
                        &[],
                        false,
                    )
                    .await,
                };
            let id = crate::channels::discord::durable::uuid_hex();
            let conv = ctx.conversation();
            match ctx.store.schedule_add(
                &id,
                interval as u64,
                &prompt,
                ctx.channel_id,
                &conv,
                crate::channels::discord::durable::now_secs(),
            ) {
                Ok(()) => {
                    let embed = commands::embed(
                        "Scheduled",
                        format!(
                            "`{}` will run every {} in <#{}>.\n> {}",
                            id,
                            commands::human(interval),
                            ctx.channel_id,
                            prompt.trim()
                        ),
                        &[],
                    );
                    answer(ctx, &embed, &[], true).await;
                }
                Err(e) => answer(ctx, &commands::embed("Cron", e, &[]), &[], false).await,
            }
        }
        Request::CronRemove { id } => match ctx.store.schedule_remove(&id) {
            Ok(()) => {
                answer(
                    ctx,
                    &commands::embed("Cron", format!("Deleted `{id}`."), &[]),
                    &[],
                    false,
                )
                .await
            }
            Err(e) => answer(ctx, &commands::embed("Cron", e, &[]), &[], false).await,
        },
        Request::ModelShow => {
            let (override_model, provider_model) = current_model(ctx);
            let effective =
                commands::effective_model(override_model.as_deref(), provider_model.as_deref());
            let mut fields = vec![json!({
                "name": "In use here",
                "value": format!("`{effective}`"),
                "inline": false,
            })];
            if let Some(m) = override_model {
                fields.push(json!({
                    "name": "Pinned by /model set",
                    "value": format!("`{m}`"),
                    "inline": false,
                }));
            }
            if let Some(m) = provider_model {
                fields.push(json!({
                    "name": "From gray's config",
                    "value": format!("`{m}`"),
                    "inline": false,
                }));
            }
            answer(
                ctx,
                &commands::embed(
                    "Model",
                    "`/model set <provider/model-id>` changes this channel only.",
                    &fields,
                ),
                &[],
                true,
            )
            .await;
        }
        Request::ModelSet { model } => {
            let model = model.trim().to_string();
            match ctx
                .store
                .meta_set(&commands::model_key(&ctx.conversation()), &model)
            {
                Ok(()) => {
                    answer(
                        ctx,
                        &commands::embed(
                            "Model",
                            format!("This channel now answers with `{model}`."),
                            &[],
                        ),
                        &[],
                        true,
                    )
                    .await
                }
                Err(e) => answer(ctx, &commands::embed("Model", e, &[]), &[], false).await,
            }
        }
        Request::MemoryList
        | Request::MemoryShow { .. }
        | Request::MemorySet { .. }
        | Request::MemoryRemove { .. } => {
            let argv = match &request {
                Request::MemoryList => commands::memory_argv("list", None, None),
                Request::MemoryShow { key } => commands::memory_argv("show", Some(key), None),
                Request::MemorySet { key, text } => {
                    commands::memory_argv("set", Some(key), Some(text))
                }
                Request::MemoryRemove { key } => commands::memory_argv("remove", Some(key), None),
                _ => unreachable!(),
            };
            // Deleting is the destructive one, so it stays with the caller.
            let destructive = matches!(&request, Request::MemoryRemove { .. });
            if destructive {
                let (stdout, stderr) = match gray_cli(ctx, &argv).await {
                    Ok(o) => o,
                    Err(e) => {
                        return answer(ctx, &commands::embed("Memory", e, &[]), &[], false).await;
                    }
                };
                answer(
                    ctx,
                    &commands::cli_embed("Memory", &stdout, &stderr, stderr.trim().is_empty()),
                    &[],
                    false,
                )
                .await;
            } else {
                answer_deferred(ctx, true, async move {
                    match gray_cli(ctx, &argv).await {
                        Ok((stdout, stderr)) => commands::cli_embed(
                            "Memory",
                            &stdout,
                            &stderr,
                            stderr.trim().is_empty(),
                        ),
                        Err(e) => commands::embed("Memory", e, &[]),
                    }
                })
                .await;
            }
        }
        Request::SetHome => {
            let owner = ctx
                .config
                .get("owner_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if owner.is_empty() || owner != ctx.user_id {
                let embed =
                    commands::embed("Home", "Only the owner can move gray's home channel.", &[]);
                return answer(ctx, &embed, &[], false).await;
            }
            let saved = crate::channels::discord::config::load_config(ctx.config_path).and_then(
                |mut config| {
                    config["channel_id"] = Value::String(ctx.channel_id.to_string());
                    crate::channels::discord::config::save_config(ctx.config_path, &config)
                },
            );
            let embed = match saved {
                Ok(()) => commands::embed(
                    "Home set",
                    "This chat is now gray's home. `discord_send` and `discord_file` post here unless they name another `target`.",
                    &[],
                ),
                Err(e) => {
                    commands::embed("Home", format!("Could not save the home channel: {e}"), &[])
                }
            };
            answer(ctx, &embed, &[], false).await;
        }
        Request::VoiceProvider { id } => {
            answer_deferred(ctx, true, async move {
                match crate::channels::discord::stt::provider(ctx.config, id.as_deref()).await {
                    Ok(text) => commands::embed("Voice", text, &[]),
                    Err(e) => commands::embed("Voice", e, &[]),
                }
            })
            .await;
        }
        Request::Gateway { args } => gateway_command(ctx, args.as_deref()).await,
        Request::Status => {
            let depth = ctx.store.pending_count().unwrap_or(0);
            let (override_model, provider_model) = current_model(ctx);
            let model =
                commands::effective_model(override_model.as_deref(), provider_model.as_deref());
            let embed = commands::embed(
                "Status",
                "Queue and model for this channel.",
                &[
                    json!({"name": "Turns queued", "value": depth.to_string(), "inline": true}),
                    json!({"name": "Model", "value": format!("`{model}`"), "inline": true}),
                    json!({"name": "Bridge", "value": format!("v{}", env!("CARGO_PKG_VERSION")), "inline": true}),
                ],
            );
            answer(ctx, &embed, &[], true).await;
        }
        Request::Ask { prompt } => {
            // The 3-second rule: acknowledge now, enqueue the turn, and let
            // the delivery loop answer through the webhook when gray is done.
            if let Err(e) = ctx.rest.defer_v2(ctx.int_id, ctx.int_token, true).await {
                crate::channels::discord::eprintln_stamped!("[discord] /ask defer failed: {e}");
                return;
            }
            let capacity = ctx
                .config
                .get("queue_capacity")
                .and_then(Value::as_u64)
                .unwrap_or(1000);
            let conv = ctx.conversation();
            match ctx
                .store
                .enqueue(ctx.int_id, ctx.channel_id, &prompt, Some(&conv), capacity)
            {
                Ok(true) => {
                    let _ = ctx
                        .store
                        .set_interaction(ctx.int_id, ctx.int_token, ctx.app_id);
                }
                Ok(false) => {
                    edit_deferred(
                        ctx,
                        &commands::embed(
                            "/ask",
                            "That turn is already queued (Discord redelivered it).",
                            &[],
                        ),
                        &[],
                        true,
                    )
                    .await;
                }
                Err(e) => {
                    crate::channels::discord::eprintln_stamped!(
                        "[discord] /ask enqueue failed: {e}"
                    );
                    edit_deferred(
                        ctx,
                        &commands::embed("/ask", format!("Cannot queue that: {e}"), &[]),
                        &[],
                        true,
                    )
                    .await;
                }
            }
        }
        Request::Reset => {
            let _canceled = ctx
                .store
                .cancel_pending_conversation(&ctx.conversation())
                .unwrap_or(false);
            if let Err(e) = crate::channels::discord::session::reset_home(
                &ctx.conversation_home(),
                crate::channels::discord::durable::now_secs(),
            ) {
                answer(ctx, &commands::embed("Session", e, &[]), &[], false).await;
                return;
            }
            answer(
                ctx,
                &commands::embed("Session", "Session reset.", &[]),
                &[],
                false,
            )
            .await;
        }
        Request::CmdRun {
            name,
            args,
            scope,
            surface,
            slow,
        } => {
            if name.trim_start_matches('/') == "gateway" {
                gateway_command(ctx, args.as_deref()).await;
            } else {
                crate::channels::discord::graycmd::run(
                    ctx,
                    &name,
                    args.as_deref(),
                    &scope,
                    &surface,
                    slow,
                )
                .await;
            }
        }
        Request::Stop => {
            let stopped = ctx
                .store
                .cancel_conversation(&ctx.conversation())
                .unwrap_or(false);
            let text = if stopped {
                "Stopping the running turn."
            } else {
                "Nothing running."
            };
            answer(ctx, &commands::embed("Stop", text, &[]), &[], false).await;
        }
        Request::Steer { text } => {
            let conv = ctx.conversation();
            let reply = if crate::channels::discord::runner::steer(&conv, &text).await {
                "Steering the current turn.".to_string()
            } else {
                let capacity = ctx
                    .config
                    .get("queue_capacity")
                    .and_then(Value::as_u64)
                    .unwrap_or(1000);
                match ctx
                    .store
                    .enqueue(ctx.int_id, ctx.channel_id, &text, Some(&conv), capacity)
                {
                    Ok(_) => "Nothing running — sent as a new message.".to_string(),
                    Err(e) => format!("Cannot queue that: {e}"),
                }
            };
            answer(ctx, &commands::embed("Steer", reply, &[]), &[], false).await;
        }
    }
}

/// Acknowledge an interaction shape the current bridge does not understand.
pub async fn unknown(ctx: &Ctx<'_>) {
    answer(
        ctx,
        &commands::embed("Command", "That command is no longer available.", &[]),
        &[],
        false,
    )
    .await;
}

/// Modal submissions are acknowledged even when no modal workflow is
/// currently registered. This prevents Discord's three-second retry storm if
/// an old or manually-created modal reaches the bridge.
pub async fn modal(ctx: &Ctx<'_>, custom_id: &str) {
    crate::channels::discord::eprintln_stamped!(
        "[discord] unhandled modal submission: {custom_id}"
    );
    answer(
        ctx,
        &commands::embed("Modal", "This Gray dialog is no longer active.", &[]),
        &[],
        false,
    )
    .await;
}

async fn update_original_or_answer(ctx: &Ctx<'_>, embed: &Value) {
    let components = rendered(ctx, embed, &[]);
    if let Err(e) = ctx
        .rest
        .interaction_update_v2(ctx.int_id, ctx.int_token, &components)
        .await
    {
        crate::channels::discord::eprintln_stamped!(
            "[discord] component update failed; sending fallback: {e}"
        );
        answer(ctx, embed, &[], false).await;
    }
}

/// The live card's Stop button: `turn:stop:<opaque-token>`. Any admitted
/// user in the turn's channel may press it once. The press flags the turn
/// and answers with the pressed card flipped to "stopping…" at once
/// (UPDATE_MESSAGE); the worker settles it properly on its next frame.
/// `message` is the pressed message's component tree, as Discord sent it.
pub async fn stop_button(ctx: &Ctx<'_>, custom_id: &str, message: Option<&Value>) {
    let token = custom_id.strip_prefix("turn:stop:").unwrap_or("");
    let stopped =
        match ctx
            .store
            .component_state_take(token, ctx.user_id, ctx.channel_id, "turn_stop")
        {
            Ok(Some(turn)) => ctx.store.cancel(&turn).is_ok(),
            Ok(None) => false,
            Err(e) => {
                crate::channels::discord::eprintln_stamped!("[discord] stop state failed: {e}");
                false
            }
        };
    if !stopped {
        return expired(ctx, "Stop", "That turn already finished.").await;
    }
    let flipped = message
        .and_then(|components| crate::channels::discord::stream::stopping(components, custom_id));
    let updated = match flipped {
        Some(components) => ctx
            .rest
            .interaction_update_v2(ctx.int_id, ctx.int_token, &components)
            .await
            .is_ok(),
        None => false,
    };
    if !updated && let Err(e) = ctx.rest.defer_update(ctx.int_id, ctx.int_token).await {
        crate::channels::discord::eprintln_stamped!("[discord] stop acknowledgement failed: {e}");
    }
}

/// A press on a question card (a plugin's `host/ask`): an option button, the
/// select menu, "Other…" (opens the text box), or the submitted text box
/// (`typed`). The answer is recorded and the card redrawn in the same
/// response; the asking sidecar picks it up from the store.
pub async fn ask_press(ctx: &Ctx<'_>, custom_id: &str, values: &[String], typed: Option<&str>) {
    use crate::channels::discord::ask::Choice;
    let closed = "This question is closed.";
    let Some(press) = crate::channels::discord::ask::parse_press(custom_id) else {
        return expired(ctx, "Question", closed).await;
    };
    let row = match ctx.store.ask_get(&press.ask_id) {
        Ok(Some(row)) if row.channel == ctx.channel_id && row.state == "open" => row,
        _ => return expired(ctx, "Question", closed).await,
    };
    let questions: Vec<crate::channels::discord::ask::Question> =
        serde_json::from_value(row.questions.clone()).unwrap_or_default();
    let Some(question) = questions.get(press.question) else {
        return expired(ctx, "Question", closed).await;
    };
    if press.choice == Choice::Other {
        match crate::channels::discord::ask::note_modal(&row, press.question) {
            Some(modal) => {
                if let Err(e) = ctx
                    .rest
                    .interaction_callback(
                        ctx.int_id,
                        ctx.int_token,
                        &json!({"type": 9, "data": modal}),
                    )
                    .await
                {
                    crate::channels::discord::eprintln_stamped!("[discord] ask modal failed: {e}");
                }
            }
            None => expired(ctx, "Question", closed).await,
        }
        return;
    }
    let answers = match press.choice {
        Choice::Note => typed
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| vec![crate::channels::discord::ask::note(text)]),
        _ => crate::channels::discord::ask::answers_for(question, &press.choice, values),
    };
    let Some(answers) = answers else {
        return expired(ctx, "Question", "That answer was empty.").await;
    };
    match ctx.store.ask_answer(
        &row.ask_id,
        &question.id,
        &answers,
        crate::channels::discord::durable::now_secs(),
    ) {
        Ok(Some(updated)) => {
            let card = crate::channels::discord::ask::render(&updated);
            if ctx
                .rest
                .interaction_update_v2(ctx.int_id, ctx.int_token, &card)
                .await
                .is_err()
            {
                let _ = ctx.rest.defer_update(ctx.int_id, ctx.int_token).await;
            }
        }
        _ => expired(ctx, "Question", "That question was already answered.").await,
    }
}

/// The private "that button no longer works" reply.
pub(crate) async fn expired(ctx: &Ctx<'_>, title: &str, text: &str) {
    answer(ctx, &commands::embed(title, text, &[]), &[], false).await;
}

/// A pressed button: `cron:remove:<opaque-token>`. The token is
/// consumed atomically and is valid only for the original user/channel.
pub async fn button(ctx: &Ctx<'_>, custom_id: &str) {
    let Some(token) = custom_id.strip_prefix("cron:remove:") else {
        answer(
            ctx,
            &commands::embed("Action", "That action is no longer available.", &[]),
            &[],
            false,
        )
        .await;
        return;
    };
    let resource =
        ctx.store
            .component_state_take(token, ctx.user_id, ctx.channel_id, "cron_remove");
    let id = match resource {
        Ok(Some(id)) => id,
        Ok(None) => {
            answer(
                ctx,
                &commands::embed("Cron", "That action expired; list the jobs again.", &[]),
                &[],
                false,
            )
            .await;
            return;
        }
        Err(e) => {
            crate::channels::discord::eprintln_stamped!("[discord] component state failed: {e}");
            answer(
                ctx,
                &commands::embed("Cron", "The action could not be completed.", &[]),
                &[],
                false,
            )
            .await;
            return;
        }
    };
    match ctx.store.schedule_remove(&id) {
        Ok(()) => {
            update_original_or_answer(
                ctx,
                &commands::embed("Cron", format!("Deleted `{id}`."), &[]),
            )
            .await
        }
        Err(e) => answer(ctx, &commands::embed("Cron", e, &[]), &[], false).await,
    }
}
