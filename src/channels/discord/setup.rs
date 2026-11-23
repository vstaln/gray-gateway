//! Interactive owner pairing.
//! Credentials stay in the wizard; the model never receives the token.
use serde_json::{Value, json};
use std::path::Path;

use crate::channels::discord::discord_check::{self, BotCheck, CheckError};

/// One-click OAuth2 invite : the bridge's full permission
/// set, bot + slash commands, server install.
pub fn invite(app_id: &str) -> String {
    discord_check::invite_url(app_id)
}

/// Discord rejections the wizard re-asks before giving up.
pub const TOKEN_TRIES: usize = 3;
/// Intent re-checks before the wizard moves on with the link.
pub const INTENT_RECHECKS: usize = 5;

/// Terminal IO. The real implementation talks to stdin/stderr; tests inject
/// a scripted fake. `print_line` goes to user-visible output (never logs).
pub trait Prompter {
    fn is_terminal(&self) -> bool;
    fn prompt(&mut self, text: &str) -> Result<String, String>;
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String>;
    fn confirm(&mut self, text: &str) -> Result<bool, String>;
    fn print_line(&mut self, text: &str);
}

/// Owner pairing result: `(owner_id, dm_channel_id)` from the DM wait.
pub type PairingResult = Result<(String, String), String>;
/// The pairing wait as a future: it holds a gateway connection open, so it
/// cannot be plain sync work.
pub type PairingFut = std::pin::Pin<Box<dyn std::future::Future<Output = PairingResult> + Send>>;
/// Injected wait for the owner's DM: receives the bot token and the printed
/// code, returns `(owner_id, dm_channel_id)`. Tests inject a scripted fake
/// (`&|_, _| Box::pin(async { .. })`); the CLI passes [`default_pairing`].
pub type WaitForPairing = dyn Fn(&str, &str) -> PairingFut;
/// Token check injected for tests (`default_check` in production).
pub type CheckStep = dyn Fn(&str) -> CheckFut;

/// Real terminal prompter (hidden input via `stty -echo`, no new deps).
pub struct Tty;

impl Prompter for Tty {
    fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    fn prompt(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let res = without_echo(|| std::io::stdin().read_line(&mut line));
        let _ = std::io::stderr().write_all(b"\n");
        res.map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim().to_string())
    }
    fn confirm(&mut self, text: &str) -> Result<bool, String> {
        let answer = self.prompt(text)?.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
    fn print_line(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// Echo off for one read. Ctrl-C would otherwise kill setup with the
/// terminal still silent, so SIGINT restores it first (tcsetattr is
/// async-signal-safe).
#[cfg(unix)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
    extern "C" fn restore_and_exit(_: libc::c_int) {
        if let Some(t) = SAVED.get() {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
        unsafe { libc::_exit(130) };
    }
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| term);
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    let handler = restore_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let old = unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &quiet);
        libc::signal(libc::SIGINT, handler)
    };
    let out = read();
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        libc::signal(libc::SIGINT, old);
    }
    out
}

/// Echo off for one read via the console input mode. If setup is Ctrl-C'd
/// mid-read the console stays silent, but cmd.exe restores its own mode at
/// the next prompt.
#[cfg(windows)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return read();
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return read();
        }
        let saved = mode;
        SetConsoleMode(handle, saved & !ENABLE_ECHO_INPUT);
        let out = read();
        SetConsoleMode(handle, saved);
        out
    }
}

#[cfg(not(any(unix, windows)))]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    read()
}

/// Resolve the gray binary (`GRAY_BIN` env or `PATH` lookup).
fn resolve_gray(explicit: &str) -> Option<String> {
    let candidate = explicit.trim();
    if candidate.is_empty() {
        return None;
    }
    if candidate.contains('/') || candidate.contains('\\') {
        let path = Path::new(candidate);
        if path.is_file() && is_executable(path) {
            return Some(candidate.to_string());
        }
        return None;
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let mut candidates = vec![dir.join(candidate)];
        if cfg!(windows) && !candidate.ends_with(".exe") {
            candidates.push(dir.join(format!("{candidate}.exe")));
        }
        for full in candidates {
            if full.is_file() && is_executable(&full) {
                return Some(full.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Run the setup wizard. `wait_for_pairing` polls gateway events for the
/// owner's DM; it receives the bot token and the printed code and returns
/// `(owner_id, dm_channel_id)`. Only used when the caller asks for pairing.
pub struct Wiring {
    /// Ask Discord about a token (`GET /applications/@me`).
    pub check: &'static CheckStep,
    /// Open the DM channel with a user (setup's home channel).
    pub dm: &'static DmStep,
    /// The app's own doctor, run after the config is written.
    pub verify: &'static VerifyStep,
    /// The gateway-code wait, for `--pair`.
    pub pairing: &'static WaitForPairing,
    /// Overrides for the silent gray-path resolution (`GRAY_BIN` / `GRAY_HOME`
    /// in production); tests pin them instead of mutating the environment.
    pub gray_bin: Option<String>,
    pub gray_home: Option<String>,
}

/// Everything setup needs from the outside world. Production defaults in
/// [`Wiring::production`]; tests inject stubs and keep the network out.
pub type DmStep = dyn Fn(&str, u64) -> DmFut;
pub type VerifyStep = dyn Fn(&Value) -> VerifyFut;

impl Wiring {
    pub fn production() -> Self {
        Self {
            check: &default_check,
            dm: &default_dm,
            verify: &default_verify,
            pairing: &default_pairing,
            gray_bin: None,
            gray_home: None,
        }
    }
}

/// The wizard. `pair` selects the DM-code dance for discovering the owner's
/// ID; the default path just asks for the ID (token + your
/// user ID, done).
pub async fn run(path: &Path, io: &mut dyn Prompter, pair: bool) -> Result<bool, String> {
    let wiring = Wiring::production();
    run_wired(path, io, &wiring, pair).await
}

pub async fn run_wired(
    path: &Path,
    io: &mut dyn Prompter,
    wiring: &Wiring,
    pair: bool,
) -> Result<bool, String> {
    if !io.is_terminal() {
        return Err("Setup needs a terminal for hidden token input".to_string());
    }
    if path.exists() && !io.confirm("Replace existing configuration? [y/N] ")? {
        return Ok(false);
    }
    // One secret, one identity. Gray's binary and home are
    // resolved silently (env, then the conventional defaults) — they are
    // derived facts, not questions.
    let gray = match &wiring.gray_bin {
        Some(g) => g.clone(),
        None => resolve_gray(&std::env::var("GRAY_BIN").unwrap_or_else(|_| "gray".to_string()))
            .ok_or_else(|| "Install gray first; executable not found".to_string())?,
    };
    let gray_home = match &wiring.gray_home {
        Some(h) => absolutize(Path::new(h)),
        None => absolutize(Path::new(&shellexpand_home(
            &std::env::var("GRAY_HOME").unwrap_or_else(|_| "~/.gray".to_string()),
        ))),
    };

    // Whoever the old config allowed stays allowed: the wizard only adds.
    let prior = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object);
    let (prior_owner, prior_allowed) = prior_access(prior.as_ref());

    for line in [
        "1. Open https://discord.com/developers/applications → New Application",
        "2. Open the Bot page → Reset Token → copy the token",
        "The token, the intents and the invite link are checked for you next.",
    ] {
        io.print_line(line);
    }
    let (token, bot) = prompt_checked_token(io, wiring).await?;
    let bot = match bot {
        Some(bot) => {
            let bot = ensure_message_content(io, wiring, &token, bot).await?;
            for line in discord_check::invite_lines(&bot) {
                io.print_line(&line);
            }
            Some(bot)
        }
        None => {
            io.print_line(OFFLINE_INTENT_NOTE);
            None
        }
    };

    // Who may talk to the bot: the owner plus an allowlist. The home channel
    // is then the DM with the owner — created, never asked for.
    let (owner, mut allowed) = if pair {
        let pairing = crate::channels::discord::policy::Pairing::new(now_secs());
        io.print_line(&format!(
            "DM this one-time code to the bot within five minutes: {}",
            pairing.code
        ));
        let (owner, dm) = (wiring.pairing)(&token, &pairing.code).await?;
        io.print_line(&format!("Owner ID: {owner} (home channel: your DM {dm})"));
        (owner, prior_allowed)
    } else if let Some((owner, allowed)) =
        offer_owner(io, bot.as_ref(), &prior_owner, &prior_allowed)?
    {
        (owner, allowed)
    } else {
        let answer = io.prompt("Your Discord user ID (comma-separated to also allow others): ")?;
        let mut ids = discord_check::clean_user_ids(&answer);
        anyhow_ids(&ids)?;
        let owner = ids.remove(0);
        (owner, discord_check::merge_allowed(&prior_allowed, &ids))
    };
    if !crate::channels::discord::config::snowflake(&Value::String(owner.clone())) {
        return Err("User IDs must be Discord snowflakes".to_string());
    }
    allowed.retain(|id| crate::channels::discord::config::snowflake(&Value::String(id.clone())));

    let owner_num: u64 = owner
        .parse()
        .map_err(|_| "User IDs must be Discord snowflakes".to_string())?;
    let dm_channel = (wiring.dm)(&token, owner_num).await?;

    // Python parity (`setup.py`): `Path(gray).absolute()`,
    // `gray_home.expanduser().resolve()`, `path.parent.resolve()` — the saved
    // config must hold absolute paths or `save_config` validation rejects it.
    let workdir = path
        .parent()
        .map(|p| absolutize(p).to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let mut saved = json!({
        "token": token,
        "owner_id": owner,
        "channel_id": dm_channel.to_string(),
        "gray_bin": absolutize(Path::new(&gray)).to_string_lossy(),
        "gray_home": gray_home.to_string_lossy(),
        "workdir": workdir,
        "session_reset": {
            "mode": "both",
            "idle_minutes": 1_440,
            "at_hour": 4
        }
    });
    if !allowed.is_empty() {
        saved["allowed_users"] = Value::Array(allowed.into_iter().map(Value::String).collect());
    }
    // A re-run replaces only what the wizard asked about: limits, budget,
    // shares and other hand-tuned keys survive (unless they no longer
    // validate, then the fresh config stands alone).
    if let Some(mut merged) = prior {
        for (k, v) in saved.as_object().into_iter().flatten() {
            if k != "session_reset" || merged.get(k).is_none() {
                merged[k] = v.clone();
            }
        }
        if saved.get("allowed_users").is_none() {
            if let Some(m) = merged.as_object_mut() {
                m.remove("allowed_users");
            }
        }
        if crate::channels::discord::config::validate_config(&merged).is_ok() {
            saved = merged;
        }
    }
    crate::channels::discord::config::save_config(path, &saved)?;
    io.print_line("Configuration saved privately.");

    // Prove it with the app's own doctor before anyone is told it worked;
    // a failure stops here so no service is started on a broken config.
    (wiring.verify)(&saved).await.map_err(|e| {
        format!("Saved, but the doctor disagreed: {e}. Fix it, then run `gray discord doctor` and `gray discord install`.")
    })?;
    io.print_line("Doctor verified the configuration.");
    Ok(true)
}

pub const OFFLINE_INTENT_NOTE: &str = "Make sure Message Content Intent is on (Bot page → Privileged Gateway Intents), or Discord will refuse the bot's connection.";

/// `owner_id` and `allowed_users` from the config being replaced, read raw
/// (it may not validate any more). Missing or unreadable means nobody.
fn prior_access(old: Option<&Value>) -> (Option<String>, Vec<String>) {
    let Some(old) = old else {
        return (None, Vec::new());
    };
    let owner = old
        .get("owner_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let allowed = old
        .get("allowed_users")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (owner, allowed)
}

/// Ask for the token until Discord accepts one. A rejected token is never
/// returned, so it can never be saved: three rejections end setup. A numeric
/// app-ID paste is refused once with guidance. `Ok((token, None))` means
/// Discord could not be asked: the token is kept, with a warning.
async fn prompt_checked_token(
    io: &mut dyn Prompter,
    wiring: &Wiring,
) -> Result<(String, Option<BotCheck>), String> {
    let mut rejected = 0;
    let mut numeric_warned = false;
    loop {
        let raw = io.prompt_hidden("Discord bot token (hidden): ")?;
        if !raw.is_ascii() {
            io.print_line(
                "Stripped non-ASCII characters (curly quotes or lookalike glyphs) from the pasted token.",
            );
        }
        let token = discord_check::clean_token(&raw);
        if token.is_empty() {
            return Err("A bot token is required".to_string());
        }
        if let Some(why) = discord_check::token_shape_error(&token) {
            if !numeric_warned {
                numeric_warned = true;
                io.print_line(why);
                continue;
            }
        }
        let why = if discord_check::has_inner_break(&token) {
            "That isn't a bot token (it contains a line break). Copy it again from the Bot page."
        } else {
            match (wiring.check)(&token).await {
                Ok(bot) => {
                    io.print_line(&format!(
                        "Token works: this is the bot \"{}\".",
                        bot.bot_name
                    ));
                    return Ok((token, Some(bot)));
                }
                Err(CheckError::Rejected) => {
                    "Discord rejected that token. On the Bot page click Reset Token, copy the new token and paste it here."
                }
                Err(CheckError::Status(code)) => {
                    io.print_line(&format!(
                        "Couldn't verify the token (Discord answered {code}); keeping it anyway."
                    ));
                    return Ok((token, None));
                }
                Err(CheckError::Unreachable(e)) => {
                    io.print_line(&format!(
                        "Couldn't reach Discord to verify the token ({e}); keeping it anyway."
                    ));
                    return Ok((token, None));
                }
            }
        };
        rejected += 1;
        if rejected >= TOKEN_TRIES {
            return Err("Discord rejected three tokens in a row; nothing was saved.".to_string());
        }
        io.print_line(why);
    }
}

/// Message Content Intent off: link straight to the toggle, Enter re-checks
/// (up to five times), `skip` keeps going. The doctor still refuses a bot
/// without it, so nothing claims success early.
async fn ensure_message_content(
    io: &mut dyn Prompter,
    wiring: &Wiring,
    token: &str,
    mut bot: BotCheck,
) -> Result<BotCheck, String> {
    for _ in 0..INTENT_RECHECKS {
        if bot.message_content {
            break;
        }
        for line in discord_check::intent_lines(&bot) {
            io.print_line(&line);
        }
        let answer = io.prompt("Press Enter once it's saved to re-check, or type 'skip': ")?;
        if answer.trim().eq_ignore_ascii_case("skip") {
            return Ok(bot);
        }
        match (wiring.check)(token).await {
            Ok(fresh) => bot = fresh,
            Err(_) => return Ok(bot),
        }
    }
    if bot.message_content {
        io.print_line("Message Content Intent is on.");
    }
    Ok(bot)
}

/// The owner Discord named, offered instead of a typed ID. Returns the owner
/// and the allowlist (prior entries kept, owners and extras only added), or
/// `None` when there is nobody to offer or the offer was declined.
fn offer_owner(
    io: &mut dyn Prompter,
    bot: Option<&BotCheck>,
    prior_owner: &Option<String>,
    prior_allowed: &[String],
) -> Result<Option<(String, Vec<String>)>, String> {
    let Some(bot) = bot.filter(|b| !b.owners.is_empty()) else {
        return Ok(None);
    };
    let who = discord_check::owner_names(bot);
    let question = if bot.owners.len() == 1 {
        format!("Allow yourself ({who}) to talk to the bot? [Y/n] ")
    } else {
        format!("Allow your team ({who}) to talk to the bot? [Y/n] ")
    };
    let answer = io.prompt(&question)?;
    if !matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    ) {
        return Ok(None);
    }
    let ids: Vec<String> = bot.owners.iter().map(|(id, _)| id.clone()).collect();
    // An owner already set stays the owner; the detected one joins the list.
    let owner = prior_owner.clone().unwrap_or_else(|| ids[0].clone());
    let mut allowed = discord_check::merge_allowed(prior_allowed, &ids);
    io.print_line(&format!(
        "You are allowlisted ({who}): owner detected, no Developer Mode needed."
    ));
    let extra = io.prompt("Other allowed user IDs (comma-separated, Enter to skip): ")?;
    allowed = discord_check::merge_allowed(&allowed, &discord_check::clean_user_ids(&extra));
    Ok(Some((owner, allowed)))
}

/// Snowflake check for every comma-separated ID, with the same error text.
fn anyhow_ids(ids: &[String]) -> Result<(), String> {
    if ids.is_empty() {
        return Err("At least your own user ID is required".to_string());
    }
    if ids
        .iter()
        .any(|id| !crate::channels::discord::config::snowflake(&Value::String(id.clone())))
    {
        return Err("User IDs must be Discord snowflakes".to_string());
    }
    Ok(())
}

/// The production DM open: POST /users/@me/channels with a 30 s bound.
pub type DmFut = std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, String>> + Send>>;

pub fn default_dm(token: &str, owner: u64) -> DmFut {
    let token = token.to_string();
    Box::pin(async move {
        let rest = crate::channels::discord::transport::Rest::production(&token);
        tokio::time::timeout(std::time::Duration::from_secs(30), rest.create_dm(owner))
            .await
            .map_err(|_| "Discord did not answer opening your DM".to_string())?
            .map_err(|e| e.to_string())
    })
}

/// The production doctor, bounded like the CLI's own (30 s).
pub type VerifyFut =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

pub fn default_verify(config: &Value) -> VerifyFut {
    let config = config.clone();
    Box::pin(async move {
        let token = config
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let rest = crate::channels::discord::transport::Rest::production(&token);
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            crate::channels::discord::doctor::check(&config, &rest),
        )
        .await
        .map_err(|_| "doctor timed out after 30s".to_string())?
    })
}

/// The production pairing wait: one bare gateway connection that accepts
/// exactly one DM carrying the printed code, then disconnects. Mirrors
/// The earlier wait loop, which polled the live gateway for the
/// owner's DM; the code expires after 300 s (`policy::Pairing`).
pub fn default_pairing(token: &str, code: &str) -> PairingFut {
    use twilight_gateway::{EventTypeFlags, Intents, Shard, ShardId, StreamExt};
    use twilight_model::gateway::event::Event;

    let token = token.to_string();
    let code = code.to_string();
    Box::pin(async move {
        let intents = Intents::GUILD_MESSAGES | Intents::DIRECT_MESSAGES | Intents::MESSAGE_CONTENT;
        let mut shard = Shard::new(ShardId::ONE, token, intents);
        let deadline = std::time::Duration::from_secs(300);
        let started = std::time::Instant::now();
        loop {
            let left = deadline.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            // Timeout, a fatal shard error, or a dropped stream all end the
            // wait the same way: the operator retries setup.
            let event =
                match tokio::time::timeout(left, shard.next_event(EventTypeFlags::MESSAGE_CREATE))
                    .await
                {
                    Ok(Some(Ok(event))) => event,
                    _ => break,
                };
            if let Event::MessageCreate(msg) = event {
                let m = &msg.0;
                // DMs only, never another bot, exact code match.
                if m.guild_id.is_none()
                    && !m.author.bot
                    && crate::channels::discord::policy::code_matches(&m.content, &code)
                {
                    return Ok((m.author.id.to_string(), m.channel_id.to_string()));
                }
            }
        }
        Err("Pairing failed or expired; check intent and DM permissions".to_string())
    })
}

/// Boxed check future: `Send` so the wizard stays `Send` for tokio.
pub type CheckFut =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<BotCheck, CheckError>> + Send>>;

/// Production check: `GET /applications/@me` with the bot token, 10 s bound.
pub fn default_check(token: &str) -> CheckFut {
    let token = token.to_string();
    Box::pin(async move {
        let rest = crate::channels::discord::transport::Rest::production(&token);
        discord_check::check_bot_token(&rest).await
    })
}

/// Join with cwd when relative (no `canonicalize`: the target may not
/// exist yet). Mirrors `Path.absolute()` / non-strict `resolve()`.
fn absolutize(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn shellexpand_home(raw: &str) -> String {
    // Python parity (`Path.expanduser`): bare `~` and `~/...` both expand.
    if raw == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).to_string_lossy().into_owned();
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{rest}", Path::new(&home).to_string_lossy());
        }
    }
    raw.to_string()
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
