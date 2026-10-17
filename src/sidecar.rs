use serde_json::{Value, json};

pub fn manifest() -> Value {
    json!({
        "name": "gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.3",
        "tools": [crate::cron_tool::tool_def()],
        "commands": ["/cron"],
        "completion": [
            "setup", "run", "status", "start", "stop", "restart", "install",
            "uninstall", "on", "off", "lifecycle", "send", "wake", "activity",
            "pause", "resume", "inbox", "cron",
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
            let now = crate::cron::now_secs();
            let health = store.health(now).ok();
            let query = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let query = query.trim();
            let jobs = store.list().unwrap_or_default();
            let text = if query.is_empty() {
                crate::cron_status::format_cron_dashboard(&jobs, health.as_ref(), now)
            } else {
                match jobs
                    .into_iter()
                    .find(|j| j.id.starts_with(query) || j.name == query)
                {
                    Some(j) => {
                        crate::cron_status::format_cron_dashboard(&[j], health.as_ref(), now)
                    }
                    None => format!("unknown cron job {query:?}"),
                }
            };
            json!({"text": text})
        }
        _ => json!({"text": format!("unknown command {name:?}")}),
    }
}

const MAX_LINE: usize = 1 << 20;

pub async fn run() -> anyhow::Result<()> {
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
