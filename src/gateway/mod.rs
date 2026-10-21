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

pub(crate) mod run;

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
    let home = crate::vendor::gray_home()?;
    let apps = crate::vendor::setup_apps(&home);
    let name = match platform {
        Some(p) => p,
        None => match apps.len() {
            0 => "discord".to_string(),
            1 => apps[0].name.clone(),
            _ => {
                for (i, a) in apps.iter().enumerate() {
                    println!("  {}. {}", i + 1, a.name);
                }
                match pick_platform(apps.len())? {
                    Pick::Choice(i) => apps[i - 1].name.clone(),
                    Pick::Cancel(code) => {
                        eprintln!("cancelled");
                        std::process::exit(code);
                    }
                }
            }
        },
    };
    let mut exe = apps
        .iter()
        .find(|a| a.name == name)
        .and_then(|a| a.exe.clone());
    if !apps.iter().any(|a| a.name == name) {
        let out = crate::vendor::gray_cli(&["plugin", "install", "--yes", &name])?;
        if !out.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
        }
        if !out.status.success() {
            anyhow::bail!("gray plugin install {name} failed ({})", out.status);
        }
        print!("{}", String::from_utf8_lossy(&out.stdout));
        // A sidecar install lands no `gray <name>` forwarding; resolve its
        // binary so setup still runs.
        exe = crate::vendor::setup_apps(&home)
            .into_iter()
            .find(|a| a.name == name)
            .and_then(|a| a.exe);
    }
    let status = match exe {
        // Same argv `gray <name> setup` would exec, minus the forwarding
        // check that rejects sidecar-only installs.
        Some(exe) => std::process::Command::new(exe)
            .arg("setup")
            .env("GRAY_BIN", crate::vendor::gray_bin())
            .status()?,
        None => std::process::Command::new(crate::vendor::gray_bin())
            .arg(&name)
            .arg("setup")
            .status()?,
    };
    std::process::exit(status.code().unwrap_or(1));
}

/// What the numbered platform prompt settled on.
enum Pick {
    /// 1-based index into the printed list.
    Choice(usize),
    /// The user bailed (esc / ctrl-c / end of input); the value is the exit
    /// code to leave with (130 for ctrl-c, matching SIGINT elsewhere).
    Cancel(i32),
}

/// `Platform [1]:` — a numbered prompt re-asked until a valid choice, esc,
/// ctrl-c/d or EOF. On a tty it reads keys in cbreak mode so esc cancels
/// outright and arrow keys don't echo `^[[B`; piped stdin takes one line
/// per try.
fn pick_platform(count: usize) -> anyhow::Result<Pick> {
    #[cfg(unix)]
    {
        // SAFETY: isatty only inspects the fd.
        if unsafe { libc::isatty(0) } == 1 {
            return pick_tty(count);
        }
    }
    pick_line(count)
}

/// Line-mode fallback (piped stdin, Windows, no tty): read a line, apply
/// the same rules. An escape byte in the line or EOF cancels.
fn pick_line(count: usize) -> anyhow::Result<Pick> {
    use std::io::Write;
    loop {
        print!("Platform [1]: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 || line.contains('\x1b') {
            return Ok(Pick::Cancel(1));
        }
        let pick = line.trim();
        if pick.is_empty() {
            return Ok(Pick::Choice(1));
        }
        if let Ok(i) = pick.parse::<usize>()
            && (1..=count).contains(&i)
        {
            return Ok(Pick::Choice(i));
        }
        println!("pick a number from 1 to {count}");
    }
}

/// One stdin byte in cbreak mode; None on EOF or a read error.
#[cfg(unix)]
fn read_key(fd: i32) -> Option<u8> {
    let mut b = 0u8;
    loop {
        // SAFETY: reads at most one byte into a live buffer.
        let n = unsafe { libc::read(fd, &mut b as *mut u8 as *mut libc::c_void, 1) };
        match n {
            1 => return Some(b),
            -1 if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) => continue,
            _ => return None,
        }
    }
}

/// Is a byte waiting on fd within `ms`? poll, so buffered reads never
/// confuse a held escape sequence for a bare esc.
#[cfg(unix)]
fn fd_ready(fd: i32, ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: polls one fd struct for `ms`.
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 && pfd.revents & libc::POLLIN != 0 }
}

/// After ESC: a following escape sequence (arrows, f-keys) is consumed and
/// ignored — returns true so the prompt continues. A bare esc (no byte
/// inside 150 ms) returns false so the caller cancels.
#[cfg(unix)]
fn swallow_escape(fd: i32) -> bool {
    if !fd_ready(fd, 150) {
        return false;
    }
    if read_key(fd) == Some(b'[') {
        // CSI: bytes until the final byte (0x40..=0x7e), capped.
        for _ in 0..8 {
            match read_key(fd) {
                Some(b) if (0x40..=0x7e).contains(&b) => break,
                Some(_) if fd_ready(fd, 20) => {}
                _ => break,
            }
        }
    }
    true
}

/// The cbreak prompt loop: digits edit the choice, Enter accepts (empty =
/// [1]), invalid entries re-ask, esc/ctrl-c/ctrl-d/EOF cancel.
#[cfg(unix)]
fn pick_tty(count: usize) -> anyhow::Result<Pick> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    let Some(_raw) = TermRaw::enter(fd) else {
        return pick_line(count);
    };
    let mut buf = String::new();
    let pick = 'ask: loop {
        print!("Platform [1]: ");
        std::io::stdout().flush()?;
        loop {
            let Some(b) = read_key(fd) else {
                break 'ask Pick::Cancel(1);
            };
            match b {
                0x03 => break 'ask Pick::Cancel(130),
                0x04 => break 'ask Pick::Cancel(1),
                0x1b => {
                    if !swallow_escape(fd) {
                        break 'ask Pick::Cancel(1);
                    }
                }
                b'\r' | b'\n' => {
                    println!();
                    if buf.is_empty() {
                        break 'ask Pick::Choice(1);
                    }
                    if let Ok(i) = buf.parse::<usize>()
                        && (1..=count).contains(&i)
                    {
                        break 'ask Pick::Choice(i);
                    }
                    println!("pick a number from 1 to {count}");
                    buf.clear();
                    break;
                }
                b'0'..=b'9' => {
                    buf.push(b as char);
                    print!("{}", b as char);
                    std::io::stdout().flush()?;
                }
                0x7f | 0x08 if buf.pop().is_some() => {
                    print!("\x08 \x08");
                    std::io::stdout().flush()?;
                }
                _ => {}
            }
        }
    };
    Ok(pick)
}

/// Cbreak stdin for the prompt: canonical mode, echo and signal chars off,
/// restored on drop. With ISIG off, ctrl-c arrives as a byte the prompt
/// can turn into a clean exit instead of a mid-read kill.
#[cfg(unix)]
struct TermRaw {
    fd: i32,
    saved: libc::termios,
}

#[cfg(unix)]
impl TermRaw {
    fn enter(fd: i32) -> Option<TermRaw> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(TermRaw { fd, saved })
    }
}

#[cfg(unix)]
impl Drop for TermRaw {
    fn drop(&mut self) {
        // SAFETY: restores the termios this guard saved.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
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
            println!("{}", tokio::task::spawn_blocking(|| toggle(true)).await?);
            Ok(())
        }
        GatewayCmd::Off => {
            println!("{}", tokio::task::spawn_blocking(|| toggle(false)).await?);
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
            let exe = crate::vendor::gray_bin_abs()?;
            println!("{}", service::install(&home, &exe, no_start, print)?);
            Ok(())
        }
        GatewayCmd::Uninstall => {
            println!("{}", service::uninstall()?);
            Ok(())
        }
        GatewayCmd::Cron(_) | GatewayCmd::Session(_) => unreachable!("handled by main"),
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
