//! `MEDIA:<path>` delivery for ordinary replies.
//!
//! The agent writes `MEDIA:/abs/path/chart.png` anywhere in its answer; the
//! gateway strips the tag, uploads the file, and posts it as a Components V2
//! message: images and videos in one media gallery, everything else as file
//! cards. Tags that cannot be delivered (missing file, blocked path, unknown
//! location) are left in the text so nothing silently disappears.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Discord's attachment cap per message.
pub const MAX_UPLOADS_PER_MESSAGE: usize = 10;
/// Upper bound per file; servers without boosts reject earlier and the
/// transport reports that as a 413.
pub const MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Upload {
    pub name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

impl Upload {
    pub fn is_visual(&self) -> bool {
        self.media_type.starts_with("image/") || self.media_type.starts_with("video/")
    }
}

/// Optional `media_roots` config: when set, files may only come from those
/// directories. Unset (the default) allows any non-secret file, matching the reference bot.
pub fn roots_from_config(config: &Value) -> Vec<PathBuf> {
    config
        .get("media_roots")
        .and_then(Value::as_array)
        .map(|roots| {
            roots
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|root| resolve(root, Path::new("/")))
                .filter_map(|root| root.canonicalize().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Media(PathBuf),
}

pub fn extract_parts(text: &str, cwd: &Path, roots: &[PathBuf]) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("MEDIA:") {
        let (before, after) = rest.split_at(at);
        let body = &after["MEDIA:".len()..];
        let trimmed = body.trim_start_matches([' ', '\t']);
        let end = trimmed
            .find(|c: char| c.is_whitespace() || c == '`' || c == '"' || c == '\'')
            .unwrap_or(trimmed.len());
        let end = trimmed[..end].find("MEDIA:").unwrap_or(end);
        let raw = trimmed[..end]
            .trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '*', '_', '!', '?']);
        let consumed = body.len() - trimmed.len() + end;
        match resolve(raw, cwd).and_then(|path| deliverable(&path, roots).ok()) {
            Some(path) => {
                // Drop markdown emphasis or code ticks wrapped around the tag.
                out.push_str(before.trim_end_matches(['*', '_', '`']));
                if !parts.contains(&Part::Media(path.clone())) {
                    parts.push(Part::Text(std::mem::take(&mut out)));
                    parts.push(Part::Media(path));
                }
                rest = body[consumed..].trim_start_matches(['*', '_', '`']);
            }
            None => {
                out.push_str(before);
                out.push_str("MEDIA:");
                rest = body;
            }
        }
    }
    out.push_str(rest);
    parts.push(Part::Text(out));
    parts
        .into_iter()
        .filter_map(|part| match part {
            Part::Text(text) => {
                let text = tidy(&text);
                (!text.is_empty()).then_some(Part::Text(text))
            }
            media => Some(media),
        })
        .collect()
}

pub fn extract(text: &str, cwd: &Path, roots: &[PathBuf]) -> (String, Vec<PathBuf>) {
    let parts = extract_parts(text, cwd, roots);
    (prose(&parts), media_paths(&parts))
}

pub fn prose(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            Part::Text(text) => Some(text.as_str()),
            Part::Media(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn media_paths(parts: &[Part]) -> Vec<PathBuf> {
    parts
        .iter()
        .filter_map(|part| match part {
            Part::Media(path) => Some(path.clone()),
            Part::Text(_) => None,
        })
        .collect()
}

/// Hide `MEDIA:` tags from a reply that is still streaming (the bridge's
/// `strip_media_directives_for_display`): the files go out after the turn,
/// and a half-typed path flickering in the preview is noise. A line that held
/// only a tag disappears with it.
pub fn strip_for_display(text: &str) -> String {
    if !text.contains("MEDIA:") {
        return text.to_string();
    }
    let mut lines = Vec::new();
    for line in text.split('\n') {
        if !line.contains("MEDIA:") {
            lines.push(line.to_string());
            continue;
        }
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(at) = rest.find("MEDIA:") {
            out.push_str(rest[..at].trim_end_matches(['*', '_', '`']));
            let body = rest[at + "MEDIA:".len()..].trim_start_matches([' ', '\t']);
            let end = body
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .unwrap_or(body.len());
            rest = body[end..].trim_start_matches(['*', '_', '`']);
            if out.ends_with(' ') {
                rest = rest.trim_start_matches(' ');
            }
        }
        out.push_str(rest);
        if !out.trim().is_empty() {
            lines.push(out.trim_end().to_string());
        }
    }
    lines.join("\n")
}

pub fn resolve(raw: &str, cwd: &Path) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    let raw = raw.strip_prefix("file://").unwrap_or(raw);
    let path = if let Some(home_relative) = raw.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(home_relative)
    } else {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    };
    Some(path)
}

/// A path may be delivered when it is a regular, bounded file that is not a
/// credential or configuration secret, inside `roots` when any are set.
pub fn deliverable(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|_| format!("{} does not exist", path.display()))?;
    let metadata = std::fs::metadata(&canonical).map_err(|_| "file cannot be read".to_string())?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", canonical.display()));
    }
    if metadata.len() == 0 {
        return Err(format!("{} is empty", canonical.display()));
    }
    if metadata.len() > MAX_UPLOAD_BYTES {
        return Err(format!("{} is larger than 25 MiB", canonical.display()));
    }
    if !roots.is_empty() && !roots.iter().any(|root| canonical.starts_with(root)) {
        let allowed: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        return Err(format!(
            "{} is outside the configured media_roots (allowed: {}); copy it under one of those or add its folder to media_roots",
            canonical.display(),
            allowed.join(", ")
        ));
    }
    if is_sensitive(&canonical) {
        return Err(format!(
            "{} looks like a credential or config file and is never uploaded",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn is_sensitive(path: &Path) -> bool {
    const DIRS: &[&str] = &[
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        ".password-store",
        "keyrings",
    ];
    // Gray's own state directories hold tokens and sessions; only the
    // per-conversation `work` directory inside them is the agent's output.
    const FILES: &[&str] = &[
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        "credentials",
        "credentials.json",
        "config.json",
        "auth.json",
        "token",
        "tokens.json",
        "secrets.json",
        "queue.sqlite",
    ];
    let components: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    let Some(name) = components.last() else {
        return true;
    };
    if components.iter().any(|part| DIRS.contains(&part.as_str())) {
        return true;
    }
    let in_gray_state = components
        .iter()
        .any(|part| part == "gray-discord" || part == ".gray");
    if in_gray_state && !components.iter().any(|part| part == "work") {
        return true;
    }
    if FILES.contains(&name.as_str()) || name.starts_with(".env") {
        return true;
    }
    name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.ends_with(".kdbx")
}

/// Read deliverable files into uploads with unique, Discord-safe names.
pub fn load(paths: &[PathBuf]) -> Vec<Upload> {
    load_pairs(paths)
        .into_iter()
        .map(|(_, upload)| upload)
        .collect()
}

/// [`load`], keeping which path each upload came from (unreadable paths are
/// skipped), so a caller can tell which files still need delivering.
pub fn load_pairs(paths: &[PathBuf]) -> Vec<(PathBuf, Upload)> {
    let mut used: Vec<String> = Vec::new();
    let mut uploads = Vec::new();
    for path in paths {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let original = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let mut name = safe_name(original);
        if used.contains(&name) {
            let (stem, ext) = match name.rsplit_once('.') {
                Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
                None => (name.clone(), String::new()),
            };
            let mut index = 2;
            while used.contains(&format!("{stem}-{index}{ext}")) {
                index += 1;
            }
            name = format!("{stem}-{index}{ext}");
        }
        used.push(name.clone());
        uploads.push((
            path.clone(),
            Upload {
                media_type: crate::channels::discord::component_media::infer_media_type(&name)
                    .to_string(),
                name,
                bytes,
            },
        ));
    }
    uploads
}

fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

/// Components for one batch of uploads (at most ten): visuals in a single
/// gallery, other files as file cards, with an optional caption on top.
pub fn components(uploads: &[Upload], caption: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(caption) = caption.map(str::trim).filter(|caption| !caption.is_empty()) {
        let caption: String = caption.chars().take(2000).collect();
        out.push(json!({"type": 10, "content": caption}));
    }
    let visuals: Vec<Value> = uploads
        .iter()
        .filter(|upload| upload.is_visual())
        .map(|upload| json!({"media": {"url": format!("attachment://{}", upload.name)}}))
        .collect();
    if !visuals.is_empty() {
        out.push(json!({"type": 12, "items": visuals}));
    }
    for upload in uploads.iter().filter(|upload| !upload.is_visual()) {
        out.push(json!({"type": 13, "file": {"url": format!("attachment://{}", upload.name)}}));
    }
    out
}

fn tidy(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let mut out = Vec::new();
    let mut blank = 0;
    for line in lines {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push(line);
    }
    out.join("\n").trim_start_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_display_hides_media_tags_and_their_lines() {
        let text = "Here is the chart:\nMEDIA:/tmp/chart.png\nand **MEDIA:/tmp/b.pdf** too";
        assert_eq!(strip_for_display(text), "Here is the chart:\nand too");
        assert_eq!(strip_for_display("no tags\n\nkept"), "no tags\n\nkept");
    }

    #[test]
    fn tags_are_stripped_only_when_deliverable() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("chart.png");
        std::fs::write(&image, b"png").unwrap();
        let text = format!(
            "Here you go:\n**MEDIA:{}**\nand MEDIA:/nope/missing.png stays.",
            image.display()
        );
        let (prose, paths) = extract(&text, dir.path(), &[]);
        assert_eq!(paths, vec![image.canonicalize().unwrap()]);
        assert!(!prose.contains("chart.png"), "{prose}");
        assert!(prose.contains("MEDIA:/nope/missing.png"), "{prose}");
        assert!(prose.starts_with("Here you go:"));
    }

    #[test]
    fn relative_paths_resolve_against_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.pdf"), b"%PDF").unwrap();
        let (prose, paths) = extract("Report: MEDIA:report.pdf.", dir.path(), &[]);
        assert_eq!(paths.len(), 1);
        assert_eq!(prose, "Report:");
    }

    #[test]
    fn secrets_are_never_deliverable() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(ssh.join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.path().join(".env"), b"KEY=1").unwrap();
        std::fs::write(dir.path().join("server.pem"), b"x").unwrap();
        let state = dir.path().join("gray-discord");
        std::fs::create_dir_all(state.join("conversations/abc/work")).unwrap();
        std::fs::write(state.join("config.json"), b"{}").unwrap();
        std::fs::write(state.join("notes.md"), b"x").unwrap();
        std::fs::write(state.join("conversations/abc/work/out.png"), b"x").unwrap();
        let (_, paths) = extract(
            "MEDIA:gray-discord/conversations/abc/work/out.png",
            dir.path(),
            &[],
        );
        assert_eq!(paths.len(), 1, "conversation work output is deliverable");
        for name in [
            ".ssh/notes.txt",
            ".env",
            "server.pem",
            "gray-discord/notes.md",
        ] {
            let (_, paths) = extract(&format!("MEDIA:{name}"), dir.path(), &[]);
            assert!(paths.is_empty(), "{name} must not be delivered");
        }
    }

    #[test]
    fn visuals_share_a_gallery_and_files_get_cards() {
        let uploads = vec![
            Upload {
                name: "a.png".into(),
                media_type: "image/png".into(),
                bytes: vec![1],
            },
            Upload {
                name: "b.pdf".into(),
                media_type: "application/pdf".into(),
                bytes: vec![1],
            },
        ];
        let components = components(&uploads, Some("Results"));
        crate::channels::discord::render::validate_components(&components).unwrap();
        assert_eq!(components[1]["type"], 12);
        assert_eq!(components[2]["file"]["url"], "attachment://b.pdf");
    }

    #[test]
    fn media_roots_restrict_sources_when_configured() {
        let allowed = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(allowed.path().join("in.png"), b"x").unwrap();
        std::fs::write(other.path().join("out.png"), b"x").unwrap();
        let config = json!({"media_roots": [allowed.path().to_string_lossy()]});
        let roots = roots_from_config(&config);
        assert_eq!(roots.len(), 1);
        assert!(deliverable(&allowed.path().join("in.png"), &roots).is_ok());
        let error = deliverable(&other.path().join("out.png"), &roots).unwrap_err();
        assert!(error.contains("media_roots"), "{error}");
        assert!(deliverable(&other.path().join("out.png"), &[]).is_ok());
    }

    #[test]
    fn duplicate_names_are_made_unique() {
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        std::fs::write(one.path().join("x.png"), b"1").unwrap();
        std::fs::write(two.path().join("x.png"), b"2").unwrap();
        let uploads = load(&[one.path().join("x.png"), two.path().join("x.png")]);
        assert_eq!(uploads[0].name, "x.png");
        assert_eq!(uploads[1].name, "x-2.png");
    }
}
