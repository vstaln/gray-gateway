pub mod activity;
pub mod brain;
pub mod cli;
pub mod deliver;
pub mod event;
pub mod lifecycle;
pub mod outbox;
pub mod pid;
pub mod service;
pub mod sessions;
pub mod settings;
pub mod socket;
pub mod state;
pub mod turn;

mod run;

pub fn state_dir(home: &Path) -> std::path::PathBuf {
    home.join("gateway")
}

pub fn paused(dir: &Path) -> bool {
    dir.join("PAUSED").exists()
}

use std::path::Path;

fn ensure_gateway_on() -> anyhow::Result<()> {
    ensure_gateway_on_with(crate::vendor::gw_auto_enabled())
}

fn ensure_gateway_on_with(enabled: bool) -> anyhow::Result<()> {
    if enabled {
        return Ok(());
    }
    anyhow::bail!("gateway is off — `gray gateway on` to enable")
}

async fn setup_platform(platform: Option<String>) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let home = crate::vendor::gray_home()?;
    let name = match platform {
        Some(p) => p,
        None => {
            let apps = crate::vendor::setup_apps(&home);
            match apps.len() {
                0 => "discord".to_string(),
                1 => apps[0].clone(),
                _ => {
                    use std::io::Write;
                    for (i, a) in apps.iter().enumerate() {
                        println!("  {}. {a}", i + 1);
                    }
                    print!("Platform [1]: ");
                    std::io::stdout().flush()?;
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    let pick = line.trim();
                    let i = if pick.is_empty() {
                        1
                    } else {
                        pick.parse::<usize>()?
                    };
                    apps.get(i.wrapping_sub(1))
                        .context("no such platform")?
                        .clone()
                }
            }
        }
    };
    if !crate::vendor::setup_apps(&home).contains(&name) {
        let out = crate::vendor::gray_cli(&["plugin", "install", "--yes", &name])?;
        if !out.status.success() {
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            anyhow::bail!("gray plugin install {name} failed ({})", out.status);
        }
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
    let status = std::process::Command::new(crate::vendor::gray_bin())
        .arg(&name)
        .arg("setup")
        .status()?;
    std::process::exit(status.code().unwrap_or(1));
}

fn toggle(on: bool) -> String {
    match crate::vendor::set_flag("gw_auto", on) {
        Ok(()) if on => "✓ gateway on".to_string(),
        Ok(()) => {
            "✓ gateway off — run/start will refuse to start, status/stop still work".to_string()
        }
        Err(e) => format!("{e:#}"),
    }
}

pub async fn run_cli(cmd: crate::GatewayCmd) -> anyhow::Result<()> {
    use crate::GatewayCmd;

    if let GatewayCmd::Lifecycle(cmd) = cmd {
        return lifecycle_cli(cmd);
    }
    if let GatewayCmd::Agent(cmd) = cmd {
        return cli::run(cmd).await;
    }
    if let GatewayCmd::Setup { platform } = cmd {
        return setup_platform(platform).await;
    }

    anyhow::ensure!(
        !cfg!(windows),
        "gateway is not supported on native Windows; use WSL for background scheduling"
    );
    match cmd {
        GatewayCmd::Run => {
            ensure_gateway_on()?;
            run::run_foreground().await
        }
        GatewayCmd::Status => {
            let home = crate::vendor::gray_home()?;
            let (running, lines) = report(&home);
            for line in lines {
                println!("{line}");
            }
            if !running {
                std::process::exit(1);
            }
            Ok(())
        }
        GatewayCmd::Start => {
            ensure_gateway_on()?;
            println!("{}", service::start()?);
            Ok(())
        }
        GatewayCmd::On => {
            println!("{}", toggle(true));
            Ok(())
        }
        GatewayCmd::Off => {
            println!("{}", toggle(false));
            Ok(())
        }
        GatewayCmd::Stop => {
            println!("{}", service::stop()?);
            Ok(())
        }
        GatewayCmd::Restart => {
            println!("{}", service::restart()?);
            Ok(())
        }
        GatewayCmd::Install { no_start, print } => {
            let home = crate::vendor::gray_home()?;
            let exe = crate::vendor::gray_bin();
            println!("{}", service::install(&home, &exe, no_start, print)?);
            Ok(())
        }
        GatewayCmd::Uninstall => {
            println!("{}", service::uninstall()?);
            Ok(())
        }
        GatewayCmd::Cron(_) => unreachable!("handled by main"),
        GatewayCmd::Lifecycle(_) | GatewayCmd::Setup { .. } | GatewayCmd::Agent(_) => {
            unreachable!("handled above")
        }
    }
}

fn lifecycle_cli(cmd: crate::LifecycleCmd) -> anyhow::Result<()> {
    use crate::LifecycleCmd;
    let answer = match cmd {
        LifecycleCmd::Boot { dir, interrupted } => lifecycle::boot_json(&dir, interrupted),
        LifecycleCmd::Stop { dir } => lifecycle::stop_json(&dir),
        LifecycleCmd::Restart { dir } => {
            lifecycle::request_restart(&dir)?;
            serde_json::json!({"restart": true})
        }
        LifecycleCmd::ClearRestart { dir } => {
            lifecycle::clear_restart(&dir);
            serde_json::json!({"restart": false})
        }
    };
    println!("{answer}");
    Ok(())
}

fn report(home: &Path) -> (bool, Vec<String>) {
    let now = crate::cron::now_secs();
    let sup = service::detect();
    let mut lines = Vec::new();
    let mut running = false;

    if let Some(result) = socket::query(home, "status") {
        running = true;
        let pid = result.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
        let uptime = result
            .get("uptime_secs")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        lines.push(format!(
            "gateway: running (pid {pid}, up {})",
            crate::cron_status::ago_short(uptime)
        ));
        lines.push(format!(
            "  socket: {} — answered status (protocol {})",
            socket::sock_path(home).display(),
            socket::PROTOCOL
        ));
        if let Some(kind) = result.get("supervisor").and_then(|v| v.as_str()) {
            lines.push(format!("  supervisor: {kind}"));
        }
    } else if let Some(rec) = pid::running(home) {
        running = true;
        lines.push(format!(
            "gateway: running (pid {}, up {}) — pid file only, no socket answer",
            rec.pid,
            crate::cron_status::ago_short(now.saturating_sub(rec.started_at))
        ));
    } else {
        lines.push("gateway: not running".to_string());
        let sock = socket::sock_path(home);
        if sock.exists() {
            lines.push(format!("  socket: stale file at {}", sock.display()));
        }
        if let Some(st) = state::read(home) {
            lines.push(format!(
                "  last state: {} ({}) — {} ago",
                st.gateway_state,
                st.exit_reason.as_deref().unwrap_or("no reason recorded"),
                crate::cron_status::ago_short(now.saturating_sub(st.updated_at))
            ));
        }
    }

    match crate::cron::CronStore::open(home.join("cron")) {
        Ok(store) => {
            let health = store.health(now).unwrap_or_default();
            let jobs = store.list().map(|v| v.len()).unwrap_or(0);
            lines.push(format!("  jobs: {jobs}"));
            for l in crate::cron_status::ticker_line(&health, now).lines() {
                lines.push(format!("  cron: {l}"));
            }
        }
        Err(e) => lines.push(format!("  cron: store unavailable: {e:#}")),
    }
    let svc = service::status(&sup);
    lines.push(format!(
        "  service: {}",
        service::describe_status(&sup, &svc)
    ));
    if !running && svc.up {
        lines.push(
            "  note: the service reports a live pid but nothing answers this home's socket — different GRAY_HOME, or the gateway is crash-looping (`gray gateway run` shows why)"
                .to_string(),
        );
    }
    (running, lines)
}
