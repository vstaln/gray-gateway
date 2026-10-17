use std::path::{Path, PathBuf};

pub fn gray_home() -> anyhow::Result<PathBuf> {
    if let Ok(v) = std::env::var("GRAY_HOME") {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home: set GRAY_HOME"))?;
    Ok(home.join(".gray"))
}

pub fn saved_config_path() -> anyhow::Result<PathBuf> {
    Ok(gray_home()?.join("config.json"))
}

fn saved_config_value(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn flag_at(path: &Path, key: &str) -> bool {
    saved_config_value(path)
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

pub fn cron_auto_enabled() -> bool {
    saved_config_path()
        .map(|p| flag_at(&p, "cron_auto"))
        .unwrap_or(true)
}

pub fn gw_auto_enabled() -> bool {
    saved_config_path()
        .map(|p| flag_at(&p, "gw_auto"))
        .unwrap_or(true)
}

pub fn auto_update_enabled() -> bool {
    saved_config_path()
        .map(|p| flag_at(&p, "auto_update"))
        .unwrap_or(true)
}

pub fn set_flag(key: &str, on: bool) -> anyhow::Result<()> {
    let path = saved_config_path()?;
    let mut v = saved_config_value(&path);
    if !v.is_object() {
        v = serde_json::json!({});
    }
    if on {
        v.as_object_mut().map(|o| o.remove(key));
    } else {
        v[key] = serde_json::Value::Bool(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&v)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn valid_session_id(s: &str) -> bool {
    if s.is_empty()
        || s.len() > 128
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if matches!(lower.as_str(), "con" | "prn" | "aux" | "nul") {
        return false;
    }
    if lower.len() == 4
        && (lower.starts_with("com") || lower.starts_with("lpt"))
        && lower.as_bytes()[3].is_ascii_digit()
        && lower.as_bytes()[3] != b'0'
    {
        return false;
    }
    true
}

pub fn append_injected_user(home: &Path, sid: &str, text: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    if !valid_session_id(sid) {
        anyhow::bail!("invalid session id {sid:?}");
    }
    let path = home.join("sessions").join(format!("{sid}.jsonl"));
    let content = std::fs::read_to_string(&path)?;
    let mut max_id: Option<u64> = None;
    let mut last_id: Option<u64> = None;
    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(id) = v.get("entry_id").and_then(|x| x.as_u64()) {
            max_id = Some(max_id.map_or(id, |m: u64| m.max(id)));
            last_id = Some(id);
        }
    }
    let entry = serde_json::json!({
        "entry_id": max_id.map_or(0, |m| m + 1),
        "parent_id": last_id,
        "timestamp": now_millis(),
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}],
            "injected": true
        }
    });
    let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
    file.write_all(format!("{entry}\n").as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn gray_bin() -> PathBuf {
    std::env::var_os("GRAY_BIN")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("gray"))
}

#[derive(Debug, Clone)]
pub struct ExeWatch {
    path: PathBuf,
    stamp: Option<std::time::SystemTime>,
}

impl ExeWatch {
    pub fn capture() -> Option<Self> {
        let path = gray_bin();
        let real = std::fs::canonicalize(&path).unwrap_or(path);
        let stamp = std::fs::metadata(&real).and_then(|m| m.modified()).ok();
        stamp.map(|stamp| Self {
            path: real,
            stamp: Some(stamp),
        })
    }

    pub fn replaced(&self) -> bool {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()
            != self.stamp
    }
}

fn skill_name_of(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim_start();
    if trimmed.starts_with("---") {
        for line in trimmed[3..].lines() {
            if line.trim() == "---" {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.trim() == "name"
            {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_string)
}

fn find_in(root: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth > 6 || !root.is_dir() {
        return None;
    }
    let direct = root.join(name).join("SKILL.md");
    if direct.is_file() {
        return Some(direct);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if let Some(hit) = find_in(&path, name, depth + 1) {
                return Some(hit);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md")
            && skill_name_of(&path).as_deref() == Some(name)
        {
            return Some(path);
        }
    }
    None
}

fn skill_roots(cwd: &Path, agent_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![agent_dir.join("skills")];
    if std::env::var("GRAY_SKILLS_ONLY").as_deref() != Ok("1")
        && let Some(home) = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
    {
        let config_base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"));
        let opencode = config_base.join("opencode");
        if let Ok(entries) = std::fs::read_dir(&opencode) {
            let mut subs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path().join("skills"))
                .filter(|p| p.is_dir())
                .collect();
            subs.sort();
            roots.extend(subs);
        }
        roots.push(home.join(".agents").join("skills"));

        let mut cur = Some(cwd.to_path_buf());
        let mut ancestors = Vec::new();
        let git_root = {
            let mut c = Some(cwd.to_path_buf());
            loop {
                match c {
                    Some(d) if d.join(".git").exists() => break Some(d),
                    Some(d) => c = d.parent().map(|p| p.to_path_buf()),
                    None => break None,
                }
            }
        };
        while let Some(dir) = cur {
            ancestors.push(dir.clone());
            if Some(&dir) == git_root.as_ref() {
                break;
            }
            cur = dir.parent().map(|p| p.to_path_buf());
        }
        for ancestor in ancestors.iter().rev() {
            for cfg in [".gray", ".opencode", ".agents"] {
                roots.push(ancestor.join(cfg).join("skills"));
            }
        }
    }
    roots
}

pub fn resolve_skill_name(cwd: &Path, name: &str) -> Option<PathBuf> {
    let agent_dir = gray_home().unwrap_or_else(|_| PathBuf::from(".gray"));
    for root in skill_roots(cwd, &agent_dir) {
        if let Some(hit) = find_in(&root, name, 0) {
            return Some(hit);
        }
    }
    None
}

pub fn setup_apps(home: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(home.join("plugins/lock.json")) else {
        return Vec::new();
    };
    let Ok(lock) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(plugins) = lock.get("plugins").and_then(|p| p.as_object()) {
        for (name, entry) in plugins {
            if entry.get("cli_argv").is_none()
                || entry.get("enabled") == Some(&serde_json::Value::Bool(false))
            {
                continue;
            }
            let manifest = home.join("plugins").join(format!("{name}-manifest.json"));
            let has_setup = std::fs::read_to_string(manifest)
                .ok()
                .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                .and_then(|m| m["completion"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .any(|v| v.as_str() == Some("setup"));
            if has_setup {
                out.push(name.clone());
            }
        }
    }
    out.sort();
    out
}

pub fn gray_cli(args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(std::process::Command::new(gray_bin()).args(args).output()?)
}
