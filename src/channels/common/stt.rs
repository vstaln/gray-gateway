//! Inbound voice notes, transcribed before the model turn.
//!
//! The gateway never speaks to a speech provider itself: it spawns the
//! installed voice plugin as a sidecar and asks it for a transcript, so the
//! provider, the key and the /voice settings stay in one place. The
//! transcript is folded into the prompt and therefore lives in the
//! conversation's session; nothing is posted back to the chat.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const VOICE_TIMEOUT: Duration = Duration::from_secs(120);
const VOICE_LOOKUP: Duration = Duration::from_secs(10);

const AUDIO_EXT: &[&str] = &[
    "wav", "ogg", "oga", "opus", "mp3", "m4a", "aac", "flac", "webm", "amr", "silk",
];

pub fn is_audio(filename: &str, content_type: &str) -> bool {
    if content_type.to_ascii_lowercase().starts_with("audio/") {
        return true;
    }
    if content_type.eq_ignore_ascii_case("video/ogg") {
        return true;
    }
    Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| AUDIO_EXT.contains(&e.as_str()))
}

fn lock_binary(gray_home: &str) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(Path::new(gray_home).join("plugins/lock.json")).ok()?;
    let lock: Value = serde_json::from_str(&raw).ok()?;
    let entry = lock.pointer("/plugins/voice")?;
    for key in ["argv", "cli_argv"] {
        let first = entry.get(key).and_then(Value::as_array)?.first()?;
        let path = PathBuf::from(first.as_str()?);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn voice_binary(config: &Value) -> Option<PathBuf> {
    config
        .get("gray_home")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("GRAY_HOME").ok().filter(|s| !s.is_empty()))
        .and_then(|h| lock_binary(&h))
        .or_else(|| on_path("voice"))
}

async fn ask(config: &Value, method: &str, params: Value) -> Result<Value, String> {
    let bin = voice_binary(config).ok_or("the voice plugin is not installed")?;
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(home) = config.get("gray_home").and_then(Value::as_str) {
        if !home.is_empty() {
            cmd.env("GRAY_HOME", home);
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start the voice plugin: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "voice plugin has no stdin".to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "voice plugin has no stdout".to_string())?;
    let request = json!({"id": 1, "method": method, "params": params});
    {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(|e| format!("voice plugin write failed: {e}"))?;
        stdin.flush().await.ok();
    }
    let timeout = if method == "tool/call" {
        VOICE_TIMEOUT
    } else {
        VOICE_LOOKUP
    };
    let mut line = String::new();
    let read = tokio::time::timeout(timeout, async {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(&mut stdout);
        reader.read_line(&mut line).await
    })
    .await;
    drop(stdin);
    let _ = child.kill().await;
    let reply: Value = match read {
        Err(_) => return Err("the voice plugin did not answer".to_string()),
        Ok(Err(e)) => return Err(format!("voice plugin read failed: {e}")),
        Ok(Ok(0)) => return Err("the voice plugin closed without answering".to_string()),
        Ok(Ok(_)) => {
            serde_json::from_str(line.trim()).map_err(|_| "bad voice reply".to_string())?
        }
    };
    if let Some(err) = reply.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("voice plugin error");
        return Err(msg.to_string());
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

pub async fn transcribe(config: &Value, path: &Path) -> Result<String, String> {
    let result = ask(
        config,
        "tool/call",
        json!({"name": "voice_transcribe", "args": {"path": path.to_string_lossy()}}),
    )
    .await?;
    if result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(result
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("transcription failed")
            .to_string());
    }
    let text = result
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("the note was silent".to_string());
    }
    Ok(text)
}

pub async fn provider(config: &Value, id: Option<&str>) -> Result<String, String> {
    let mut argv = vec![json!("provider")];
    if let Some(id) = id.map(str::trim).filter(|i| !i.is_empty()) {
        argv.push(json!(id));
    }
    let result = ask(
        config,
        "command/run",
        json!({"name": "/voice", "argv": argv}),
    )
    .await?;
    let text = result
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("/voice provider gave no answer".to_string());
    }
    Ok(text)
}

/// Fold transcripts into the prompt ahead of the attachment list.
pub fn prompt_block(transcripts: &[String]) -> String {
    transcripts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if transcripts.len() == 1 {
                format!("[voice note transcript] {t}")
            } else {
                format!("[voice note {} transcript] {t}", i + 1)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
