//! CLI tree: `gray-discord --config <path> <subcommand>`.
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Standalone Discord transport and setup for gray.
#[derive(Debug, Parser)]
#[command(name = "gray-discord", bin_name = "gray discord")]
pub struct Cli {
    /// Config file path.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn config_path(&self) -> PathBuf {
        match &self.config {
            Some(p) => p.clone(),
            None => crate::channels::discord::config::default_path(),
        }
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Interactive setup. Default: the token is checked with
    /// Discord and the app's owner is offered as you; home channel is your
    /// DM. `--pair` instead discovers your ID from a DM you send the bot
    /// (a one-time code dance).
    Setup {
        /// Discover the owner's ID by DMing a one-time code to the bot.
        #[arg(long)]
        pair: bool,
    },
    Run,
    Sidecar {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
    /// Print the plugin manifest (what `gray plugin install discord` reads).
    #[command(hide = true)]
    Manifest,
    Register,
    Install,
    Status,
    Stop,
    Restart,
    Doctor,
    Uninstall,
    Limits {
        #[arg(long)]
        timeout_seconds: Option<u64>,
        #[arg(long)]
        concurrency: Option<u64>,
        #[arg(long)]
        max_requests: Option<u64>,
    },
    Budget {
        #[command(subcommand)]
        action: BudgetAction,
    },
    Share {
        #[arg(long)]
        skill: Vec<String>,
        #[arg(long)]
        context: Vec<String>,
        #[arg(long, name = "plugin-argv")]
        plugin_argv: Vec<String>,
        #[arg(long)]
        clear: bool,
    },
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    Allowlist {
        #[command(subcommand)]
        action: AllowlistAction,
    },
    Pairing {
        #[command(subcommand)]
        action: PairingAction,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum BudgetAction {
    Status,
    Set {
        #[arg(long, name = "daily-usd")]
        daily_usd: f64,
        #[arg(long, name = "turn-usd")]
        turn_usd: f64,
        #[arg(long, name = "input-per-million")]
        input_per_million: f64,
        #[arg(long, name = "output-per-million")]
        output_per_million: f64,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum QueueAction {
    List,
    Cancel { id: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ScheduleAction {
    Add {
        #[arg(long)]
        every: u64,
        prompt: String,
    },
    List,
    Remove {
        id: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum AllowlistAction {
    Add { id: String },
    Remove { id: String },
    List,
}

#[derive(Debug, Clone, Subcommand)]
pub enum PairingAction {
    /// Approve a code the bot handed an unconfigured DMer: they join the
    /// allowlist (or become the owner if none is set yet).
    Approve {
        /// Platform the code was issued for. Only `discord` exists.
        platform: String,
        /// The code from the bot's DM reply.
        code: String,
        /// The Discord user ID shown in that same DM. Approval is refused
        /// unless it matches, so a code passed along by someone else is not enough.
        #[arg(long = "as")]
        as_user: String,
    },
}

/// Register the outgoing `discord_send` tool in gray's plugin lock.
/// Matches `gray_plugin::lock::LockEntry` (explicit argv, no fake install).
pub fn register(config: &Value, config_path: &Path) -> Result<(), String> {
    use serde_json::{Value, json};
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let lock = Path::new(gray_home).join("plugins/lock.json");
    let mut data: Value = std::fs::read(&lock)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({"schema": 1, "plugins": {}}));
    if data.get("schema").and_then(Value::as_u64) != Some(1)
        || !data.get("plugins").is_some_and(Value::is_object)
    {
        return Err("Unsupported gray plugin lock; not changing it".to_string());
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|_| "cannot locate plugin binary".to_string())?;
    let resolved = config_path
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| config_path.to_string_lossy().into_owned());
    let argv = vec![
        Value::String(exe.clone()),
        Value::String("discord".to_string()),
        Value::String("sidecar".to_string()),
        Value::String("--config".to_string()),
        Value::String(resolved),
    ];
    let prev_exe = data
        .pointer("/plugins/discord/argv/0")
        .and_then(Value::as_str);
    if prev_exe.is_some_and(|p| p != exe && Path::new(p).is_file()) {
        return Err("Another discord plugin is registered; refusing to overwrite it".to_string());
    }
    let installed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    // Merge into any existing row: gray now keeps cli_argv and the consent
    // record (granted_capabilities, capabilities_hash) in the same lock entry,
    // so replacing the object would drop them.
    let entry = &mut data["plugins"]["discord"];
    if !entry.is_object() {
        *entry = json!({});
    }
    entry["ecosystem"] = json!("gray-native");
    entry["version"] = json!(env!("CARGO_PKG_VERSION"));
    entry["source"] = json!("https://github.com/vstaln/gray-discord-plugin");
    entry["argv"] = Value::Array(argv);
    entry["adapter_version"] = json!("1.1");
    entry["installed_at"] = json!(installed_at);
    if entry.get("hash").is_none() {
        entry["hash"] = json!("");
    }
    if entry.get("scope").is_none() {
        entry["scope"] = json!("user");
    }
    if entry.get("enabled").is_none() {
        entry["enabled"] = json!(true);
    }
    // Re-register heals a row whose CLI binary moved (reinstall, cargo
    // install to a new prefix); a working custom cli_argv is kept.
    let cli_ok = entry
        .get("cli_argv")
        .and_then(|a| a.get(0))
        .and_then(Value::as_str)
        .is_some_and(|p| Path::new(p).is_file());
    if !cli_ok {
        entry["cli_argv"] = json!([exe]);
    }
    crate::channels::discord::config::atomic_json(&lock, &data)
}

/// Parse the service-install answer. Python parity (`cli.py`):
/// `input('Enable and start the background service now? [Y/n] ').strip().lower()`
/// in `('', 'y', 'yes')` — default yes.
pub fn install_confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes")
}

/// Dispatch a parsed subcommand. Arms without a landed task fail closed so
/// no half-wired command can run.
pub fn run(cmd: &Command, config_path: &Path) -> Result<(), String> {
    use serde_json::Value;
    match cmd {
        Command::Sidecar { args } => {
            if args.is_empty() {
                crate::channels::discord::sidecar::serve(config_path)
            } else {
                let argv = std::iter::once("gray-discord".to_string()).chain(args.iter().cloned());
                let cli = Cli::try_parse_from(argv).map_err(|e| e.to_string())?;
                let cfg = cli.config.unwrap_or_else(|| config_path.to_path_buf());
                run(&cli.command, &cfg)
            }
        }
        Command::Manifest => {
            use clap::CommandFactory;
            let mut m = crate::channels::discord::sidecar::MANIFEST.clone();
            m["completion"] = Cli::command()
                .get_subcommands()
                .filter(|c| {
                    !c.is_hide_set() && !["run", "sidecar", "register"].contains(&c.get_name())
                })
                .map(|c| Value::String(c.get_name().to_string()))
                .collect();
            m["sidecar_args"] = serde_json::json!(["sidecar"]);
            println!("{m}");
            Ok(())
        }
        Command::Register => {
            let config: Value = crate::channels::discord::config::load_config(config_path)?;
            register(&config, config_path)
        }
        Command::Setup { pair } => {
            use crate::channels::discord::setup::Prompter;
            let mut io = crate::channels::discord::setup::Tty;
            // setup is async; drive it on a throwaway current-thread runtime
            // so `run` stays sync like the other CLI arms.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "cannot start setup runtime".to_string())?;
            let done = rt.block_on(crate::channels::discord::setup::run(
                config_path,
                &mut io,
                *pair,
            ))?;
            if done {
                let config: Value = crate::channels::discord::config::load_config(config_path)?;
                register(&config, config_path)?;
                println!("Outgoing tool registered with gray.");
                if crate::channels::discord::service::installed(config_path) {
                    // A reconfigured bot takes effect now.
                    println!(
                        "{}",
                        crate::channels::discord::service::restart(config_path)?
                    );
                    return Ok(());
                }
                let answer = io.prompt("Enable and start the background service now? [Y/n] ")?;
                if install_confirmed(&answer) {
                    let line = crate::channels::discord::service::install(config_path)?;
                    println!("{line}");
                    println!("Run gray discord status to check it.");
                    if matches!(
                        crate::channels::discord::service::detect(),
                        crate::channels::discord::service::Supervisor::SystemdUser { .. }
                    ) {
                        println!("For operation after logout: loginctl enable-linger \"$USER\"");
                    }
                } else {
                    println!(
                        "Run gray discord install when ready, or gray discord run in the foreground."
                    );
                }
            }
            Ok(())
        }
        Command::Run => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|_| "cannot start gateway runtime".to_string())?;
            rt.block_on(crate::channels::discord::gateway::run(config_path))
        }
        Command::Status => {
            println!(
                "{}",
                crate::channels::discord::service::status(config_path)?
            );
            Ok(())
        }
        Command::Stop => {
            println!("{}", crate::channels::discord::service::stop(config_path)?);
            Ok(())
        }
        Command::Restart => {
            println!(
                "{}",
                crate::channels::discord::service::restart(config_path)?
            );
            Ok(())
        }
        Command::Uninstall => {
            let line = crate::channels::discord::service::uninstall(config_path)?;
            println!("{line}");
            println!(
                "Private configuration and sessions retained; registered outgoing tool retained."
            );
            Ok(())
        }
        Command::Install => {
            let _ = crate::channels::discord::config::load_config(config_path)?;
            let line = crate::channels::discord::service::install(config_path)?;
            println!("{line}");
            if matches!(
                crate::channels::discord::service::detect(),
                crate::channels::discord::service::Supervisor::SystemdUser { .. }
            ) {
                println!("To survive logout, enable user linger: loginctl enable-linger \"$USER\"");
            }
            Ok(())
        }
        Command::Doctor => {
            let config = crate::channels::discord::config::load_config(config_path)?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "cannot start runtime".to_string())?;
            rt.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    crate::channels::discord::doctor::doctor(&config),
                )
                .await
                .map_err(|_| "doctor timed out after 30s".to_string())?
            })
        }
        Command::Share {
            skill,
            context,
            plugin_argv,
            clear,
        } => {
            let mut config = crate::channels::discord::config::load_config(config_path)?;
            if *clear {
                config["shared_skills"] = Value::Array(Vec::new());
                config["shared_context"] = Value::Array(Vec::new());
                config["shared_plugins"] = Value::Array(Vec::new());
            }
            let mut skills: Vec<String> = config
                .get("shared_skills")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            for s in skill {
                if !skills.contains(s) {
                    skills.push(s.clone());
                }
            }
            config["shared_skills"] = Value::Array(skills.into_iter().map(Value::String).collect());

            let mut contexts: Vec<String> = config
                .get("shared_context")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            for c in context {
                if !contexts.contains(c) {
                    contexts.push(c.clone());
                }
            }
            config["shared_context"] =
                Value::Array(contexts.into_iter().map(Value::String).collect());

            let mut plugins: Vec<Value> = config
                .get("shared_plugins")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for p_str in plugin_argv {
                let parsed: Value = serde_json::from_str(p_str)
                    .map_err(|_| "Invalid plugin-argv JSON".to_string())?;
                plugins.push(parsed);
            }
            config["shared_plugins"] = Value::Array(plugins);

            let tmp_dir = std::env::temp_dir().join(format!(
                "gray-cap-{}",
                crate::channels::discord::durable::uuid_hex()
            ));
            std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("cannot create tempdir: {e}"))?;
            let res = crate::channels::discord::capabilities::prepare(&config, &tmp_dir);
            let _ = std::fs::remove_dir_all(&tmp_dir);
            res?;
            crate::channels::discord::config::save_config(config_path, &config)?;
            println!(
                "Shared capabilities saved. Only select trusted code/non-secret context. Restart to apply."
            );
            Ok(())
        }
        Command::Limits {
            timeout_seconds,
            concurrency,
            max_requests,
        } => {
            let mut config = crate::channels::discord::config::load_config(config_path)?;
            if let Some(n) = timeout_seconds {
                config["timeout_seconds"] = Value::from(*n);
            }
            if let Some(n) = concurrency {
                config["concurrency"] = Value::from(*n);
            }
            if let Some(n) = max_requests {
                config["max_requests"] = Value::from(*n);
            }
            crate::channels::discord::config::save_config(config_path, &config)?;
            println!("Limits saved; restart the gateway to apply.");
            Ok(())
        }
        Command::Budget { action } => match action {
            BudgetAction::Set {
                daily_usd,
                turn_usd,
                input_per_million,
                output_per_million,
            } => {
                let mut config = crate::channels::discord::config::load_config(config_path)?;
                let gray_home = config
                    .get("gray_home")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "gray_home missing".to_string())?;
                let provider_bytes = std::fs::read(Path::new(gray_home).join("config.json"))
                    .map_err(|_| "gray provider configuration is missing".to_string())?;
                let provider: Value = serde_json::from_slice(&provider_bytes)
                    .map_err(|_| "Invalid gray config.json".to_string())?;
                let model = provider
                    .get("model")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "model missing".to_string())?;

                let new_budget = serde_json::json!({
                    "model": model,
                    "daily_usd": daily_usd,
                    "turn_usd": turn_usd,
                    "input_per_million": input_per_million,
                    "output_per_million": output_per_million,
                });
                crate::channels::discord::budget::validate(&new_budget, model)?;
                config["budget"] = new_budget;
                crate::channels::discord::config::save_config(config_path, &config)?;
                println!("Budget saved for selected model; restart the gateway to apply.");
                Ok(())
            }
            BudgetAction::Status => {
                let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
                let b =
                    crate::channels::discord::budget::Budget::new(&parent.join("budget.sqlite"))?;
                println!("Accounted/reserved micro-USD: {}", b.total());
                Ok(())
            }
        },
        Command::Schedule { action } => {
            let config = crate::channels::discord::config::load_config(config_path)?;
            let store = crate::channels::discord::gateway::open_store(config_path)?;
            match action {
                ScheduleAction::Add { every, prompt } => {
                    // No channel context here, so a CLI-added job targets the
                    // configured home channel and shares that channel's gray
                    // home — the same result `/cron add` gives in the home
                    // channel, so the two entry points cannot drift.
                    let home = config
                        .get("channel_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "channel_id missing".to_string())?;
                    let job_id = crate::channels::discord::durable::uuid_hex();
                    store.schedule_add(
                        &job_id,
                        *every,
                        prompt,
                        home,
                        &format!("chat:{home}"),
                        crate::channels::discord::durable::now_secs(),
                    )?;
                    println!("{job_id}");
                    Ok(())
                }
                ScheduleAction::List => {
                    for job in store.schedules()? {
                        println!(
                            "{} {} {} {}",
                            job.id,
                            job.interval,
                            job.status,
                            if job.channel.is_empty() {
                                "(home)"
                            } else {
                                job.channel.as_str()
                            }
                        );
                    }
                    Ok(())
                }
                ScheduleAction::Remove { id } => {
                    store.schedule_remove(id)?;
                    Ok(())
                }
            }
        }
        Command::Queue { action } => {
            let store = crate::channels::discord::gateway::open_store(config_path)?;
            match action {
                QueueAction::List => {
                    for item in store.items()? {
                        println!(
                            "{} {} {} {}",
                            item.0,
                            item.1,
                            item.2,
                            item.3.as_deref().unwrap_or("")
                        );
                    }
                    Ok(())
                }
                QueueAction::Cancel { id } => {
                    store.cancel(id)?;
                    Ok(())
                }
            }
        }
        Command::Pairing { action } => match action {
            PairingAction::Approve {
                platform,
                code,
                as_user,
            } => {
                let line = crate::channels::discord::pairing::approve(
                    config_path,
                    platform,
                    code,
                    as_user,
                )?;
                println!("{line}");
                Ok(())
            }
        },
        Command::Allowlist { action } => {
            let mut config = crate::channels::discord::config::load_config(config_path)?;
            let mut allowed: Vec<String> = config
                .get("allowed_users")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            match action {
                AllowlistAction::Add { id } => {
                    if !crate::channels::discord::config::snowflake(&Value::String(id.clone())) {
                        return Err("Invalid snowflake ID".to_string());
                    }
                    if !allowed.contains(id) {
                        allowed.push(id.clone());
                        config["allowed_users"] =
                            Value::Array(allowed.into_iter().map(Value::String).collect());
                        crate::channels::discord::config::save_config(config_path, &config)?;
                    }
                    Ok(())
                }
                AllowlistAction::Remove { id } => {
                    if !crate::channels::discord::config::snowflake(&Value::String(id.clone())) {
                        return Err("Invalid snowflake ID".to_string());
                    }
                    allowed.retain(|x| x != id);
                    config["allowed_users"] =
                        Value::Array(allowed.into_iter().map(Value::String).collect());
                    crate::channels::discord::config::save_config(config_path, &config)?;
                    Ok(())
                }
                AllowlistAction::List => {
                    for id in allowed {
                        println!("{id}");
                    }
                    Ok(())
                }
            }
        }
    }
}

/// `gray-gateway discord <args…>`: this channel's former CLI, parsed the way
/// the standalone binary parsed it.
pub fn run_discord(argv: &[String]) -> Result<(), String> {
    let cli = Cli::try_parse_from(
        std::iter::once("gray-discord".to_string()).chain(argv.iter().cloned()),
    )
    .map_err(|e| e.to_string())?;
    let path = cli.config_path();
    let path = std::path::absolute(&path).unwrap_or(path);
    run(&cli.command, &path)
}
