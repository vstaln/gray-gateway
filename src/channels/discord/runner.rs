//! Isolated `gray -p --json` child per turn.
//! One conversation home per sha256(conversation); explicit session pointers
//! only — never guess a shared session id.
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::channels::discord::budget::{Budget, BudgetBlocked};

/// Hard cap on one turn when config has no `timeout_seconds`.
pub const DEFAULT_HARD_CAP_SECS: u64 = 14_400;
/// Silence from gray that ends a turn when config has no `inactivity_seconds`.
pub const DEFAULT_INACTIVITY_SECS: u64 = 600;

/// Cap on one NDJSON stdout line (matches the Python `limit=1024*1024`).
const LINE_CAP: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub enum RunError {
    Busy,
    Budget(BudgetBlocked),
    /// The hard cap on the whole turn (`timeout_seconds`) was reached.
    Timeout,
    /// No output from gray for the inactivity window (`inactivity_seconds`).
    Stalled(u64),
    /// The turn used all `max_requests` provider requests and was stopped.
    RequestLimit(u64),
    Spawn(String),
    Protocol(String),
    /// gray exited non-zero; the last stderr line, when there was one.
    Exit(Option<i32>, Option<String>),
    /// gray finished with an `error` row; its one-line reason.
    Agent(String),
    Incomplete,
    Empty,
    Io(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("This conversation is busy"),
            Self::Budget(e) => write!(f, "{e}"),
            Self::Timeout => f.write_str("gray turn hit its time limit"),
            Self::Stalled(secs) => write!(
                f,
                "gray turn stalled: no activity for {}",
                human_duration(*secs)
            ),
            Self::RequestLimit(n) => write!(f, "gray turn hit its {n}-request limit"),
            Self::Spawn(_) => f.write_str("cannot start gray agent"),
            Self::Protocol(_) => {
                f.write_str("Invalid agent JSON output; upgrade gray to a compatible version")
            }
            Self::Exit(Some(n), _) => write!(
                f,
                "gray exited with code {n}; not retrying possible side effects"
            ),
            Self::Exit(None, _) => {
                f.write_str("gray exited without a status; not retrying possible side effects")
            }
            Self::Incomplete => f.write_str(
                "Agent did not return a completed result; actions may already have occurred",
            ),
            Self::Empty => f.write_str("Agent returned no final answer"),
            Self::Agent(_) => f.write_str("gray turn failed"),
            Self::Io(_) => f.write_str("gray turn failed; check connectivity"),
        }
    }
}
impl std::error::Error for RunError {}

impl RunError {
    /// One line on why the turn failed, for the card footer, when known.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Agent(reason) => Some(reason.clone()),
            Self::Exit(code, reason) => reason
                .clone()
                .or_else(|| code.map(|n| format!("gray exited with code {n}"))),
            _ => None,
        }
    }
}

/// Boxed progress callback: one `--json` progress row in, nothing out,
/// never fails. The whole disclosed row (phase, tool, safe detail/output,
/// and internal call ID) so a renderer can use everything gray emits
/// without a second protocol.
pub type ProgressFn<'a> = Box<dyn FnMut(&Value) + Send + 'a>;

/// Answers a question a plugin asked through gray's `host/ask` (an `ask`
/// row on the `--json` wire): takes the row's `questions` and a flag that
/// turns true once the turn is over, resolves to answers by question id
/// (`{"<id>": ["<label>"]}`, empty when nobody answered).
pub type AskFn = std::sync::Arc<
    dyn Fn(
            Value,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>
        + Send
        + Sync,
>;

/// `None` once the turn's input is closed (see the terminal handling in
/// `run_gray_input`); a write then fails and the message becomes a new turn.
type SharedStdin = std::sync::Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>;

struct AskWire {
    handler: AskFn,
    stdin: SharedStdin,
    turn_over: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

fn live_stdins() -> &'static std::sync::Mutex<std::collections::HashMap<String, SharedStdin>> {
    static LIVE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, SharedStdin>>,
    > = std::sync::OnceLock::new();
    LIVE.get_or_init(Default::default)
}

struct LiveStdin {
    conversation: String,
    stdin: SharedStdin,
}

impl LiveStdin {
    fn register(conversation: &str, stdin: SharedStdin) -> Self {
        if let Ok(mut live) = live_stdins().lock() {
            live.insert(conversation.to_string(), stdin.clone());
        }
        Self {
            conversation: conversation.to_string(),
            stdin,
        }
    }
}

impl Drop for LiveStdin {
    fn drop(&mut self) {
        unregister(&self.conversation, &self.stdin);
    }
}

/// Removes `stdin` from the registry, but only if it is still the entry for
/// `conversation` (a newer turn may have replaced it).
fn unregister(conversation: &str, stdin: &SharedStdin) {
    if let Ok(mut live) = live_stdins().lock() {
        if live
            .get(conversation)
            .is_some_and(|s| std::sync::Arc::ptr_eq(s, stdin))
        {
            live.remove(conversation);
        }
    }
}

pub fn is_live(conversation: &str) -> bool {
    live_stdins()
        .lock()
        .map(|live| live.contains_key(conversation))
        .unwrap_or(false)
}

/// Longest a steer may wait on a turn's stdin. The gateway awaits steer inline
/// in its event loop, so a child that stopped reading (full pipe) must not
/// stall every other conversation.
const STEER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Writes a steer line to the turn running in `conversation`. `false` when
/// there is no such turn, the write fails, or the child does not take the
/// line within [`STEER_TIMEOUT`]; the caller then enqueues the text as a new
/// message. A timed-out child is unregistered and its stdin closed, so later
/// messages do not retry the wedged pipe.
pub async fn steer(conversation: &str, text: &str) -> bool {
    use tokio::io::AsyncWriteExt;
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let Some(stdin) = live_stdins()
        .lock()
        .ok()
        .and_then(|live| live.get(conversation).cloned())
    else {
        return false;
    };
    let line = format!("{}\n", serde_json::json!({"type": "steer", "text": text}));
    let write = async {
        let mut guard = stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return false;
        };
        stdin.write_all(line.as_bytes()).await.is_ok() && stdin.flush().await.is_ok()
    };
    match tokio::time::timeout(STEER_TIMEOUT, write).await {
        Ok(delivered) => delivered,
        Err(_) => {
            unregister(conversation, &stdin);
            // Closing stdin needs the lock the stalled write may hold; the
            // write is dropped by now, so this task gets it promptly.
            tokio::spawn(async move {
                *stdin.lock().await = None;
            });
            false
        }
    }
}

/// Per-turn options. `timeout_secs` defaults to `timeout_seconds` from config
/// (600 when absent); `progress` phases never fail the turn; `receipt` gets
/// the terminal row merged in, like the Python's `receipt.update(final)`.

#[derive(Default)]
pub struct RunOpts<'a> {
    pub timeout_secs: Option<u64>,
    /// Stop after this long with no output from gray. Defaults to
    /// `inactivity_seconds` from config (600 when absent).
    pub inactivity_secs: Option<u64>,
    pub progress: Option<ProgressFn<'a>>,
    pub receipt: Option<&'a mut Value>,
    /// Per-turn model override (`/model set`). Appended as `--model` only
    /// when present, so a channel that never picks one is unaffected.
    pub model: Option<String>,
    /// Show plugin questions (`host/ask`) to the person. Set, gray runs with
    /// `GRAY_JSON_ASK=1` and a piped stdin; unset, questions resolve empty
    /// as in any headless run. Inert unless a questions plugin is loaded:
    /// gray itself never asks.
    pub ask: Option<AskFn>,
}

/// Progress callback without a captured borrow: plain function pointer plus
/// an opaque `Send` payload. Lets the consume future own everything it
/// touches (the Python just awaits a closure; Rust needs the split).
pub struct ProgressCb<'a> {
    pub call: fn(&mut ProgressCtx<'a>, &Value),
    pub ctx: ProgressCtx<'a>,
}

pub struct ProgressCtx<'a> {
    pub inner: Option<ProgressFn<'a>>,
}

pub fn default_opts() -> RunOpts<'static> {
    RunOpts::default()
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunInput {
    Text(String),
    Structured(Value),
}

/// Run one isolated text turn. Compatibility wrapper for existing callers.
pub async fn run_gray(
    config: &Value,
    config_path: &Path,
    conversation: &str,
    prompt: &str,
    opts: RunOpts<'_>,
) -> Result<String, RunError> {
    run_gray_input(
        config,
        config_path,
        conversation,
        RunInput::Text(prompt.to_string()),
        opts,
    )
    .await
}

/// Run one isolated turn with either ordinary text or a versioned structured
/// input event. Structured JSON is written to a private per-turn file and
/// passed through Gray's native `--input-json` mode.
pub async fn run_gray_input(
    config: &Value,
    config_path: &Path,
    conversation: &str,
    input: RunInput,
    mut opts: RunOpts<'_>,
) -> Result<String, RunError> {
    let timeout_secs = match opts.timeout_secs {
        Some(0) => return Err(RunError::Io("Timeout must be positive".to_string())),
        Some(n) => n,
        None => config
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_HARD_CAP_SECS),
    };
    if timeout_secs == 0 {
        return Err(RunError::Io("Timeout must be positive".to_string()));
    }
    let inactivity_secs = match opts.inactivity_secs {
        Some(n) => n,
        None => config
            .get("inactivity_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_INACTIVITY_SECS),
    };
    if inactivity_secs == 0 {
        return Err(RunError::Io(
            "Inactivity window must be positive".to_string(),
        ));
    }
    match &input {
        RunInput::Text(prompt) if prompt.trim().is_empty() || prompt.chars().count() > 32000 => {
            return Err(RunError::Io(
                "Prompt must contain 1–32000 characters".to_string(),
            ));
        }
        RunInput::Structured(value) => validate_structured_input(value)?,
        RunInput::Text(_) => {}
    }

    let key = hex_sha256(conversation.as_bytes());
    let base = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let home = base.join("conversations").join(&key);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&home)
        .map_err(|_| RunError::Io("cannot create conversation home".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    }

    wait_for_reaper(conversation).await;
    // Non-blocking per-conversation lock, held for the whole turn.
    let Some(lock_file) = open_lock_file(&home.join("run.lock"))
        .map_err(|_| RunError::Io("cannot lock conversation".to_string()))?
    else {
        return Err(RunError::Busy);
    };
    let _lock = ConversationLock { _file: lock_file };
    let input = match input {
        RunInput::Text(prompt) => RunInput::Text(crate::channels::discord::always_on::with_posted(
            &home, prompt,
        )),
        other => other,
    };

    // Snapshot the provider config into the isolated home.
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut provider: Value = std::fs::read(Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| RunError::Io("gray provider configuration is missing".to_string()))?;
    if let Some(model) = config
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
    {
        provider["model"] = Value::String(model.to_string());
    }
    crate::channels::discord::config::atomic_json(&home.join("config.json"), &provider)
        .map_err(RunError::Io)?;

    // Dedicated workdir/profile: no lockfile plugins that might start
    // another gateway.
    let work = home.join("work");
    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&work)
        .or_else(|e| if work.is_dir() { Ok(()) } else { Err(e) })
        .map_err(|_| RunError::Io("cannot create work directory".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(windows)]
    let launcher = home.join("discord-sidecar.cmd");
    #[cfg(unix)]
    let launcher = home.join("discord-sidecar");
    // A reinstall under the running service reads as "<path> (deleted)";
    // the launcher must exec the replacement at <path>.
    let exe = std::env::current_exe()
        .map(|p| {
            let p = p.to_string_lossy();
            p.strip_suffix(" (deleted)").unwrap_or(&p).to_string()
        })
        .unwrap_or_else(|_| "gray-discord".to_string());
    let abs_config = config_path
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| config_path.to_string_lossy().into_owned());
    #[cfg(unix)]
    let script = format!(
        "#!/bin/sh\nexec {} discord sidecar --config {}\n",
        shell_word(&exe),
        shell_word(&abs_config)
    );
    #[cfg(windows)]
    let script = format!(
        "@echo off\r\n\"{}\" discord sidecar --config \"{}\"\r\n",
        exe, abs_config
    );
    std::fs::write(&launcher, script)
        .map_err(|_| RunError::Io("cannot write launcher".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| RunError::Io("cannot protect launcher".to_string()))?;
    }
    let shared =
        crate::channels::discord::capabilities::prepare(config, &home).map_err(RunError::Io)?;
    let profile = format!(
        "plugins:\n  - builtin: tools-minimal\n  - sidecar: {}\n{}",
        serde_json::to_string(&launcher.to_string_lossy()).unwrap_or_default(),
        shared
    );
    std::fs::write(work.join("gray.yml"), profile)
        .map_err(|_| RunError::Io("cannot write profile".to_string()))?;
    // Bound project discovery at the dedicated work directory.
    let _ = std::fs::create_dir(work.join(".git"));

    let state_path = home.join("session.json");
    let mut state: Value = std::fs::read(&state_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if !state.is_object() {
        state = Value::Object(Default::default());
    }
    let sessions = home.join("sessions");
    let mut files: Vec<PathBuf> = if sessions.is_dir() {
        std::fs::read_dir(&sessions)
            .map_err(|_| RunError::Io("cannot list sessions".to_string()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect()
    } else {
        Vec::new()
    };
    let now = crate::channels::discord::durable::now_secs();
    let has_session = state
        .get("session_id")
        .and_then(Value::as_str)
        .is_some_and(|sid| !sid.is_empty())
        || !files.is_empty();
    if has_session && reset_due(config, config_path, Path::new(gray_home), &state, now).await {
        crate::channels::discord::session::reset_home(&home, now).map_err(RunError::Io)?;
        state = serde_json::json!({
            "generation": crate::channels::discord::durable::uuid_hex(),
            "last_activity": now
        });
        files.clear();
    }
    if state
        .get("generation")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        state["generation"] = Value::String(crate::channels::discord::durable::uuid_hex());
    }
    state["last_activity"] = serde_json::json!(now);
    crate::channels::discord::config::atomic_json(&state_path, &state).map_err(RunError::Io)?;
    let generation = state
        .get("generation")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    files.sort_by_key(|p| std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).ok()));

    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if gray_bin.is_empty() {
        return Err(RunError::Io("gray executable is missing".to_string()));
    }
    let max_requests = config
        .get("max_requests")
        .and_then(Value::as_u64)
        .unwrap_or(200);
    let mut input_file = InputFileGuard::default();
    let mut args: Vec<String> = vec![gray_bin.to_string()];
    match &input {
        RunInput::Text(prompt) => {
            args.push("-p".to_string());
            args.push(prompt.clone());
        }
        RunInput::Structured(value) => {
            let path = home.join("turn-input.json");
            let encoded = serde_json::to_vec(value)
                .map_err(|_| RunError::Io("structured input could not be encoded".to_string()))?;
            std::fs::write(&path, encoded).map_err(|_| {
                RunError::Io("structured input file could not be written".to_string())
            })?;
            set_private_file(&path)?;
            input_file.path = Some(path.clone());
            args.push("--input-json".to_string());
            args.push(path.to_string_lossy().into_owned());
        }
    }
    args.extend([
        "--json".to_string(),
        "--max-requests".to_string(),
        max_requests.to_string(),
    ]);
    let policy = config.get("budget").cloned().unwrap_or(Value::Null);
    let budget_required = config
        .get("budget_required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut ledger: Option<Budget> = None;
    let reservation = reservation_id();
    if !policy.is_null() || budget_required {
        let budget = Budget::new(&base.join("budget.sqlite")).map_err(RunError::Io)?;
        let model = provider.get("model").and_then(Value::as_str).unwrap_or("");
        budget
            .reserve(&reservation, &policy, model)
            .map_err(RunError::Budget)?;
        for (flag, key) in [
            ("--max-cost-usd", "turn_usd"),
            ("--input-price", "input_per_million"),
            ("--output-price", "output_per_million"),
        ] {
            args.push(flag.to_string());
            args.push(value_text(policy.get(key).unwrap_or(&Value::Null)));
        }
        ledger = Some(budget);
    }
    let mut base_args = args;
    if let Some(model) = opts.model.as_ref().filter(|m| !m.trim().is_empty()) {
        base_args.push("--model".to_string());
        base_args.push(model.clone());
    }
    // Resume the pinned session, else the newest transcript in the home.
    let mut session_arg = state
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            files
                .first()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
        });
    let mut progress_cb = ProgressCb {
        call: |ctx, row| {
            if let Some(inner) = ctx.inner.as_deref_mut() {
                inner(row);
            }
        },
        ctx: ProgressCtx {
            inner: opts.progress.take(),
        },
    };
    let ask = opts.ask.take();
    let mut lock = Some(_lock);
    let spawn_cmd = |args: &[String]| -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&args[0]);
        cmd.args(&args[1..]);
        cmd.current_dir(&work);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        // Own process group so a timeout kill reaches grandchildren too.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        #[cfg(windows)]
        {
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        cmd.env_clear();
        for (k, v) in std::env::vars_os() {
            let Some(k_str) = k.to_str() else { continue };
            if k_str.starts_with("GRAY_")
                || k_str.starts_with("DISCORD_")
                || k_str.starts_with("OPENAI_")
            {
                continue;
            }
            cmd.env(k, v);
        }
        cmd.env("GRAY_HOME", &home);
        if let Some(user_home) = config
            .get("gray_home")
            .and_then(Value::as_str)
            .filter(|h| !h.trim().is_empty())
        {
            cmd.env("GRAY_USER_HOME", user_home);
        }
        cmd.env(
            "PATH",
            child_path(
                std::env::var_os("PATH"),
                std::env::var_os("HOME").map(PathBuf::from),
                Path::new(gray_bin),
            ),
        );
        // A job the model adds with a plain `gray cron add` inherits this
        // conversation's binding, so it comes back to the channel it was added
        // from. Absent outside a chat: the job is just local.
        if let Some(origin) = crate::channels::discord::cron::origin_env(&home) {
            cmd.env("GRAY_CRON_ORIGIN", origin);
        }
        cmd.env("GRAY_SKILLS_ONLY", "1");
        // Tool narration is safe to show; raw model reasoning is not. Keep the
        // wire quiet even when the activity bubble is enabled, matching the bridge.
        cmd.env("GRAY_SHOW_REASONING", "0");
        // Live replies: gray streams the prose as `text` rows and
        // the gateway edits the reply in place. A gray without the flag just
        // ignores it, and the answer is posted whole at the end as before.
        if crate::channels::discord::stream::enabled(config) {
            cmd.env("GRAY_STREAM_TEXT", "1");
        }
        cmd.env("GRAY_MAX_WALL_SECS", (timeout_secs.max(1)).to_string());
        cmd.env("GRAY_STDIN_STEER", "1");
        if ask.is_some() {
            cmd.env("GRAY_JSON_ASK", "1");
        }
        cmd
    };

    loop {
        let mut args = base_args.clone();
        if let Some(sid) = &session_arg {
            args.push("--session".to_string());
            args.push(sid.clone());
        }
        let mut child = spawn_cmd(&args[..])
            .spawn()
            .map_err(|_| RunError::Spawn("spawn failed".to_string()))?;
        let pid = child.id().unwrap_or(0);
        let mut stderr_tail = StderrTail::start(child.stderr.take());

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RunError::Spawn("no stdout".to_string()))?;
        let stdin: Option<SharedStdin> = child
            .stdin
            .take()
            .map(|s| std::sync::Arc::new(tokio::sync::Mutex::new(Some(s))));
        let live = stdin.clone().map(|s| LiveStdin::register(conversation, s));
        let turn_over = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let asks = match (ask.clone(), stdin.clone()) {
            (Some(handler), Some(stdin)) => Some(AskWire {
                handler,
                stdin,
                turn_over: turn_over.clone(),
            }),
            _ => None,
        };
        // Whatever way the turn ends, an open question card closes with it.
        struct TurnOver(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for TurnOver {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let _turn_over = TurnOver(turn_over);
        let consume = async {
            consume_ndjson(
                stdout,
                &state_path,
                &generation,
                &mut state,
                Some(&mut progress_cb),
                asks.as_ref(),
                inactivity_secs,
            )
            .await
        };
        // Hard cap on the whole turn; the inactivity watchdog inside
        // `consume_ndjson` is what stops a turn that has gone quiet.
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), consume).await;
        // However the read ended, the child takes no more input: unregister it so
        // a later message is not steered into it, then close its stdin.
        drop(live);
        if let Some(stdin) = stdin {
            tokio::spawn(async move {
                *stdin.lock().await = None;
            });
        }
        let consumed = match outcome {
            Ok(Ok(consumed)) => consumed,
            Ok(Err(e)) => {
                kill_group(pid);
                let _ = child.wait().await;
                log_failed_turn(pid, &e, &stderr_tail.text().await);
                return Err(e);
            }
            // Hard cap: kill the whole process group (Python: killpg SIGKILL).
            Err(_) => {
                kill_group(pid);
                let _ = child.wait().await;
                let e = RunError::Timeout;
                log_failed_turn(pid, &e, &stderr_tail.text().await);
                return Err(e);
            }
        };
        // A resumed session gray no longer knows about: nothing ran, so drop
        // the pointer and start once as a new session. The reservation is
        // settled at zero because no provider request was made.
        if session_arg.is_some() && consumed.terminal.as_ref().is_some_and(is_stale_session) {
            kill_group(pid);
            let _ = child.wait().await;
            if let Some(budget) = &ledger {
                budget.settle(
                    &reservation,
                    &serde_json::json!({"usage_complete": true, "cost_micros": 0}),
                );
            }
            if let Some(map) = state.as_object_mut() {
                map.remove("session_id");
            }
            crate::channels::discord::config::atomic_json(&state_path, &state)
                .map_err(RunError::Io)?;
            crate::channels::discord::eprintln_stamped!(
                "[discord] gray child {pid}: stale session pointer dropped; retrying once as a new session"
            );
            session_arg = None;
            continue;
        }
        if consumed.terminal.is_some() {
            // The terminal row is the answer and is published now. The child gets
            // a short grace period to exit on its own; the reaper kills it after
            // that and keeps the conversation lock until the child is gone.
            set_reaping(conversation, true);
            let lock = lock
                .take()
                .ok_or_else(|| RunError::Io("conversation lock missing".to_string()))?;
            tokio::spawn(reap_after_result(
                child,
                pid,
                lock,
                conversation.to_string(),
            ));
        } else {
            let status = match tokio::time::timeout(TURN_EXIT_GRACE, child.wait()).await {
                Ok(status) => status.map_err(|_| RunError::Spawn("wait failed".to_string()))?,
                Err(_) => {
                    kill_group(pid);
                    child
                        .wait()
                        .await
                        .map_err(|_| RunError::Spawn("wait failed".to_string()))?
                }
            };
            if !status.success() {
                let tail = stderr_tail.text().await;
                let e = RunError::Exit(status.code(), last_line(&tail));
                log_failed_turn(pid, &e, &tail);
                return Err(e);
            }
        }
        if let Some(final_row) = &consumed.terminal {
            if let Some(receipt) = opts.receipt.as_deref_mut() {
                for (k, v) in final_row.as_object().cloned().unwrap_or_default() {
                    receipt[k] = v;
                }
            }
            if ledger.is_some() && consumed.protocol_ok {
                let accounting = final_row.get("accounting").cloned().unwrap_or(Value::Null);
                if let Some(budget) = &ledger {
                    budget.settle(&reservation, &accounting);
                }
            }
        }
        // gray reports its request cap as a generic error row; the accounting
        // beside it says whether the cap is what stopped the turn.
        if let Some(row) = consumed.terminal.as_ref().filter(|r| r["type"] == "error") {
            if row["accounting"]["requests"].as_u64() >= Some(max_requests) {
                return Err(RunError::RequestLimit(max_requests));
            }
        }
        if !consumed.protocol_ok {
            let e = RunError::Protocol(
                "Invalid agent JSON output; upgrade gray to a compatible version".to_string(),
            );
            log_failed_turn(pid, &e, &stderr_tail.text().await);
            return Err(e);
        }
        let Some(final_row) = consumed.terminal else {
            let e = RunError::Incomplete;
            log_failed_turn(pid, &e, &stderr_tail.text().await);
            return Err(e);
        };
        if final_row.get("type").and_then(Value::as_str) == Some("error") {
            let e = RunError::Agent(error_reason(&final_row));
            log_failed_turn(pid, &e, &stderr_tail.text().await);
            return Err(e);
        }
        if final_row.get("type").and_then(Value::as_str) != Some("result")
            || final_row
                .get("session_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(RunError::Incomplete);
        }
        return match final_row.get("text").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => Ok(t.to_string()),
            _ => Err(RunError::Empty),
        };
    }
}

async fn reset_due(
    config: &Value,
    config_path: &Path,
    gray_home: &Path,
    state: &Value,
    now: f64,
) -> bool {
    match gray_home.join("gateway/session-reset.json").try_exists() {
        Ok(false) => {
            let effective = match config_path.try_exists() {
                Ok(false) => config.clone(),
                Ok(true) => match crate::channels::discord::config::load_config(config_path) {
                    Ok(saved) => saved,
                    Err(_) => {
                        crate::channels::discord::eprintln_stamped!(
                            "[discord] could not read the session reset policy; keeping session"
                        );
                        return false;
                    }
                },
                Err(_) => {
                    crate::channels::discord::eprintln_stamped!(
                        "[discord] could not read the session reset policy; keeping session"
                    );
                    return false;
                }
            };
            crate::channels::discord::session::ResetPolicy::from_config(&effective)
                .reset_reason(state, true, now)
                .is_some()
        }
        Ok(true) => {
            let bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
            let mut argv = ["gateway", "reset-policy", "--check"]
                .map(str::to_string)
                .to_vec();
            if let Some(last) = state.get("last_activity").and_then(Value::as_f64) {
                argv.push(format!("--last-activity={last}"));
            }
            match crate::channels::discord::graycmd::spawn(
                bin,
                gray_home,
                &argv,
                std::time::Duration::from_secs(3),
            )
            .await
            {
                Ok((out, _)) if out.trim() == "null" => false,
                Ok((out, _)) if matches!(out.trim(), "\"legacy\"" | "\"idle\"" | "\"daily\"") => {
                    true
                }
                _ => {
                    crate::channels::discord::eprintln_stamped!(
                        "[discord] Gray Gateway could not check the session reset policy; keeping session"
                    );
                    false
                }
            }
        }
        Err(_) => {
            crate::channels::discord::eprintln_stamped!(
                "[discord] could not locate the gateway reset policy; keeping session"
            );
            false
        }
    }
}

/// A resumed session that gray reports as missing before any model work:
/// zero requests and the `no session matching` error. Safe to retry once.
fn is_stale_session(row: &Value) -> bool {
    row.get("type").and_then(Value::as_str) == Some("error")
        && row["accounting"]["requests"].as_u64().unwrap_or(0) == 0
        && row
            .get("detail")
            .and_then(Value::as_str)
            .is_some_and(|d| d.contains("no session matching") && d.contains("not found"))
}

/// The error row's one-line reason, for the card and the log.
fn error_reason(row: &Value) -> String {
    let text = row
        .get("detail")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| row.get("message").and_then(Value::as_str))
        .unwrap_or("gray stopped without an answer");
    clip(
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(text),
    )
}

/// Last non-empty stderr line, one line for the card.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(clip)
}

/// Keep a reason short enough for a card footer.
fn clip(text: &str) -> String {
    const MAX: usize = 200;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    }
}

/// Failed turns go to the daemon log with gray's own stderr tail, so the
/// cause is on record and not only the `agent_failed` code.
fn log_failed_turn(pid: u32, error: &RunError, stderr: &str) {
    let why = error
        .reason()
        .map(|r| format!(" ({r})"))
        .unwrap_or_default();
    let tail = stderr.trim();
    if tail.is_empty() {
        crate::channels::discord::eprintln_stamped!(
            "[discord] gray child {pid} failed: {error}{why}"
        );
    } else {
        crate::channels::discord::eprintln_stamped!(
            "[discord] gray child {pid} failed: {error}{why}; stderr tail: {tail}"
        );
    }
}

/// Human duration for a card or a log line: `10m`, `4h`, `1h 30m`, `45s`.
pub fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 && m > 0 {
        format!("{h}h {m}m")
    } else if h > 0 {
        format!("{h}h")
    } else if m > 0 && s > 0 {
        format!("{m}m {s}s")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// The last bytes of a child's stderr. Read by its own task so a chatty
/// child can never block on a full pipe, and kept bounded.
struct StderrTail {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl StderrTail {
    fn start(stderr: Option<tokio::process::ChildStderr>) -> Self {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let reader = stderr.map(|mut err| {
            let buf = buf.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut chunk = [0u8; 1024];
                loop {
                    match err.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut tail) = buf.lock() {
                                tail.extend_from_slice(&chunk[..n]);
                                let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                                tail.drain(..excess);
                            }
                        }
                    }
                }
            })
        });
        Self { buf, reader }
    }

    /// The captured tail. Gives the reader a moment to drain what the child
    /// wrote before it exited; never waits longer than that.
    async fn text(&mut self) -> String {
        if let Some(reader) = self.reader.as_mut() {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(300), reader).await;
        }
        let bytes = self.buf.lock().map(|t| t.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Bytes of gray's stderr kept for a failed turn.
const STDERR_TAIL_BYTES: usize = 4096;

#[derive(Default)]
struct InputFileGuard {
    path: Option<PathBuf>,
}

impl Drop for InputFileGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A supervisor (runit, systemd) hands the daemon a bare PATH. gray's
/// provider plugins and their CLIs live beside the gray binary and in the
/// user's bin dirs, so put those first or every turn dies as `agent_failed`.
fn child_path(
    existing: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
    gray_bin: &Path,
) -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = gray_bin.parent() {
        dirs.push(dir.to_path_buf());
    }
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    if let Some(existing) = existing {
        dirs.extend(std::env::split_paths(&existing));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_default()
}

#[cfg(test)]
mod child_path_tests {
    use super::*;

    #[test]
    fn bare_supervisor_path_gains_user_bins() {
        let existing = std::env::join_paths([Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
        let got = child_path(
            Some(existing),
            Some(PathBuf::from("/home/u")),
            Path::new("/home/u/.local/bin/gray"),
        );
        let want = std::env::join_paths([
            PathBuf::from("/home/u/.local/bin"),
            Path::new("/home/u").join(".cargo/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
        .unwrap();
        assert_eq!(got, want);
    }
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn set_private_file(path: &Path) -> Result<(), RunError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|_| {
            RunError::Io("structured input file could not be protected".to_string())
        })?;
    }
    Ok(())
}

fn validate_structured_input(value: &Value) -> Result<(), RunError> {
    let object = value
        .as_object()
        .ok_or_else(|| RunError::Io("structured input must be an object".to_string()))?;
    if object.len() != 4
        || object.get("protocol").and_then(Value::as_str) != Some("gray.discord.input")
        || object.get("version").and_then(Value::as_u64) != Some(1)
        || object
            .get("kind")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind.is_empty() || kind.len() > 64)
        || !object.get("payload").is_some_and(Value::is_object)
    {
        return Err(RunError::Io(
            "structured input envelope is invalid".to_string(),
        ));
    }
    if serde_json::to_vec(value)
        .map(|bytes| bytes.len() > 1_048_576)
        .unwrap_or(true)
    {
        return Err(RunError::Io("structured input exceeds 1 MiB".to_string()));
    }
    Ok(())
}

struct ConversationLock {
    _file: std::fs::File,
}

impl Drop for ConversationLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: the descriptor is owned by this guard and remains valid
            // until this method returns.
            unsafe {
                libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

struct Consume {
    terminal: Option<Value>,
    protocol_ok: bool,
}

/// Read NDJSON rows: protocol-1 only, single turn id, session pinning with
/// atomic save on change, terminal result/error capture. Any malformed row
/// sets `protocol_ok = false` but keeps consuming (Python parity: record the
/// protocol error, still wait for the child, still settle on clean rows).
async fn consume_ndjson(
    stdout: tokio::process::ChildStdout,
    state_path: &Path,
    generation: &str,
    state: &mut Value,
    mut progress: Option<&mut ProgressCb<'_>>,
    asks: Option<&AskWire>,
    inactivity_secs: u64,
) -> Result<Consume, RunError> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stdout).lines();
    let mut turn_id: Option<String> = None;
    let mut terminal: Option<Value> = None;
    let mut protocol_ok = true;
    let mut after_terminal = 0usize;
    // The newest session id gray named, pinned at the end if it never got a
    // transcript mid-turn.
    let mut named_session: Option<String> = None;
    loop {
        let read = if terminal.is_some() {
            // The turn ended with the terminal row. Only rows already buffered
            // are read, so a violation right behind it is still caught, but a
            // child that lingers with stdout open cannot hold the turn.
            after_terminal += 1;
            if after_terminal > POST_TERMINAL_LINES {
                break;
            }
            match tokio::time::timeout(std::time::Duration::ZERO, lines.next_line()).await {
                Ok(read) => read,
                Err(_) => break,
            }
        } else {
            // Every row restarts the window: a long run that keeps talking is
            // never cut off; one that goes quiet for the whole window is.
            let window = std::time::Duration::from_secs(inactivity_secs);
            match tokio::time::timeout(window, lines.next_line()).await {
                Ok(read) => read,
                Err(_) => return Err(RunError::Stalled(inactivity_secs)),
            }
        };
        let line = read.map_err(|_| RunError::Io("cannot read agent output".to_string()))?;
        let Some(line) = line else { break };
        if line.len() > LINE_CAP {
            protocol_ok = false;
            continue;
        }
        let row: Value = match serde_json::from_str(&line) {
            Ok(Value::Object(map)) => Value::Object(map),
            _ => {
                protocol_ok = false;
                continue;
            }
        };
        if row.get("type").and_then(Value::as_str) == Some("retract") {
            if terminal.is_none() {
                if let Some(cb) = progress.as_deref_mut() {
                    let mut row = row;
                    row["phase"] = Value::String("retract".to_string());
                    (cb.call)(&mut cb.ctx, &row);
                }
            }
            continue;
        }
        if row.get("protocol").and_then(Value::as_i64) != Some(1) {
            protocol_ok = false;
            continue;
        }
        let row_turn = row.get("turn_id").and_then(Value::as_str).unwrap_or("");
        if row_turn.is_empty() || (turn_id.is_some() && turn_id.as_deref() != Some(row_turn)) {
            protocol_ok = false;
            continue;
        }
        turn_id = Some(row_turn.to_string());
        // Data after the terminal row is a protocol violation (Python:
        // "Data after terminal agent result"), not a second result.
        if terminal.is_some() {
            protocol_ok = false;
            break;
        }
        if let Some(sid) = row.get("session_id").and_then(Value::as_str) {
            if session_id_valid(sid) {
                // `/new` advances the on-disk generation while an old child
                // may still be finishing. Never let that child resurrect its
                // predecessor's pointer.
                if !crate::channels::discord::session::generation_is_current(state_path, generation)
                {
                    protocol_ok = false;
                    continue;
                }
                // One turn names one session: compare with what this turn
                // already named, else with the pinned id.
                let expected = named_session.clone().or_else(|| {
                    state
                        .get("session_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                });
                if expected.as_deref().is_some_and(|e| e != sid) {
                    protocol_ok = false;
                    continue;
                }
                named_session = Some(sid.to_string());
            } else {
                protocol_ok = false;
                continue;
            }
        }
        match row.get("type").and_then(Value::as_str) {
            Some("result") | Some("error") => {
                terminal = Some(row);
            }
            Some("progress") if row.get("phase").and_then(Value::as_str) == Some("ask") => {
                if let Some(asks) = asks {
                    answer_ask(asks, &row);
                }
            }
            Some("progress") => {
                if let Some(cb) = progress.as_deref_mut() {
                    (cb.call)(&mut cb.ctx, &row);
                }
            }
            _ => {
                protocol_ok = false;
            }
        }
    }
    // Pin the session the turn named only when it finished cleanly, or gray
    // wrote its transcript: a turn that failed before its first write leaves
    // no session to resume, and a stale pointer would fail every later turn.
    if let Some(sid) = named_session {
        let succeeded = protocol_ok
            && terminal
                .as_ref()
                .and_then(|t| t.get("type"))
                .and_then(Value::as_str)
                == Some("result");
        let pinned = state
            .get("session_id")
            .and_then(Value::as_str)
            .is_some_and(|p| !p.is_empty());
        if !pinned && (succeeded || transcript_exists(state_path, &sid)) {
            state["session_id"] = Value::String(sid);
            let _ = crate::channels::discord::config::atomic_json(state_path, state);
        }
    }
    Ok(Consume {
        terminal,
        protocol_ok,
    })
}

/// Whether gray has written a transcript for this session in the home.
fn transcript_exists(state_path: &Path, sid: &str) -> bool {
    fn walk(dir: &Path, name: &str, depth: u8) -> bool {
        if depth > 4 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|e| {
            let p = e.path();
            if p.is_dir() {
                walk(&p, name, depth + 1)
            } else {
                p.file_name().and_then(|f| f.to_str()) == Some(name)
            }
        })
    }
    let Some(home) = state_path.parent() else {
        return false;
    };
    walk(&home.join("sessions"), &format!("{sid}.jsonl"), 0)
}

/// Answer one `ask` row on its own task, so gray's output keeps flowing
/// while the person thinks: the handler's answers go back to gray's stdin
/// as `{"ask_id":N,"answers":{...}}`.
fn answer_ask(asks: &AskWire, row: &Value) {
    let Some(ask_id) = row.get("ask_id").and_then(Value::as_u64) else {
        return;
    };
    let questions = row.get("questions").cloned().unwrap_or(Value::Null);
    let pending = (asks.handler)(questions, asks.turn_over.clone());
    let stdin = asks.stdin.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let answers = pending.await;
        let line = format!(
            "{}\n",
            serde_json::json!({"ask_id": ask_id, "answers": answers})
        );
        let mut guard = stdin.lock().await;
        if let Some(stdin) = guard.as_mut() {
            let _ = stdin.write_all(line.as_bytes()).await;
            let _ = stdin.flush().await;
        }
    });
}

/// How long a gray child may keep running after its terminal row before its
/// process group is killed.
const TURN_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Rows read after the terminal row (only those already buffered) before the
/// reader stops looking for a protocol violation.
const POST_TERMINAL_LINES: usize = 64;

/// Exclusive lock on `path`: `Some(file)` while another process has not
/// already locked it. Unix takes `flock` on an FD_CLOEXEC descriptor;
/// Windows opens with `share_mode(0)`, which closes the lock with the file.
#[cfg(unix)]
pub(crate) fn open_lock_file(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::create(path)?;
    let fd = f.as_raw_fd();
    // Do not let another concurrently spawned gray child inherit this lock
    // descriptor. Otherwise a child for conversation B can keep conversation
    // A's flock alive after A's runner future has returned.
    let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fd_flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let locked = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
    Ok(locked.then_some(f))
}

#[cfg(windows)]
pub(crate) fn open_lock_file(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::windows::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .share_mode(0)
        .open(path)
    {
        Ok(f) => Ok(Some(f)),
        // ERROR_SHARING_VIOLATION: another process holds the lock.
        Err(e) if e.raw_os_error() == Some(32) => Ok(None),
        Err(e) => Err(e),
    }
}

/// SIGKILL the child's process group (it leads its own, see `pre_exec`).
#[cfg(unix)]
fn kill_group(pid: u32) {
    if pid != 0 {
        // SAFETY: plain kill(2) on the negated pgid of a child we spawned.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

/// The process-tree equivalent on Windows: `taskkill /T` walks children.
#[cfg(windows)]
fn kill_group(pid: u32) {
    if pid != 0 {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", pid.to_string().as_str(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Wait for a child whose terminal row was already taken as the answer, and
/// kill it if it has not exited within the grace period. The conversation
/// lock is held until the child is gone, so the next turn cannot overlap it.
async fn reap_after_result(
    mut child: tokio::process::Child,
    pid: u32,
    lock: ConversationLock,
    conversation: String,
) {
    match tokio::time::timeout(TURN_EXIT_GRACE, child.wait()).await {
        Ok(Ok(status)) if !status.success() => {
            crate::channels::discord::eprintln_stamped!(
                "[discord] gray child {pid} exited {:?} after its result",
                status.code()
            );
        }
        Ok(_) => {}
        Err(_) => {
            kill_group(pid);
            let _ = child.wait().await;
            crate::channels::discord::eprintln_stamped!(
                "[discord] gray child {pid} still running {}s after its result; killed",
                TURN_EXIT_GRACE.as_secs()
            );
        }
    }
    // Release the lock before the next turn is let in.
    drop(lock);
    set_reaping(&conversation, false);
}

/// Conversations whose previous child is still inside its grace period. A new
/// turn for one of them waits instead of reporting the conversation busy.
fn reaping() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static REAPING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    REAPING.get_or_init(Default::default)
}

fn set_reaping(conversation: &str, on: bool) {
    if let Ok(mut set) = reaping().lock() {
        if on {
            set.insert(conversation.to_string());
        } else {
            set.remove(conversation);
        }
    }
}

fn is_reaping(conversation: &str) -> bool {
    reaping().lock().is_ok_and(|set| set.contains(conversation))
}

/// Wait (bounded) for a previous child's grace period to end, so that a turn
/// right after an answer is not rejected as busy by a child that is exiting.
async fn wait_for_reaper(conversation: &str) {
    let deadline = std::time::Instant::now() + TURN_EXIT_GRACE + std::time::Duration::from_secs(5);
    while is_reaping(conversation) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn session_id_valid(s: &str) -> bool {
    // gray >= 0.1.11 names sessions with readable lowercase words
    // (`ionic-nickel-supernova`); older releases emitted canonical UUIDs.
    // Both are dash-separated lowercase [a-z0-9] runs; keep the check
    // structural so a word id never trips a protocol error.
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 64 || b[0] == b'-' || b[b.len() - 1] == b'-' {
        return false;
    }
    let mut prev_dash = false;
    for c in b {
        let dash = *c == b'-';
        if (dash && prev_dash) || !(dash || c.is_ascii_lowercase() || c.is_ascii_digit()) {
            return false;
        }
        prev_dash = dash;
    }
    true
}

#[cfg(test)]
mod session_id_tests {
    use super::session_id_valid;

    #[test]
    fn accepts_uuid_and_word_ids() {
        assert!(session_id_valid("cff0d620-d17c-456a-bb6c-d24af7ddf859"));
        assert!(session_id_valid("ionic-nickel-supernova"));
        assert!(session_id_valid("abc"));
    }

    #[test]
    fn rejects_junk() {
        for bad in [
            "",
            "-leading",
            "trailing-",
            "double--dash",
            "Upper-Case",
            "has space",
            "semi;colon",
        ] {
            assert!(!session_id_valid(bad), "{bad:?}");
        }
        assert!(!session_id_valid(&"a".repeat(65)));
    }
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn reservation_id() -> String {
    // 16 hex chars of OS entropy (Python: uuid4 hex, truncated display).
    let mut buf = [0u8; 8];
    crate::channels::discord::durable::fill_entropy(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Single-quote shell escaping for launcher argv words.
#[cfg(unix)]
fn shell_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
