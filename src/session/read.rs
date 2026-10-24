//! `gray read`: list live sessions or render a session's recent transcript.
//! Read-only port of core's `session_read.rs`; parses `<id>.jsonl` as plain
//! JSON (header line + one entry per line) instead of core's typed store.

use std::path::Path;
use std::time::SystemTime;

use serde_json::{Value, json};

use super::store;

const PREVIEW_CHARS: usize = 160;
const SUPPORTED_SESSION_VERSION: u64 = 1;

fn usage(msg: String) -> ! {
    eprintln!("gray read: {msg}");
    std::process::exit(2)
}

pub fn run(session: Option<&str>, live: bool, last: usize, json: bool) -> anyhow::Result<()> {
    let root = super::owner_home()?.join("sessions");
    match (session, live) {
        (Some(id), false) => read_session(&root, id, last, json),
        (None, _) | (_, true) => list_live(&root, json),
    }
}

struct Live {
    id: String,
    pid: Option<u32>,
    cwd: String,
    model: String,
    last_activity: Option<SystemTime>,
}

fn list_live(root: &Path, json: bool) -> anyhow::Result<()> {
    let mut rows: Vec<Live> = store::live_sessions(root)?
        .into_iter()
        .map(|(id, pid)| {
            let transcript = store::transcript_path(root, &id);
            let header = read_header(&transcript);
            Live {
                cwd: header["cwd"].as_str().unwrap_or("").to_string(),
                model: header["model"].as_str().unwrap_or("").to_string(),
                last_activity: std::fs::metadata(&transcript)
                    .and_then(|m| m.modified())
                    .ok(),
                id,
                pid,
            }
        })
        .collect();
    rows.sort_by_key(|a| std::cmp::Reverse(a.last_activity));
    if json {
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "pid": r.pid,
                    "cwd": r.cwd,
                    "model": r.model,
                    "last_activity": r.last_activity.map(unix_secs),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no live sessions in {}", root.display());
        return Ok(());
    }
    let width = rows.iter().map(|r| r.id.len()).max().unwrap_or(2).max(2);
    println!(
        "{:<width$}  {:>7}  {:<20}  {:<16}  CWD",
        "ID", "PID", "MODEL", "LAST ACTIVITY"
    );
    for r in &rows {
        println!(
            "{:<width$}  {:>7}  {:<20}  {:<16}  {}",
            r.id,
            r.pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string()),
            r.model,
            r.last_activity.map(ago).unwrap_or_else(|| "-".to_string()),
            r.cwd,
        );
    }
    Ok(())
}

struct Meta {
    cwd: String,
    model: String,
}

struct Entry {
    timestamp: u64,
    role: String,
    content: Vec<Value>,
}

/// Load header + active entries (after the last compaction boundary). Same
/// acceptance rules as core's `JsonlSessionStore::load` for a viewer: the
/// header must parse with the supported version and matching id; a torn final
/// line is ignored; any other unparsable line is an error. Unlike core this
/// never repairs or quarantines the file — a reader must not write.
fn load(path: &Path, id: &str) -> anyhow::Result<(Meta, Vec<Entry>)> {
    let bytes =
        std::fs::read(path).map_err(|e| anyhow::anyhow!("session not found: {id} ({e})"))?;
    let content = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            let valid = error.utf8_error().valid_up_to();
            anyhow::ensure!(
                !error.as_bytes()[valid..].contains(&b'\n'),
                "session file contains invalid UTF-8 before the final record"
            );
            String::from_utf8_lossy(&error.as_bytes()[..valid]).into_owned()
        }
    };
    let lines: Vec<(usize, &str)> = content
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l))
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();
    let corrupt = |line: usize, why: String| {
        anyhow::anyhow!("corrupt session file {}:{line}: {why}", path.display())
    };
    let (hline, hstr) = lines.first().copied().unwrap_or((1, ""));
    let header: Value = serde_json::from_str(hstr).map_err(|e| corrupt(hline, e.to_string()))?;
    let version = header["version"].as_u64();
    if version != Some(SUPPORTED_SESSION_VERSION) {
        return Err(corrupt(
            hline,
            format!(
                "unsupported session version {} (expected {SUPPORTED_SESSION_VERSION})",
                header["version"]
            ),
        ));
    }
    let hid = header["id"].as_str().unwrap_or("");
    if hid != id {
        return Err(corrupt(
            hline,
            format!("session id mismatch: filename '{id}' != header id '{hid}'"),
        ));
    }
    let meta = Meta {
        cwd: header["cwd"].as_str().unwrap_or("").to_string(),
        model: header["model"].as_str().unwrap_or("").to_string(),
    };
    let body = &lines[1.min(lines.len())..];
    let mut entries = Vec::with_capacity(body.len());
    for (idx, (n, line)) in body.iter().enumerate() {
        match parse_entry(line) {
            // core drops everything up to and including the last boundary
            Ok((true, _)) => entries.clear(),
            Ok((false, e)) => entries.push(e),
            Err(_) if idx == body.len() - 1 => {}
            Err(why) => return Err(corrupt(*n, why)),
        }
    }
    Ok((meta, entries))
}

fn parse_entry(line: &str) -> Result<(bool, Entry), String> {
    let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let boundary = v["compaction_boundary"].as_bool().unwrap_or(false);
    let timestamp = v["timestamp"]
        .as_u64()
        .ok_or_else(|| "missing field `timestamp`".to_string())?;
    let msg = &v["message"];
    let role = msg["role"]
        .as_str()
        .ok_or_else(|| "missing field `role`".to_string())?
        .to_string();
    let content = msg["content"]
        .as_array()
        .ok_or_else(|| "missing field `content`".to_string())?
        .clone();
    let injected = msg["injected"].as_bool().unwrap_or(false);
    Ok((
        boundary,
        Entry {
            timestamp,
            // injected/system turns never render; mark them by role
            role: if injected { String::new() } else { role },
            content,
        },
    ))
}

fn read_session(root: &Path, id: &str, last: usize, json: bool) -> anyhow::Result<()> {
    let id = match store::resolve_unique_prefix(root, id) {
        Ok(id) => id,
        Err(msg) => usage(msg),
    };
    let live = store::open_lock_held(root, &id);
    let pid = live.then(|| store::open_holder_pid(root, &id)).flatten();
    let (meta, entries) = load(&store::transcript_path(root, &id), &id)?;
    let messages: Vec<(u64, &str, Vec<String>)> = entries
        .iter()
        .filter(|e| !e.role.is_empty() && e.role != "system")
        .filter_map(|e| {
            let lines = render_blocks(&e.content);
            (!lines.is_empty()).then_some((e.timestamp, e.role.as_str(), lines))
        })
        .collect();
    let start = messages.len().saturating_sub(last.max(1));
    let shown = &messages[start..];
    if json {
        let out = json!({
            "id": id,
            "live": live,
            "pid": pid,
            "cwd": meta.cwd,
            "model": meta.model,
            "messages": shown.iter().map(|(ts, role, lines)| json!({
                "at": ts / 1000,
                "role": role,
                "lines": lines,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let state = match (live, pid) {
        (true, Some(pid)) => format!("live (pid {pid})"),
        (true, None) => "live".to_string(),
        (false, _) => "not live".to_string(),
    };
    println!("session {id} · {state} · {} · {}", meta.model, meta.cwd);
    if start > 0 {
        println!("({start} earlier message(s) not shown; --last N for more)");
    }
    for (ts, role, lines) in shown {
        println!();
        println!("[{}] {role}:", clock(*ts));
        for line in lines {
            println!("{line}");
        }
    }
    Ok(())
}

fn render_blocks(blocks: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for block in blocks {
        let s = |k: &str| block[k].as_str().unwrap_or("");
        match s("type") {
            "text" if !s("text").trim().is_empty() => {
                out.push(s("text").trim_end().to_string());
            }
            "tool_use" => {
                let name = s("name");
                let detail = tool_detail(&block["args"]);
                if detail.is_empty() {
                    out.push(format!("→ {name}"));
                } else {
                    out.push(format!("→ {name}: {detail}"));
                }
            }
            "tool_result" => {
                let first = s("content")
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                let is_error = block["is_error"].as_bool().unwrap_or(false);
                let mark = if is_error { "✗" } else { "⤷" };
                out.push(format!("  {mark} {}", preview(first)));
            }
            "image" => out.push("[image]".to_string()),
            "media" | "video" => out.push(format!("[{}]", s("media_type"))),
            _ => {}
        }
    }
    out
}

fn tool_detail(args: &Value) -> String {
    let pick = [
        "command",
        "path",
        "file_path",
        "pattern",
        "query",
        "url",
        "prompt",
    ]
    .iter()
    .find_map(|k| args.get(*k).and_then(Value::as_str))
    .map(str::to_string)
    .or_else(|| {
        args.as_object()
            .and_then(|m| m.values().find_map(Value::as_str))
            .map(str::to_string)
    })
    .unwrap_or_default();
    preview(&pick)
}

fn preview(text: &str) -> String {
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > PREVIEW_CHARS {
        format!("{}…", one.chars().take(PREVIEW_CHARS).collect::<String>())
    } else {
        one
    }
}

fn read_header(path: &Path) -> Value {
    use std::io::BufRead;
    std::fs::File::open(path)
        .ok()
        .and_then(|f| std::io::BufReader::new(f).lines().next()?.ok())
        .and_then(|line| serde_json::from_str(&line).ok())
        .unwrap_or(Value::Null)
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn clock(ms: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|t| t.format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}
