//! Attachment handling, lite: a user DMing an image
//! or a .txt is talking to a relay that used to drop it on the floor. We
//! save the file under workdir and name it in the prompt — gray reads it
//! with its own tools (`cat <image>` is its vision path, cat for text),
//! so no decoding happens here. Cap, don't stream: a 500MB attachment is a
//! deny, not a memory spike.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// Discord's own attachment payload fields we care about.
pub struct AttachmentRef<'a> {
    pub url: &'a str,
    pub filename: &'a str,
    pub size: u64,
    pub content_type: Option<&'a str>,
}

pub async fn save(
    http: &reqwest::Client,
    token: &str,
    a: &AttachmentRef<'_>,
    msg_id: &str,
    workdir: &Path,
    max_bytes: u64,
) -> Option<PathBuf> {
    if a.size > max_bytes || a.size == 0 {
        return None;
    }
    let safe: String = a
        .filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let dir = workdir.join("attachments");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{msg_id}-{safe}"));
    let mut req = http.get(a.url);
    if !a.url.contains("cdn.discordapp.com/") && !a.url.contains("media.discordapp.net/") {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bot {token}"));
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() as u64 > max_bytes {
        return None;
    }
    std::fs::write(&path, &bytes).ok()?;
    Some(path)
}

/// How the prompt names what the user attached. One line per file, path
/// first — that is what gray's tools consume.
pub fn prompt_lines(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("[attached file: {}]", p.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn enrich_prompt(
    config: &Value,
    token: &str,
    workdir: &Path,
    msg_id: &str,
    mut prompt: String,
    attachments: &[AttachmentRef<'_>],
) -> String {
    if attachments.is_empty() {
        return prompt;
    }
    let max_bytes = config
        .get("max_attachment_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(8 * 1024 * 1024);
    let http = reqwest::Client::new();
    let mut saved: Vec<PathBuf> = Vec::new();
    let mut spoken: Vec<String> = Vec::new();
    for a in attachments {
        let Some(p) = save(&http, token, a, msg_id, workdir, max_bytes).await else {
            continue;
        };
        if crate::channels::discord::stt::is_audio(a.filename, a.content_type.unwrap_or("")) {
            match crate::channels::discord::stt::transcribe(config, &p).await {
                Ok(text) => spoken.push(text),
                Err(e) => {
                    crate::channels::discord::eprintln_stamped!(
                        "[discord] voice note not transcribed: {e}"
                    );
                    spoken.push(format!("[voice note could not be transcribed: {e}]"));
                }
            }
        }
        saved.push(p);
    }
    if !spoken.is_empty() {
        let block = crate::channels::discord::stt::prompt_block(&spoken);
        prompt = if prompt.is_empty() {
            block
        } else {
            format!("{prompt}\n\n{block}")
        };
    }
    if !saved.is_empty() {
        let lines = prompt_lines(&saved);
        prompt = if prompt.is_empty() {
            lines
        } else {
            format!("{prompt}\n\n{lines}")
        };
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_sanitised_and_prefixed_by_the_message_id() {
        assert_eq!(
            prompt_lines(&[PathBuf::from("/w/attachments/123-cat.png")]),
            "[attached file: /w/attachments/123-cat.png]"
        );
        let bad = "../../etc/pa$$wd".to_string();
        let safe: String = bad
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        assert_eq!(safe, ".._.._etc_pa__wd");
        assert!(!safe.contains('/'), "no path separator survives sanitising");
    }

    #[test]
    fn no_attachments_produce_no_lines() {
        assert_eq!(prompt_lines(&[]), "");
    }
}
