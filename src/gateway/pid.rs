use std::io::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PidRecord {
    pub kind: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<u64>,
    #[serde(default)]
    pub argv: Vec<String>,
    pub home: String,
    pub version: String,
    pub started_at: i64,
}

pub fn record_path(home: &Path) -> PathBuf {
    home.join("gateway.pid")
}

#[cfg(unix)]
pub fn proc_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;

    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(any(unix, windows)))]
pub fn proc_start_time(_pid: u32) -> Option<u64> {
    None
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }

    unsafe {
        libc::kill(pid as libc::pid_t, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(any(unix, windows)))]
pub fn pid_alive(pid: u32) -> bool {
    pid != 0 && pid == std::process::id()
}

#[cfg(windows)]
fn windows_process(pid: u32) -> Option<(bool, Option<u64>)> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_ACCESS_DENIED, FILETIME, GetLastError},
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    if pid == 0 {
        return None;
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return (GetLastError() == ERROR_ACCESS_DENIED).then_some((true, None));
        }

        let mut code = 0;
        let queried = GetExitCodeProcess(handle, &mut code) != 0;
        let alive = queried && code == 259;
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let start = (GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)
            != 0)
            .then_some(
                (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
            );
        CloseHandle(handle);
        Some((alive, start))
    }
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    windows_process(pid).is_some_and(|(alive, _)| alive)
}

#[cfg(windows)]
pub fn proc_start_time(pid: u32) -> Option<u64> {
    windows_process(pid).and_then(|(_, start)| start)
}

pub fn alive(rec: &PidRecord) -> bool {
    if !pid_alive(rec.pid) {
        return false;
    }
    match (rec.start_time, proc_start_time(rec.pid)) {
        (Some(claimed), Some(now)) => claimed == now,

        _ => true,
    }
}

pub fn read(home: &Path) -> Option<PidRecord> {
    let body = std::fs::read_to_string(record_path(home)).ok()?;
    serde_json::from_str(&body).ok()
}

pub fn running(home: &Path) -> Option<PidRecord> {
    read(home).filter(alive)
}

const CLAIM_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn hold_claim_lock(home: &Path) -> Option<std::fs::File> {
    let path = record_path(home).with_extension("pid.lock");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(&path).ok()?;
    let deadline = std::time::Instant::now() + CLAIM_LOCK_TIMEOUT;
    loop {
        match f.try_lock() {
            Ok(()) => return Some(f),
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    log::warn!(
                        "another starter is claiming {}; proceeding unlocked",
                        home.display()
                    );
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                log::warn!(
                    "gateway claim locking unsupported on {} ({e}); proceeding unlocked",
                    path.display()
                );
                return None;
            }
        }
    }
}

pub fn claim(home: &Path) -> anyhow::Result<PidRecord> {
    std::fs::create_dir_all(home)?;
    let me = std::process::id();
    let rec = PidRecord {
        kind: "gray-gateway".to_string(),
        pid: me,
        start_time: proc_start_time(me),
        argv: std::env::args().collect(),
        home: home.display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: crate::cron::now_secs(),
    };

    let _guard = hold_claim_lock(home);
    for _ in 0..2 {
        match write_new(home, &rec) {
            Ok(()) => return Ok(rec),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(old) = read(home).filter(alive) {
                    anyhow::bail!(
                        "another gateway is already running (pid {}, home {})",
                        old.pid,
                        old.home
                    );
                }

                let _ = std::fs::remove_file(record_path(home));
            }
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!(
        "gateway pid file is contested by another starter: {}",
        record_path(home).display()
    )
}

pub fn remove_owned(home: &Path, pid: u32) {
    if read(home).is_some_and(|rec| rec.pid == pid) {
        let _ = std::fs::remove_file(record_path(home));
    }
}

fn write_new(home: &Path, rec: &PidRecord) -> std::io::Result<()> {
    let path = record_path(home);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let body = serde_json::to_string_pretty(rec).map_err(std::io::Error::other)?;
    f.write_all(body.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    Ok(())
}
