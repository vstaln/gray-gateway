use serde_json::{Value, json};

pub fn manifest() -> Value {
    json!({
        "name": "gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.3",
        "tools": [crate::cron_tool::tool_def()],
        "commands": ["/cron", "/gateway"],
        "turn_local": {"/gateway": ["*"], "/cron": ["*"]},
        // spawned at session start so the cron ticker runs while a REPL is
        // open — no `gray-gateway run` daemon needed for reminders/wakes
        "autostart": true,
        "completion": [
            "setup", "run", "status", "start", "stop", "restart", "install",
            "uninstall", "on", "off", "lifecycle", "send", "wake", "activity",
            "pause", "resume", "inbox", "cron", "session",
        ],
    })
}

fn tool_call(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("args").cloned().unwrap_or_else(|| json!({}));
    let session_id = params
        .get("session")
        .and_then(|s| s.get("id"))
        .and_then(Value::as_str);
    if name != "schedule" {
        return json!({"content": format!("unknown tool {name:?}"), "is_error": true});
    }
    let home = match crate::vendor::gray_home() {
        Ok(h) => h,
        Err(e) => return json!({"content": format!("{e:#}"), "is_error": true}),
    };
    match (crate::cron_tool::ScheduleTool { home }).apply(session_id, &args) {
        Ok(text) => json!({"content": text}),
        Err(e) => json!({"content": format!("{e:#}"), "is_error": true}),
    }
}

fn command_run(params: &Value) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start_matches('/');
    match name {
        "cron" => {
            let home = match crate::vendor::gray_home() {
                Ok(h) => h,
                Err(e) => return json!({"text": format!("{e:#}")}),
            };
            let store = match crate::cron::CronStore::open(home.join("cron")) {
                Ok(s) => s,
                Err(e) => return json!({"text": format!("cron store unavailable: {e:#}")}),
            };
            let argv: Vec<String> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            json!({"text": cron_command(&store, &argv)})
        }
        "gateway" => {
            let argv: Vec<String> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            gateway_command(&argv)
        }
        _ => json!({"text": format!("unknown command {name:?}")}),
    }
}

/// `/gateway on|off` flips the master switch (`gw_auto` in config.json —
/// the same flag `gray gateway on` writes); bare/status renders the
/// supervisor report the CLI prints for `gray gateway status`.
fn gateway_command(argv: &[String]) -> Value {
    match argv.first().map(String::as_str) {
        Some("on") => json!({"text": crate::gateway::toggle(true)}),
        Some("off") => json!({"text": crate::gateway::toggle(false)}),
        _ => match crate::vendor::gray_home() {
            Ok(home) => {
                let (_, lines) = crate::gateway::report(&home);
                let mut text = lines.join("\n");
                if !crate::vendor::gw_auto_enabled() {
                    text.push_str("\n(master switch off — /gateway on to enable)");
                }
                json!({"text": text})
            }
            Err(e) => json!({"text": format!("{e:#}")}),
        },
    }
}

fn cron_command(store: &crate::cron::CronStore, argv: &[String]) -> String {
    let mut it = argv.iter().map(|s| s.as_str());
    let verb = it.next().unwrap_or("list");
    match verb {
        "pause" | "resume" | "remove" => {
            let Some(id) = it.next() else {
                return format!("usage: /cron {verb} <job>");
            };
            let result = match verb {
                "pause" => store
                    .set_paused(id, true)
                    .map(|known| known.then(|| format!("paused {id}"))),
                "resume" => store.set_paused(id, false).map(|known| {
                    known.then(|| {
                        let next = store
                            .get(id)
                            .ok()
                            .flatten()
                            .map(|j| crate::cron_cli::fmt_ts_opt(j.next_run_at))
                            .unwrap_or_else(|| "-".to_string());
                        format!("resumed {id} next {next}")
                    })
                }),
                _ => store
                    .remove(id)
                    .map(|known| known.then(|| format!("removed {id}"))),
            };
            match result {
                Ok(Some(text)) => text,
                Ok(None) => format!("unknown cron job {id:?}"),
                Err(e) => format!("cron {verb} failed: {e:#}"),
            }
        }
        "list" => cron_listing(store, it.next()),
        query => cron_listing(store, Some(query)),
    }
}

fn cron_listing(store: &crate::cron::CronStore, query: Option<&str>) -> String {
    let now = crate::cron::now_secs();
    let health = store.health(now).ok();
    let jobs = store.list().unwrap_or_default();
    match query {
        None => crate::cron_status::format_cron_dashboard(&jobs, health.as_ref(), now),
        Some(q) => match jobs
            .into_iter()
            .find(|j| j.id.starts_with(q) || j.name == q)
        {
            Some(j) => crate::cron_status::format_cron_dashboard(&[j], health.as_ref(), now),
            None => format!("unknown cron job {q:?}"),
        },
    }
}

const MAX_LINE: usize = 1 << 20;

/// The host spawns one sidecar per session and kills it on exit — a ticking
/// sidecar means cron jobs fire whenever a REPL is open, no daemon needed.
/// Concurrent tickers (the `gray-gateway run` daemon, other sessions) are
/// fine: `claim_due` hands each due job to exactly one owner.
struct CronTicker {
    stop: tokio::sync::watch::Sender<bool>,
}

impl CronTicker {
    fn start() -> Option<Self> {
        let home = crate::vendor::gray_home()
            .map_err(|e| log::warn!("gateway: no gray home, sidecar cron disabled: {e:#}"))
            .ok()?;
        crate::gateway::run::spawn_ticker(home, std::time::Duration::from_secs(60), "sidecar")
            .map(|(stop, _handle)| Self { stop })
            .map_err(|e| log::warn!("gateway: sidecar cron ticker failed to spawn: {e:#}"))
            .ok()
    }
}

impl Drop for CronTicker {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

pub async fn run() -> anyhow::Result<()> {
    let _cron_ticker = CronTicker::start();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let stdin = tokio::io::stdin();
    let mut reader = tokio::io::BufReader::new(stdin);
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut skipping = false;
    let mut out = tokio::io::stdout();
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            if !skipping && !pending.is_empty() {
                let line = String::from_utf8_lossy(&pending).trim().to_string();
                if !line.is_empty() && !dispatch(&line, &mut out).await? {
                    return Ok(());
                }
            }
            break;
        }
        let mut rest = &buf[..n];
        loop {
            if skipping {
                match rest.iter().position(|b| *b == b'\n') {
                    Some(i) => {
                        skipping = false;
                        rest = &rest[i + 1..];
                    }
                    None => break,
                }
                continue;
            }
            match rest.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    if pending.len() + i > MAX_LINE {
                        pending.clear();
                        rest = &rest[i + 1..];
                        let frame = json!({"id": null, "error": {"code": -32600, "message": "request line exceeds 1 MiB; dropped"}});
                        out.write_all(frame.to_string().as_bytes()).await?;
                        out.write_all(b"\n").await?;
                        out.flush().await?;
                        continue;
                    }
                    pending.extend_from_slice(&rest[..i]);
                    rest = &rest[i + 1..];
                    let line = String::from_utf8_lossy(&pending).trim().to_string();
                    pending.clear();
                    if line.is_empty() {
                        continue;
                    }
                    if !dispatch(&line, &mut out).await? {
                        return Ok(());
                    }
                }
                None => {
                    pending.extend_from_slice(rest);
                    if pending.len() > MAX_LINE {
                        pending.clear();
                        skipping = true;
                        let frame = json!({"id": null, "error": {"code": -32600, "message": "request line exceeds 1 MiB; dropped"}});
                        out.write_all(frame.to_string().as_bytes()).await?;
                        out.write_all(b"\n").await?;
                        out.flush().await?;
                    }
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn dispatch(line: &str, out: &mut tokio::io::Stdout) -> anyhow::Result<bool> {
    use tokio::io::AsyncWriteExt;
    let Ok(req) = serde_json::from_str::<Value>(line) else {
        return Ok(true);
    };
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let reply = match (method, id) {
        ("plugin/shutdown", _) => {
            if let Some(id) = req.get("id") {
                let frame = json!({"id": id, "result": {}});
                out.write_all(frame.to_string().as_bytes()).await?;
                out.write_all(b"\n").await?;
                out.flush().await?;
            }
            return Ok(false);
        }
        ("plugin/manifest", Some(id)) => Some(json!({"id": id, "result": manifest()})),
        ("plugin/tools", Some(id)) => {
            Some(json!({"id": id, "result": {"tools": [crate::cron_tool::tool_def()]}}))
        }
        ("tool/call", Some(id)) => Some(json!({"id": id, "result": tool_call(&params)})),
        ("command/run", Some(id)) => Some(json!({"id": id, "result": command_run(&params)})),
        (_, None) => None,
        (_, Some(id)) => {
            Some(json!({"id": id, "error": {"code": -32601, "message": "method not found"}}))
        }
    };
    if let Some(frame) = reply {
        out.write_all(frame.to_string().as_bytes()).await?;
        out.write_all(b"\n").await?;
        out.flush().await?;
    }
    Ok(true)
}

#[path = "sidecar_tests.rs"]
#[cfg(test)]
mod tests;
