#[cfg(unix)]
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PROTOCOL: u32 = 1;
pub const SUPPORTED_VERBS: [&str; 6] =
    ["identify", "status", "metrics", "logs_tail", "pull", "ack"];

pub const LOGS_TAIL_LINES: usize = 200;

const LOGS_TAIL_MAX_BYTES: u64 = 64 * 1024;

#[cfg(unix)]
const MAX_SOCK_PATH: usize = 100;
#[cfg(unix)]
const MAX_REQUEST_BYTES: usize = 64 * 1024;
#[cfg(unix)]
const IO_TIMEOUT: Duration = Duration::from_secs(3);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

pub fn sock_path(home: &Path) -> PathBuf {
    home.join("gateway.sock")
}

pub fn identify_payload(home: &Path) -> serde_json::Value {
    let me = std::process::id();
    let started_at = super::pid::read(home)
        .map(|r| r.started_at)
        .unwrap_or_else(crate::cron::now_secs);
    serde_json::json!({
        "protocol": PROTOCOL,
        "kind": "gray-gateway",
        "pid": me,
        "start_time": super::pid::proc_start_time(me),
        "gray_home": home.display().to_string(),
        "version": env!("CARGO_PKG_VERSION"),
        "supervisor": super::service::supervisor_kind(),
        "uptime_secs": crate::cron::now_secs().saturating_sub(started_at),
    })
}

pub fn status_payload(home: &Path, now: i64) -> serde_json::Value {
    let mut out = identify_payload(home);
    let state = super::state::read(home);
    out["gateway_state"] = serde_json::json!(
        state
            .as_ref()
            .map(|s| s.gateway_state.clone())
            .unwrap_or_else(|| super::state::STATE_RUNNING.to_string())
    );
    out["exit_reason"] = serde_json::json!(state.as_ref().and_then(|s| s.exit_reason.clone()));
    out["started_at"] = serde_json::json!(state.as_ref().map(|s| s.started_at));
    out["cron"] = cron_payload(home, now);
    out["answered_at"] = serde_json::json!(now);
    out["answering_pid"] = serde_json::json!(std::process::id());
    out
}

fn cron_payload(home: &Path, now: i64) -> serde_json::Value {
    match crate::cron::CronStore::open(home.join("cron")) {
        Ok(store) => {
            let health = store.health(now).ok();
            let jobs = store.list().map(|v| v.len()).unwrap_or(0);
            let last = health.as_ref().and_then(|h| h.last_tick.as_ref());
            serde_json::json!({
                "ticker_live": health.as_ref().is_some_and(|h| h.ticker_live(now)),
                "last_tick_at": last.map(|t| t.at),
                "last_tick_kind": last.map(|t| t.kind.clone()),
                "last_tick_secs_ago": last.map(|t| now.saturating_sub(t.at)),
                "overdue": health.as_ref().map(|h| h.overdue.len()).unwrap_or(0),
                "jobs": jobs,
            })
        }
        Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
    }
}

pub fn metrics_payload(home: &Path, now: i64) -> serde_json::Value {
    let mut out = identify_payload(home);
    out["cron"] = cron_payload(home, now);
    out["answered_at"] = serde_json::json!(now);
    out["answering_pid"] = serde_json::json!(std::process::id());
    out
}

pub fn tail_gray_log(home: &Path) -> (Vec<String>, usize) {
    let body = std::fs::read(home.join("logs").join("gray.log")).unwrap_or_default();
    let capped = body.len() as u64 > LOGS_TAIL_MAX_BYTES;
    let tail: &[u8] = if capped {
        &body[body.len() - LOGS_TAIL_MAX_BYTES as usize..]
    } else {
        &body
    };
    let text = String::from_utf8_lossy(tail);

    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if capped && !lines.is_empty() {
        lines.remove(0);
    }

    let total = std::str::from_utf8(&body)
        .map(|s| s.lines().count())
        .unwrap_or(lines.len());
    let kept = if total > LOGS_TAIL_LINES {
        let from = lines.len().saturating_sub(LOGS_TAIL_LINES);
        lines.split_off(from)
    } else {
        lines
    };
    (kept, total)
}

pub fn handle_request_line(home: &Path, raw: &[u8], now: i64) -> Vec<u8> {
    let parsed = serde_json::from_slice::<serde_json::Value>(raw);
    let request_id = parsed.as_ref().ok().and_then(|v| v.get("id").cloned());
    let mut response = match parsed {
        Ok(req) if req.is_object() => match req.get("verb").and_then(|v| v.as_str()) {
            Some("identify") => serde_json::json!({
                "ok": true, "protocol": PROTOCOL, "result": identify_payload(home),
            }),
            Some("status") => serde_json::json!({
                "ok": true, "protocol": PROTOCOL, "result": status_payload(home, now),
            }),
            Some("metrics") => serde_json::json!({
                "ok": true, "protocol": PROTOCOL, "result": metrics_payload(home, now),
            }),
            Some("logs_tail") => {
                let (lines, total) = tail_gray_log(home);
                serde_json::json!({
                    "ok": true, "protocol": PROTOCOL,
                    "result": { "lines": lines, "total_lines_available": total },
                })
            }
            Some("pull") => match req.get("platform").and_then(|v| v.as_str()) {
                Some(platform) => {
                    let limit = req
                        .get("limit")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(20)
                        .clamp(1, 100);
                    let dir = super::state_dir(home);
                    let intents = super::outbox::pull(&dir, platform, now, limit as usize);
                    serde_json::json!({
                        "ok": true, "protocol": PROTOCOL, "result": { "intents": intents },
                    })
                }
                None => serde_json::json!({
                    "ok": false, "protocol": PROTOCOL, "error": "pull needs a platform",
                }),
            },
            Some("ack") => {
                let ids: Vec<String> = req
                    .get("ids")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                let acked = super::outbox::ack(&super::state_dir(home), &ids);
                serde_json::json!({
                    "ok": true, "protocol": PROTOCOL, "result": { "acked": acked },
                })
            }
            other => serde_json::json!({
                "ok": false, "protocol": PROTOCOL,
                "error": format!("unknown verb: {other:?}"),
                "supported_verbs": SUPPORTED_VERBS,
            }),
        },
        Ok(_) => serde_json::json!({
            "ok": false, "protocol": PROTOCOL, "error": "request must be a JSON object",
        }),
        Err(e) => serde_json::json!({
            "ok": false, "protocol": PROTOCOL, "error": format!("bad JSON: {e}"),
        }),
    };
    if let Some(id) = request_id
        && let Some(map) = response.as_object_mut()
    {
        map.insert("id".to_string(), id);
    }
    let mut out = serde_json::to_vec(&response).unwrap_or_default();
    out.push(b'\n');
    out
}

#[cfg(unix)]
pub async fn serve(
    home: PathBuf,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let path = sock_path(&home);
    if path.as_os_str().len() > MAX_SOCK_PATH {
        anyhow::bail!(
            "control socket path is {} bytes (max {MAX_SOCK_PATH}): {}",
            path.as_os_str().len(),
            path.display()
        );
    }

    let _ = std::fs::remove_file(&path);

    #[cfg(unix)]
    fn bind_gateway_socket(path: &std::path::Path) -> std::io::Result<tokio::net::UnixListener> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let staging = parent.join(format!(
            ".gray-sock-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
        let staged_sock = staging.join("gateway.sock");
        let listener = match tokio::net::UnixListener::bind(&staged_sock) {
            Ok(l) => l,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };
        let _ = std::fs::set_permissions(&staged_sock, std::fs::Permissions::from_mode(0o600));
        let renamed = std::fs::rename(&staged_sock, path);
        let _ = std::fs::remove_dir_all(&staging);
        renamed?;
        Ok(listener)
    }

    let listener = match bind_gateway_socket(&path) {
        Ok(l) => l,
        Err(e) => {
            log::debug!("gateway: staged socket bind unavailable ({e}); using the umask dance");
            let previous = unsafe { libc::umask(0o077) };
            let listener = tokio::net::UnixListener::bind(&path);
            unsafe { libc::umask(previous) };
            listener?
        }
    };
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    log::info!("gateway: control socket listening at {}", path.display());
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                tokio::spawn(serve_connection(home.clone(), stream));
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    log::info!("gateway: control socket closed");
    Ok(())
}

#[cfg(unix)]
async fn serve_connection(home: PathBuf, stream: tokio::net::UnixStream) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let (rd, mut wr) = stream.into_split();
    let mut rd = tokio::io::BufReader::new(rd).take(MAX_REQUEST_BYTES as u64 + 1);
    let mut raw = Vec::new();
    let read = tokio::time::timeout(
        IO_TIMEOUT,
        tokio::io::AsyncBufReadExt::read_until(&mut rd, b'\n', &mut raw),
    )
    .await;
    if let Ok(Ok(n)) = read
        && n > 0
        && n <= MAX_REQUEST_BYTES
    {
        let response = handle_request_line(&home, raw.trim_ascii_end(), crate::cron::now_secs());
        let _ = wr.write_all(&response).await;
    }
    let _ = wr.shutdown().await;
}

pub fn query(home: &Path, verb: &str) -> Option<serde_json::Value> {
    query_within(home, verb, CLIENT_TIMEOUT)
}

pub fn query_within(home: &Path, verb: &str, timeout: Duration) -> Option<serde_json::Value> {
    #[cfg(not(unix))]
    {
        let _ = (home, verb, timeout);
        None
    }
    #[cfg(unix)]
    {
        unix_query_within(home, verb, timeout)
    }
}

#[cfg(unix)]
fn unix_query_within(home: &Path, verb: &str, timeout: Duration) -> Option<serde_json::Value> {
    use std::os::unix::fs::FileTypeExt;
    let path = sock_path(home);

    let is_socket = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_socket());
    if !is_socket {
        return None;
    }
    let mut sock = std::os::unix::net::UnixStream::connect(&path).ok()?;
    sock.set_read_timeout(Some(timeout)).ok()?;
    sock.set_write_timeout(Some(timeout)).ok()?;
    let request = serde_json::json!({"verb": verb, "id": 1, "protocol": PROTOCOL});
    sock.write_all(request.to_string().as_bytes()).ok()?;
    sock.write_all(b"\n").ok()?;
    let mut line = String::new();
    std::io::BufReader::new(sock).read_line(&mut line).ok()?;
    let answer: serde_json::Value = serde_json::from_str(&line).ok()?;
    if answer.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        answer.get("result").cloned()
    } else {
        None
    }
}
