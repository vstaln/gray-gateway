//! Generic bridge to gray's headless command surface (`gray cmd`).
//!
//! `commands.rs` owns the bridge's hand-rolled slash commands; this module
//! owns everything the gray binary can answer headlessly. At connect time
//! (and lazily, through a ~5 minute cache in `meta`) the gateway asks for
//! the registry (`gray cmd list`); every command it names that is not
//! bridge-owned registers as a flat slash command with one optional `args`
//! string and runs through `cmd run` in the right `GRAY_HOME`.
//!
//! `kind:ui` answers compile through `component_compile` exactly like
//! agent-authored documents — the `gray.ui` document body is the same
//! shape — with `gc:`-namespaced custom_ids so a press resolves through
//! the durable component-state machinery back to `gray cmd interact`.
//!
//! An old gray without `cmd` fails the registry fetch, the merge degrades
//! to today's static table, and dispatch never routes: zero regression.

use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::channels::discord::command_dispatch::{Ctx, edit_deferred, expired, gray_cli_in};
use crate::channels::discord::commands::{self, Request};
use crate::channels::discord::component_compile::{
    CompileContext, CompileError, ComponentIdAllocator, compile_message,
};
use crate::channels::discord::component_normalize;
use crate::channels::discord::component_protocol::{
    FileRef, MessageDocument, Origin, Surface, UI_VERSION, UiDocument,
};
use crate::channels::discord::component_state::{NewDocument, NewState, ResolvedState};
use crate::channels::discord::durable::{Store, now_secs};

/// Commands the bridge keeps even when `cmd list` advertises them:
/// behavior stays exactly what the static table already does.
pub const BRIDGE_OWNED: &[&str] = &[
    "ask", "stop", "steer", "reset", "new", "status", "cron", "memory", "sethome", "voice",
    "gateway",
];

/// `meta` key holding the cached registry JSON: {"at": secs, "commands": [...]}.
const REGISTRY_KEY: &str = "gray_cmd_registry";
/// How long a fetch stays fresh — for a remembered failure too, so an old
/// gray is not re-probed on every slash interaction.
const REGISTRY_TTL_SECS: f64 = 300.0;
/// Local component-state TTL; matches core's pending-doc token TTL.
const TOKEN_TTL_SECS: u64 = 3600;
/// `cmd list` is file-only; the short timeout protects a slash
/// interaction's three-second ack budget when the cache is stale.
const LIST_TIMEOUT: Duration = Duration::from_millis(2500);
/// The connect-time fetch can afford the usual CLI guard.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// `run`/`interact` share the same 10s guard as the bridge's other CLI calls.
const RUN_TIMEOUT: Duration = Duration::from_secs(10);
/// Registry rows marked `slow` (compaction, plugin calls, updates) get the
/// full deferred-interaction budget instead.
const SLOW_TIMEOUT: Duration = Duration::from_secs(120);
/// Discord's global-command ceiling; `/run` always gets a slot.
const COMMAND_LIMIT: usize = 100;

/// One row of `gray cmd list`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CmdInfo {
    pub name: String,
    pub description: String,
    pub aliases: Vec<String>,
    /// `conversation` or `global` — which `GRAY_HOME` the run belongs in.
    pub scope: String,
    /// `interactive`, `text`, or `local` (TUI-only, never registered).
    pub surface: String,
    /// The first declared argument's hint, used as the option's description.
    pub arg_hint: Option<String>,
    /// `builtin`, `plugin:<name>`, or `skill` — where the row came from.
    pub source: String,
    /// Runs that can exceed the normal timeout.
    pub slow: bool,
    pub completions: Vec<(String, String)>,
}

/// The `result` of a `cmd run` / `cmd interact` call.
#[derive(Debug, Clone, PartialEq)]
pub enum CmdResult {
    Text {
        text: String,
        close: bool,
    },
    Ui {
        token: String,
        document: Value,
        fallback_text: Option<String>,
    },
    Prompt {
        prompt: String,
        fallback_text: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct CmdError {
    pub code: String,
    pub message: String,
}

impl CmdError {
    fn wire(message: impl Into<String>) -> Self {
        Self {
            code: "wire".to_string(),
            message: message.into(),
        }
    }
}

/// Spawn `gray <argv>` in `home`, returning (stdout, stderr). The single
/// low-level transport every `cmd` verb — and `command_dispatch`'s gray
/// CLI calls — shares.
pub async fn spawn(
    gray_bin: &str,
    home: &Path,
    argv: &[String],
    timeout: Duration,
) -> Result<(String, String), String> {
    let child = tokio::process::Command::new(gray_bin)
        .args(argv)
        .env("GRAY_HOME", home)
        .env("GRAY_SHOW_REASONING", "0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run gray: {e}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| "gray did not answer in time".to_string())?
        .map_err(|e| format!("gray failed: {e}"))?;
    Ok((
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

/// `gray cmd list` against the user's main home — used at connect time for
/// the registration merge, before any conversation exists.
pub async fn fetch_registry(config: &Value) -> Option<Vec<CmdInfo>> {
    let bin = config.get("gray_bin").and_then(Value::as_str)?;
    let home = config.get("gray_home").and_then(Value::as_str)?;
    let (stdout, _) = spawn(
        bin,
        Path::new(home),
        &["cmd".to_string(), "list".to_string()],
        FETCH_TIMEOUT,
    )
    .await
    .ok()?;
    parse_list(&stdout)
}

/// Persist what a fetch learned — including failure (`commands: null`),
/// so an old gray is not re-probed on every slash interaction.
pub fn cache_registry(store: &Store, commands: Option<&[CmdInfo]>) {
    let body = json!({"at": now_secs(), "commands": commands});
    let _ = store.meta_set(REGISTRY_KEY, &body.to_string());
}

/// The cache entry: freshness plus `Some(registry)` / `None` (a remembered
/// failure — still cacheable) / `None` entry at all.
fn cache_entry(store: &Store) -> Option<(bool, Option<Vec<CmdInfo>>)> {
    let raw = store.meta_get(REGISTRY_KEY).ok()??;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let at = value.get("at").and_then(Value::as_f64).unwrap_or(0.0);
    let commands = match value.get("commands") {
        Some(Value::Null) | None => None,
        Some(list) => parse_commands(list),
    };
    Some((now_secs() - at <= REGISTRY_TTL_SECS, commands))
}

/// The registry at dispatch time: a fresh cache read, else one re-fetch in
/// this conversation's home (registry content is home-independent; the
/// spawn only needs somewhere to run). A stale entry outranks a fresh
/// failure — what worked before keeps routing until gray answers again.
pub async fn registry(ctx: &Ctx<'_>) -> Option<Vec<CmdInfo>> {
    if let Some((true, commands)) = cache_entry(ctx.store) {
        return commands;
    }
    let stale = cache_entry(ctx.store).and_then(|(_, commands)| commands);
    let home = ctx.conversation_home();
    let fetched = match gray_cli_in(
        ctx,
        &home,
        &["cmd".to_string(), "list".to_string()],
        LIST_TIMEOUT,
    )
    .await
    {
        Ok((stdout, _)) => parse_list(&stdout),
        Err(_) => None,
    };
    cache_registry(ctx.store, fetched.as_deref());
    fetched.or(stale)
}

/// Parse a `cmd list` envelope. `None` for anything that is not the
/// success shape — a wrong binary, an error envelope, or plain text.
pub fn parse_list(stdout: &str) -> Option<Vec<CmdInfo>> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    if !value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    parse_commands(value.pointer("/result/commands")?)
}

fn parse_commands(list: &Value) -> Option<Vec<CmdInfo>> {
    let mut out = Vec::new();
    for c in list.as_array()? {
        let Some(name) = c.get("name").and_then(Value::as_str) else {
            continue;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        out.push(CmdInfo {
            name,
            description: c
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            aliases: c
                .get("aliases")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            scope: c
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("conversation")
                .to_string(),
            surface: c
                .get("surface")
                .and_then(Value::as_str)
                .unwrap_or("text")
                .to_string(),
            arg_hint: c
                .get("args")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|a| a.get("hint"))
                .and_then(Value::as_str)
                // A cache read hands back the serialized struct, where the
                // hint is already flattened.
                .or_else(|| c.get("arg_hint").and_then(Value::as_str))
                .map(str::to_string),
            source: c
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("builtin")
                .to_string(),
            slow: c.get("slow").and_then(Value::as_bool).unwrap_or(false),
            completions: c
                .get("completions")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|entry| {
                            if let Some(name) = entry.get("name").and_then(Value::as_str) {
                                let description = entry
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                return Some((name.to_string(), description));
                            }
                            let pair = entry.as_array()?;
                            Some((
                                pair.first()?.as_str()?.to_string(),
                                pair.get(1)
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    Some(out)
}

/// Discord's global-command array. Bridge-owned commands keep their static
/// shapes; every other live registry command registers flat with one
/// optional `args` string; `local` commands are TUI-only and skipped.
/// Registry order is the host's priority (builtins, then plugins, then
/// skills), and the table is capped under Discord's 100 with `/run` kept
/// so everything that did not fit stays reachable. `None` (an old gray
/// without `cmd`) yields today's table verbatim.
pub fn registration_json(registry: Option<&[CmdInfo]>) -> Value {
    let mut out: Vec<Value> = Vec::new();
    let mut names: HashSet<String> = HashSet::new();
    for c in commands::COMMANDS {
        if registry.is_some() && !BRIDGE_OWNED.contains(&c.name) {
            continue;
        }
        names.insert(c.name.to_string());
        out.push(commands::command_json(c));
    }
    let Some(registry) = registry else {
        return Value::Array(out);
    };
    let budget = COMMAND_LIMIT.saturating_sub(out.len() + 1);
    let mut taken = 0usize;
    for c in registry {
        if taken >= budget {
            break;
        }
        if c.surface == "local" || BRIDGE_OWNED.contains(&c.name.as_str()) {
            continue;
        }
        let name = discord_name(&c.name);
        if name.is_empty() || name == "run" || !names.insert(name.clone()) {
            continue;
        }
        taken += 1;
        let description = if c.description.trim().is_empty() {
            format!("Run /{}", c.name)
        } else {
            commands::truncate(&c.description, 100)
        };
        let hint = c
            .arg_hint
            .as_deref()
            .filter(|h| !h.trim().is_empty())
            .map(|h| commands::truncate(h, 100))
            .unwrap_or_else(|| "Optional arguments".to_string());
        if c.name == "skills" {
            out.push(json!({
                "name": name,
                "description": description,
                "options": [
                    {"name": "name", "description": "Skill name", "type": 3, "required": false, "autocomplete": true},
                    {"name": "args", "description": "Optional arguments", "type": 3, "required": false},
                ],
            }));
            continue;
        }
        out.push(json!({
            "name": name,
            "description": description,
            "options": [{
                "name": "args",
                "description": hint,
                "type": 3,
                "required": false,
            }],
        }));
    }
    out.push(json!({
        "name": "run",
        "description": "Run a gray command by name",
        "options": [
            {"name": "name", "description": "Command name", "type": 3, "required": true, "autocomplete": true},
            {"name": "args", "description": "Optional arguments", "type": 3, "required": false},
        ],
    }));
    Value::Array(out)
}

/// Discord's `^[-_a-z0-9]{1,32}$`: lowercase, anything else becomes `-`,
/// clipped to 32 chars; empty results are dropped by the caller (the row
/// stays reachable through `/run`).
pub fn discord_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        out.push(
            if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' || lower == '-'
            {
                lower
            } else {
                '-'
            },
        );
    }
    out.truncate(32);
    out
}

/// Choices for `/run`'s `name` option: every registry name matching the
/// typed prefix/substring, canonical spelling as both label and value.
pub fn run_autocomplete(registry: Option<&[CmdInfo]>, query: &str) -> Vec<(String, String)> {
    let query = query.to_ascii_lowercase();
    registry
        .into_iter()
        .flatten()
        .filter(|c| c.source != "skill")
        .filter(|c| query.is_empty() || c.name.to_ascii_lowercase().contains(&query))
        .take(25)
        .map(|c| (c.name.clone(), c.name.clone()))
        .collect()
}

pub fn skills_autocomplete(registry: Option<&[CmdInfo]>, query: &str) -> Vec<(String, String)> {
    let query = query.to_ascii_lowercase();
    registry
        .into_iter()
        .flatten()
        .find(|c| c.name == "skills")
        .map(|c| {
            c.completions
                .iter()
                .filter(|(name, _)| query.is_empty() || name.to_ascii_lowercase().contains(&query))
                .take(25)
                .map(|(name, _)| (name.clone(), name.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// An incoming slash invocation as a `cmd run`, when `name` belongs to the
/// registry and not the bridge. The flat `args` option is read first, then
/// any legacy-shaped leftovers (a stale registration's sub-command) for
/// resilience; core owns the arg grammar either way.
pub fn route(
    registry: Option<&[CmdInfo]>,
    name: &str,
    sub: Option<&str>,
    values: &[(&str, &str)],
) -> Option<Request> {
    let registry = registry?;
    if name == "run" {
        let wanted = values
            .iter()
            .find(|(k, _)| *k == "name")
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())?;
        let cmd = registry
            .iter()
            .find(|c| c.name == wanted || c.aliases.iter().any(|a| a.as_str() == wanted));
        let args = values
            .iter()
            .find(|(k, _)| *k == "args")
            .map(|(_, v)| v.to_string())
            .filter(|s| !s.trim().is_empty());
        return Some(match cmd {
            Some(c) => Request::CmdRun {
                name: c.name.clone(),
                args,
                scope: c.scope.clone(),
                surface: c.surface.clone(),
                slow: c.slow,
            },
            None => Request::CmdRun {
                name: wanted.to_string(),
                args,
                scope: "conversation".to_string(),
                surface: "text".to_string(),
                slow: false,
            },
        });
    }
    let cmd = registry.iter().find(|c| {
        c.name == name
            || discord_name(&c.name) == name
            || c.aliases.iter().any(|a| a.as_str() == name)
    })?;
    if cmd.surface == "local" || BRIDGE_OWNED.contains(&cmd.name.as_str()) {
        return None;
    }
    let args = if cmd.name == "skills" {
        let joined: Vec<&str> = values.iter().map(|(_, v)| *v).collect();
        (!joined.is_empty()).then(|| joined.join(" "))
    } else {
        values
            .iter()
            .find(|(k, _)| *k == "args")
            .map(|(_, v)| v.to_string())
            .or_else(|| {
                let joined: Vec<&str> = sub
                    .into_iter()
                    .chain(values.iter().map(|(_, v)| *v))
                    .collect();
                (!joined.is_empty()).then(|| joined.join(" "))
            })
    }
    .filter(|s| !s.trim().is_empty());
    Some(Request::CmdRun {
        name: cmd.name.clone(),
        args,
        scope: cmd.scope.clone(),
        surface: cmd.surface.clone(),
        slow: cmd.slow,
    })
}

/// `/x [args]` → `gray cmd run x [args…]` in the right home, the result
/// rendered the same way every other command answer is. Picker surfaces
/// stay private; text answers are channel information like `/help`; a
/// `prompt` answer enqueues a turn exactly as if the user had typed it.
pub async fn run(
    ctx: &Ctx<'_>,
    name: &str,
    args: Option<&str>,
    scope: &str,
    surface: &str,
    slow: bool,
) {
    let public = surface != "interactive";
    if let Err(e) = ctx.rest.defer_v2(ctx.int_id, ctx.int_token, public).await {
        crate::channels::discord::eprintln_stamped!("[discord] /{name} defer failed: {e}");
        return;
    }
    let timeout = if slow { SLOW_TIMEOUT } else { RUN_TIMEOUT };
    let reply = match gray_cli_in(ctx, &home_for(ctx, scope), &run_argv(name, args), timeout).await
    {
        Ok((stdout, _)) => parse_reply(&stdout),
        Err(e) => Err(CmdError {
            code: "unavailable".to_string(),
            message: e,
        }),
    };
    let embed = match reply {
        Ok(CmdResult::Ui {
            token,
            document,
            fallback_text,
        }) => match render(ctx, name, scope, &token, &document, public) {
            Ok(components) => {
                if let Err(e) = ctx
                    .rest
                    .edit_original_v2(ctx.app_id, ctx.int_token, &components, public)
                    .await
                {
                    crate::channels::discord::eprintln_stamped!(
                        "[discord] /{name} ui answer failed: {e}"
                    );
                }
                return;
            }
            Err(e) => {
                crate::channels::discord::eprintln_stamped!(
                    "[discord] /{name} ui compile failed: {e}"
                );
                let text = fallback_text
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| "This answer needs a newer Discord client.".to_string());
                commands::embed(format!("/{name}"), text, &[])
            }
        },
        Ok(CmdResult::Prompt {
            prompt,
            fallback_text,
        }) => {
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
                    let text = fallback_text
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or_else(|| "Working on it.".to_string());
                    commands::embed(format!("/{name}"), text, &[])
                }
                Ok(false) => commands::embed(
                    format!("/{name}"),
                    "That turn is already queued (Discord redelivered it).",
                    &[],
                ),
                Err(e) => {
                    commands::embed(format!("/{name}"), format!("Cannot queue that: {e}"), &[])
                }
            }
        }
        Ok(CmdResult::Text { text, .. }) => {
            let text = if text.trim().is_empty() {
                "Done.".to_string()
            } else {
                text
            };
            let text = if name == "help" && !text.contains("/steer") {
                format!("{text}\n\n{}", commands::STEER_HELP)
            } else {
                text
            };
            commands::embed(format!("/{name}"), text, &[])
        }
        Err(err) => commands::embed(format!("/{name}"), err.message, &[]),
    };
    edit_deferred(ctx, &embed, &[], public).await;
}

/// A press on a `gc:` component: resolve the durable state back to the
/// core token and logical id, normalize the event into the contract's
/// action-json, and render whatever `cmd interact` answers with. The press
/// is acknowledged (`defer_update`) before the spawn — a slow binary must
/// never cost Discord its three-second component window.
pub async fn component(ctx: &Ctx<'_>, custom_id: &str, values: &[String], is_button: bool) {
    let Some(state_token) = custom_id.strip_prefix("gc:") else {
        return;
    };
    let resolved = match ctx
        .store
        .resolve_state(state_token, ctx.user_id, ctx.channel_id)
    {
        Ok(r) => r,
        Err(_) => return picker_expired(ctx, "").await,
    };
    let core = resolved
        .state
        .get("t")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = resolved
        .state
        .get("c")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let scope = resolved
        .state
        .get("s")
        .and_then(Value::as_str)
        .unwrap_or("conversation")
        .to_string();
    let public = resolved
        .state
        .get("p")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if is_stale(&resolved) || core.is_empty() {
        return picker_expired(ctx, &name).await;
    }
    if let Err(e) = ctx.rest.defer_update(ctx.int_id, ctx.int_token).await {
        crate::channels::discord::eprintln_stamped!(
            "[discord] /{name} component defer failed: {e}"
        );
        return;
    }
    let action = action_json(ctx.user_id, &resolved.logical_id, is_button, values);
    let argv = vec![
        "cmd".to_string(),
        "interact".to_string(),
        core.clone(),
        action.to_string(),
    ];
    let reply = match gray_cli_in(ctx, &home_for(ctx, &scope), &argv, RUN_TIMEOUT).await {
        Ok((stdout, _)) => parse_reply(&stdout),
        Err(e) => Err(CmdError {
            code: "unavailable".to_string(),
            message: e,
        }),
    };
    let components = match reply {
        Ok(CmdResult::Text { text, .. }) => {
            // Done: swap the picker for its closing text and retire its states.
            let _ = ctx.store.invalidate_document(&doc_key(&core));
            text_components(ctx, &text)
        }
        Ok(CmdResult::Ui {
            token,
            document,
            fallback_text,
        }) => match render(ctx, &name, &scope, &token, &document, public) {
            Ok(components) => components,
            Err(e) => {
                crate::channels::discord::eprintln_stamped!(
                    "[discord] /{name} ui re-render failed: {e}"
                );
                let text = fallback_text
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| "That answer could not be drawn.".to_string());
                text_components(ctx, &text)
            }
        },
        Ok(CmdResult::Prompt {
            prompt,
            fallback_text,
        }) => {
            let capacity = ctx
                .config
                .get("queue_capacity")
                .and_then(Value::as_u64)
                .unwrap_or(1000);
            let conv = ctx.conversation();
            let text =
                match ctx
                    .store
                    .enqueue(ctx.int_id, ctx.channel_id, &prompt, Some(&conv), capacity)
                {
                    Ok(true) => fallback_text
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or_else(|| "Working on it.".to_string()),
                    Ok(false) => "That turn is already queued.".to_string(),
                    Err(e) => format!("Cannot queue that: {e}"),
                };
            text_components(ctx, &text)
        }
        Err(err) => {
            let text = if err.code == "expired" {
                expired_text(&name)
            } else {
                err.message
            };
            text_components(ctx, &text)
        }
    };
    if let Err(e) = ctx
        .rest
        .edit_original_v2(ctx.app_id, ctx.int_token, &components, public)
        .await
    {
        crate::channels::discord::eprintln_stamped!(
            "[discord] /{name} component update failed: {e}"
        );
    }
}

/// `gray cmd run`'s argv: the name plus the `args` string split into
/// words. Core re-parses them against the command's own grammar.
pub fn run_argv(name: &str, args: Option<&str>) -> Vec<String> {
    let mut argv = vec!["cmd".to_string(), "run".to_string(), name.to_string()];
    if let Some(a) = args {
        argv.extend(a.split_whitespace().map(str::to_string));
    }
    argv
}

/// A `cmd run` / `cmd interact` envelope. Non-JSON stdout is a wire error —
/// an old gray prints its usage text, which is still the best message.
pub fn parse_reply(stdout: &str) -> Result<CmdResult, CmdError> {
    let trimmed = stdout.trim();
    let value: Value = serde_json::from_str(trimmed).map_err(|_| {
        CmdError::wire(if trimmed.is_empty() {
            "gray returned no answer".to_string()
        } else {
            trimmed.to_string()
        })
    })?;
    if !value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let error = value.get("error").cloned().unwrap_or_default();
        return Err(CmdError {
            code: error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("gray could not run that")
                .to_string(),
        });
    }
    let result = value.get("result").cloned().unwrap_or_default();
    match result.get("kind").and_then(Value::as_str) {
        Some("text") => Ok(CmdResult::Text {
            text: result
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            close: result
                .get("close")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        Some("ui") => Ok(CmdResult::Ui {
            token: result
                .get("token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            document: result.get("document").cloned().unwrap_or_default(),
            fallback_text: result
                .get("fallback_text")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        Some("prompt") => Ok(CmdResult::Prompt {
            prompt: result
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            fallback_text: result
                .get("fallback_text")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        _ => Err(CmdError::wire("gray answered with an unknown result kind")),
    }
}

/// Discord component event → contract action-json: a select submits
/// `values{logical_id:[..]}` (possibly empty), a button `submit:logical_id`.
pub fn action_json(user_id: &str, logical_id: &str, is_button: bool, values: &[String]) -> Value {
    if is_button {
        json!({"user": user_id, "submit": logical_id})
    } else {
        json!({"user": user_id, "values": {logical_id: values}})
    }
}

/// The `GRAY_HOME` a command belongs in: `global` rows run against the
/// user's main home, everything else against this channel's conversation.
fn home_for(ctx: &Ctx<'_>, scope: &str) -> PathBuf {
    if scope == "global" {
        PathBuf::from(
            ctx.config
                .get("gray_home")
                .and_then(Value::as_str)
                .unwrap_or(""),
        )
    } else {
        ctx.conversation_home()
    }
}

/// The store's document id for a pending core doc — derived from the core
/// token so a revised doc upserts the same row, invalidating the previous
/// components' states automatically.
fn doc_key(token: &str) -> String {
    format!(
        "gcmd:{}",
        &crate::channels::discord::runner::hex_sha256(token.as_bytes())[..24]
    )
}

/// The "run it again" tail of every expired-picker notice.
fn expired_text(name: &str) -> String {
    let hint = if name.is_empty() {
        "run the command again".to_string()
    } else {
        format!("run `/{name}` again")
    };
    format!("That picker expired — {hint}.")
}

/// The private "press on a dead picker" reply — a type-4 ephemeral, sent
/// while the interaction is still unanswered.
async fn picker_expired(ctx: &Ctx<'_>, name: &str) {
    expired(ctx, "Picker", &expired_text(name)).await;
}

/// A text answer as V2 components, with a guaranteed non-empty body.
/// Past the display bound the answer pages in place like any command.
fn text_components(ctx: &Ctx<'_>, text: &str) -> Vec<Value> {
    let text = if text.trim().is_empty() {
        "Done.".to_string()
    } else {
        text.to_string()
    };
    if text.chars().count() > crate::channels::discord::render::MAX_DISPLAY_CHARS {
        let pages = crate::channels::discord::pager::paginate(
            &text,
            crate::channels::discord::pager::PAGE_LIMIT,
        );
        if pages.len() > 1 {
            if let Ok(token) = crate::channels::discord::pager::mint(
                ctx.store,
                ctx.user_id,
                ctx.channel_id,
                &pages,
                "",
                crate::channels::discord::render::QUIET_ACCENT,
            ) {
                return crate::channels::discord::pager::components(
                    &pages,
                    0,
                    &token,
                    "",
                    crate::channels::discord::render::QUIET_ACCENT,
                );
            }
        }
    }
    crate::channels::discord::render::text_message(&text)
        .unwrap_or_else(|_| vec![json!({"type": 10, "content": text})])
}

/// A press minted under an older render: the re-render bumps the document
/// revision, so a state whose `r` no longer matches belongs to a message
/// Discord has already replaced.
fn is_stale(resolved: &ResolvedState) -> bool {
    resolved
        .state
        .get("r")
        .and_then(Value::as_u64)
        .is_some_and(|r| r as u32 != resolved.revision)
}

/// Strict-parse a `gray.ui` v1 document body for the compiler — the same
/// shape as `gray.discord.ui` v1 minus transport-only keys — with the
/// lenient coercer as a fallback for dialect drift.
fn ui_document(raw: &Value) -> Result<MessageDocument, String> {
    let mut value = raw.clone();
    let surface = value
        .as_object_mut()
        .map(|map| {
            map.remove("protocol");
            map.remove("surface")
                .and_then(|s| s.as_str().map(str::to_string))
                .unwrap_or_else(|| "message".to_string())
        })
        .unwrap_or_else(|| "message".to_string());
    if surface != "message" {
        return Err(format!("{surface} documents are not supported here"));
    }
    if let Ok(document) = serde_json::from_value::<MessageDocument>(value.clone()) {
        return Ok(document);
    }
    let coerced = component_normalize::coerce_document(&value, Surface::Message)?;
    match component_normalize::parse_document(&coerced)? {
        UiDocument::Message(document) => Ok(document),
        UiDocument::Modal(_) => Err("modal documents are not supported here".to_string()),
    }
}

/// Compile a `gray.ui` document, minting `gc:` state for its controls.
/// `public` is the visibility the message was posted with — stored per
/// state so a later press edits the message the same way it appeared.
fn render(
    ctx: &Ctx<'_>,
    name: &str,
    scope: &str,
    token: &str,
    raw: &Value,
    public: bool,
) -> Result<Vec<Value>, String> {
    let document = ui_document(raw)?;
    let document_id = doc_key(token);
    // Millisecond-granularity revision, wrapped into u32: two renders of
    // the same doc are always a CLI roundtrip apart.
    let revision = ((now_secs() * 1000.0) as u64 % u32::MAX as u64) as u32;
    ctx.store.create_document(NewDocument {
        document_id: document_id.clone(),
        owner_id: ctx.user_id.to_string(),
        guild_id: None,
        channel_id: ctx.channel_id.to_string(),
        message_id: None,
        modal_id: None,
        surface: "message".to_string(),
        revision,
        protocol_version: UI_VERSION,
        ttl_secs: TOKEN_TTL_SECS,
    })?;
    let mut allocator = GcAllocator {
        store: ctx.store,
        document_id: &document_id,
        user_id: ctx.user_id,
        channel_id: ctx.channel_id,
        revision,
        token,
        command: name,
        scope,
        public,
    };
    let mut compile = CompileContext::new(Origin::Plugin, &mut allocator);
    compile_message(&document, &mut compile)
        .map(|c| c.components)
        .map_err(|e| e.to_string())
}

/// The `custom_id` minter for `cmd` documents: every interactive node gets
/// a durable `gc:`-namespaced state row whose payload remembers the core
/// token, the owning command, the home scope, and the render revision.
struct GcAllocator<'a> {
    store: &'a Store,
    document_id: &'a str,
    user_id: &'a str,
    channel_id: &'a str,
    revision: u32,
    token: &'a str,
    command: &'a str,
    scope: &'a str,
    public: bool,
}

impl ComponentIdAllocator for GcAllocator<'_> {
    fn allocate(&mut self, logical_id: &str, kind: &str) -> Result<String, CompileError> {
        let state = json!({
            "t": self.token,
            "c": self.command,
            "s": self.scope,
            "r": self.revision,
            "p": self.public,
        });
        self.store
            .create_state(NewState {
                document_id: self.document_id.to_string(),
                logical_id: logical_id.to_string(),
                action: kind.to_string(),
                user_id: self.user_id.to_string(),
                channel_id: self.channel_id.to_string(),
                state,
                one_shot: false,
                ttl_secs: TOKEN_TTL_SECS,
            })
            .map(|token| format!("gc:{token}"))
            .map_err(|message| CompileError::new("state_unavailable", "$.components", message))
    }

    fn resolve_file(&self, file_id: &str) -> Result<FileRef, CompileError> {
        Err(CompileError::new(
            "file_not_found",
            file_id,
            "managed file is not available",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `cmd list` output shaped like the contract's v1 coverage.
    const LIST_FIXTURE: &str = r#"{
        "ok": true, "protocol": 1,
        "result": {"commands": [
            {"name": "help", "description": "registry + plugin command lines",
             "aliases": [], "args": [{"name": "command", "required": false, "hint": "command name"}],
             "scope": "conversation", "surface": "text"},
            {"name": "model", "description": "show or pick the model",
             "aliases": ["m"], "args": [{"name": "id", "required": false, "hint": "provider/model-id"}],
             "scope": "conversation", "surface": "interactive"},
            {"name": "usage", "description": "session tokens and cost",
             "aliases": [], "args": [], "scope": "conversation", "surface": "text"},
            {"name": "sessions", "description": "resumable sessions",
             "aliases": ["s"], "args": [], "scope": "conversation", "surface": "text"},
            {"name": "cron", "description": "job list",
             "aliases": [], "args": [], "scope": "conversation", "surface": "text"},
            {"name": "theme", "description": "TUI theme",
             "aliases": [], "args": [], "scope": "global", "surface": "local"}
        ]}
    }"#;

    /// The contract's reference `/model` picker document.
    const MODEL_DOC: &str = r#"{
        "protocol": "gray.ui", "version": 1, "surface": "message",
        "document_id": "model-picker", "visibility": "ephemeral",
        "lifecycle": "editable",
        "components": [
            {"type": "text", "content": "**Model** — `openai/gpt-5` · effort `medium` · fast `off`"},
            {"type": "action_row", "children": [
                {"type": "string_select", "logical_id": "model", "placeholder": "Model",
                 "options": [
                    {"label": "GPT-5", "value": "openai/gpt-5", "default": true},
                    {"label": "Claude Sonnet", "value": "anthropic/claude-sonnet-4"}
                 ]}
            ]},
            {"type": "action_row", "children": [
                {"type": "string_select", "logical_id": "effort", "placeholder": "Effort",
                 "options": [
                    {"label": "low", "value": "low"},
                    {"label": "medium", "value": "medium", "default": true},
                    {"label": "high", "value": "high"}
                 ]}
            ]},
            {"type": "action_row", "children": [
                {"type": "button", "logical_id": "apply", "label": "Apply", "style": "primary"},
                {"type": "button", "logical_id": "fast", "label": "fast: off", "style": "secondary"},
                {"type": "button", "logical_id": "cancel", "label": "Cancel", "style": "secondary"}
            ]}
        ]
    }"#;

    fn registry() -> Vec<CmdInfo> {
        parse_list(LIST_FIXTURE).expect("fixture parses")
    }

    fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(&dir.path().join("queue.sqlite3")).expect("store");
        (store, dir)
    }

    #[test]
    fn the_list_parses_every_field() {
        let commands = registry();
        let model = commands.iter().find(|c| c.name == "model").unwrap();
        assert_eq!(model.surface, "interactive");
        assert_eq!(model.aliases, vec!["m".to_string()]);
        assert_eq!(model.arg_hint.as_deref(), Some("provider/model-id"));
        assert_eq!(model.scope, "conversation");
    }

    #[test]
    fn the_list_rejects_everything_but_the_envelope() {
        assert!(parse_list("not json").is_none());
        assert!(parse_list("").is_none());
        assert!(
            parse_list(r#"{"ok": false, "error": {"code": "unavailable", "message": "no"}}"#)
                .is_none()
        );
        assert!(parse_list(r#"{"ok": true, "result": {"commands": "nope"}}"#).is_none());
    }

    #[test]
    fn registration_without_a_registry_is_the_static_table() {
        assert_eq!(registration_json(None), commands::registration_json());
    }

    #[test]
    fn registration_merges_bridge_rows_with_registry_rows() {
        let out = registration_json(Some(&registry()));
        let rows = out.as_array().unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
        // Bridge-owned commands keep their shapes; registry adds the rest.
        for owned in BRIDGE_OWNED {
            assert!(names.contains(owned), "/{owned} missing");
        }
        for extra in ["help", "model", "usage", "sessions"] {
            assert!(names.contains(&extra), "/{extra} missing");
        }
        // `local` rows are TUI-only; bridge-owned names are never doubled.
        assert!(!names.contains(&"theme"));
        assert_eq!(names.iter().filter(|n| **n == "cron").count(), 1);
        // Registry rows are flat: one optional `args` string option.
        let model = rows.iter().find(|r| r["name"] == "model").unwrap();
        let options = model["options"].as_array().unwrap();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0]["name"], "args");
        assert_eq!(options[0]["type"], 3);
        assert_eq!(options[0]["required"], false);
        // Bridge rows keep sub-commands, not `args`.
        let cron = rows.iter().find(|r| r["name"] == "cron").unwrap();
        assert!(
            cron["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["type"] == 1)
        );
    }

    #[test]
    fn routing_sends_registry_commands_to_cmd_run() {
        let registry = registry();
        match route(Some(&registry), "model", None, &[("args", "openai/gpt-5")]) {
            Some(Request::CmdRun {
                name,
                args,
                surface,
                ..
            }) => {
                assert_eq!(name, "model");
                assert_eq!(args.as_deref(), Some("openai/gpt-5"));
                assert_eq!(surface, "interactive");
            }
            other => panic!("model should route: {other:?}"),
        }
        // Bare `/model`, bridge-owned, TUI-only, and unknown names stay put.
        assert!(matches!(
            route(Some(&registry), "model", None, &[]),
            Some(Request::CmdRun { args: None, .. })
        ));
        assert!(route(Some(&registry), "cron", Some("list"), &[]).is_none());
        assert!(route(Some(&registry), "theme", None, &[]).is_none());
        assert!(route(Some(&registry), "nope", None, &[]).is_none());
        assert!(route(None, "model", None, &[]).is_none());
        // A stale registration's legacy shape still forwards something.
        match route(Some(&registry), "model", Some("set"), &[("model", "x/y")]) {
            Some(Request::CmdRun { args, .. }) => {
                assert_eq!(args.as_deref(), Some("set x/y"))
            }
            other => panic!("legacy shape should forward: {other:?}"),
        }
    }

    #[test]
    fn run_argv_splits_the_args_string() {
        assert_eq!(run_argv("model", None), vec!["cmd", "run", "model"]);
        assert_eq!(
            run_argv("model", Some("  openai/gpt-5  ")),
            vec!["cmd", "run", "model", "openai/gpt-5"]
        );
        assert_eq!(
            run_argv("help", Some("model picker")),
            vec!["cmd", "run", "help", "model", "picker"]
        );
    }

    #[test]
    fn the_reply_envelope_parses_both_kinds() {
        let text = parse_reply(r#"{"ok":true,"result":{"kind":"text","text":"hi","close":true}}"#)
            .unwrap();
        assert_eq!(
            text,
            CmdResult::Text {
                text: "hi".to_string(),
                close: true
            }
        );
        let ui = parse_reply(&format!(
            r#"{{"ok":true,"result":{{"kind":"ui","token":"tok-1","document":{MODEL_DOC},"fallback_text":"menu"}}}}"#
        ))
        .unwrap();
        match ui {
            CmdResult::Ui {
                token,
                document,
                fallback_text,
            } => {
                assert_eq!(token, "tok-1");
                assert_eq!(document["document_id"], "model-picker");
                assert_eq!(fallback_text.as_deref(), Some("menu"));
            }
            other => panic!("expected ui: {other:?}"),
        }
        let err = parse_reply(
            r#"{"ok":false,"error":{"code":"expired","message":"token TTL hit","retryable":false}}"#,
        )
        .unwrap_err();
        assert_eq!(err.code, "expired");
        // An old binary prints text, not an envelope.
        let wire = parse_reply("unknown command: cmd").unwrap_err();
        assert_eq!(wire.code, "wire");
    }

    #[test]
    fn actions_normalize_to_the_contract_shape() {
        let button = action_json("user-1", "apply", true, &[]);
        assert_eq!(button, json!({"user": "user-1", "submit": "apply"}));
        let select = action_json("user-1", "model", false, &["openai/gpt-5".to_string()]);
        assert_eq!(
            select,
            json!({"user": "user-1", "values": {"model": ["openai/gpt-5"]}})
        );
        // A select may legally submit an empty set.
        let cleared = action_json("user-1", "effort", false, &[]);
        assert_eq!(cleared, json!({"user": "user-1", "values": {"effort": []}}));
    }

    #[test]
    fn the_contract_picker_doc_parses_and_compiles() {
        let raw: Value = serde_json::from_str(MODEL_DOC).unwrap();
        let document = ui_document(&raw).expect("the contract doc parses");
        assert_eq!(document.document_id.0, "model-picker");
        assert_eq!(document.components.len(), 4);
        let mut allocator =
            crate::channels::discord::component_compile::DeterministicAllocator::default();
        let mut context = CompileContext::new(Origin::Plugin, &mut allocator);
        let compiled = compile_message(&document, &mut context).expect("the doc compiles");
        // Two selects + three buttons, each with a minted custom_id.
        let ids: Vec<&str> = compiled
            .components
            .iter()
            .filter_map(|row| row["components"].as_array())
            .flat_map(|children| children.iter())
            .filter_map(|c| c["custom_id"].as_str())
            .collect();
        assert_eq!(ids.len(), 5, "every control got a custom_id");
    }

    #[test]
    fn gc_states_round_trip_back_to_their_core_token() {
        let (store, _dir) = store();
        let revision = 7;
        store
            .create_document(NewDocument {
                document_id: "gcmd:test".into(),
                owner_id: "user-1".into(),
                guild_id: None,
                channel_id: "channel-1".into(),
                message_id: None,
                modal_id: None,
                surface: "message".into(),
                revision,
                protocol_version: UI_VERSION,
                ttl_secs: TOKEN_TTL_SECS,
            })
            .unwrap();
        let mut allocator = GcAllocator {
            store: &store,
            document_id: "gcmd:test",
            user_id: "user-1",
            channel_id: "channel-1",
            revision,
            token: "core-tok",
            command: "model",
            scope: "conversation",
            public: false,
        };
        let custom_id = allocator.allocate("model", "select").unwrap();
        assert!(custom_id.starts_with("gc:"));
        let resolved = store
            .resolve_state(&custom_id["gc:".len()..], "user-1", "channel-1")
            .unwrap();
        assert_eq!(resolved.logical_id, "model");
        assert_eq!(resolved.state["t"], json!("core-tok"));
        assert_eq!(resolved.state["c"], json!("model"));
        assert_eq!(resolved.state["r"], json!(7));
        assert_eq!(resolved.state["p"], json!(false));
        assert!(!is_stale(&resolved));
        // A press by someone else or elsewhere does not resolve.
        assert!(
            store
                .resolve_state(&custom_id["gc:".len()..], "user-2", "channel-1")
                .is_err()
        );
        // A re-render bumps the revision: the old press is now stale.
        store
            .create_document(NewDocument {
                document_id: "gcmd:test".into(),
                owner_id: "user-1".into(),
                guild_id: None,
                channel_id: "channel-1".into(),
                message_id: None,
                modal_id: None,
                surface: "message".into(),
                revision: revision + 1,
                protocol_version: UI_VERSION,
                ttl_secs: TOKEN_TTL_SECS,
            })
            .unwrap();
        let resolved = store
            .resolve_state(&custom_id["gc:".len()..], "user-1", "channel-1")
            .unwrap();
        assert!(is_stale(&resolved));
    }

    #[test]
    fn the_registry_cache_round_trips() {
        let (store, _dir) = store();
        assert!(cache_entry(&store).is_none());
        let commands = registry();
        cache_registry(&store, Some(&commands));
        let (fresh, cached) = cache_entry(&store).unwrap();
        assert!(fresh);
        assert_eq!(cached.unwrap().len(), commands.len());
        // A remembered failure is still a fresh answer.
        cache_registry(&store, None);
        let (fresh, cached) = cache_entry(&store).unwrap();
        assert!(fresh);
        assert!(cached.is_none());
    }
}
