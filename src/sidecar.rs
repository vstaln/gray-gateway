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
            "pause", "resume", "inbox",
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
            let jobs = store.list().unwrap_or_default();
            let text = if jobs.is_empty() {
                "no cron jobs".to_string()
            } else {
                jobs.iter()
                    .map(|j| {
                        format!(
                            "{} {} next={}",
                            j.id,
                            crate::cron_tool::schedule_text(&j.schedule),
                            j.next_run_at.unwrap_or(0)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            json!({"text": text})
        }
        _ => json!({"text": format!("unknown command {name:?}")}),
    }
}

pub async fn run() -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let stdin = tokio::io::stdin();
    let mut lines = tokio::io::BufReader::new(stdin).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
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
                return Ok(());
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
    }
    Ok(())
}
