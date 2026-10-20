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

pub(crate) fn lock_file_at(
    path: &Path,
    timeout: std::time::Duration,
) -> anyhow::Result<Option<std::fs::File>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(Some(f)),
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for lock {}", path.display());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                log::warn!(
                    "file locking unsupported on {} ({e}); proceeding unlocked",
                    path.display()
                );
                return Ok(None);
            }
        }
    }
}

pub fn set_flag(key: &str, on: bool) -> anyhow::Result<()> {
    let path = saved_config_path()?;
    let _lock = lock_file_at(
        &path.with_extension("lock"),
        std::time::Duration::from_secs(5),
    )?;
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let mut v = if body.trim().is_empty() {
        serde_json::json!({})
    } else {
        let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            anyhow::anyhow!(
                "{} is not valid JSON ({e}); refusing to overwrite",
                path.display()
            )
        })?;
        if !v.is_object() {
            anyhow::bail!(
                "{} is not a JSON object; refusing to overwrite",
                path.display()
            );
        }
        v
    };
    if on {
        v.as_object_mut().map(|o| o.remove(key));
    } else {
        v[key] = serde_json::Value::Bool(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!(".gray-{}.tmp", uuid::Uuid::new_v4()));
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    std::io::Write::write_all(
        &mut opts.open(&tmp)?,
        serde_json::to_string_pretty(&v)?.as_bytes(),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
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
    let dir = home.join("sessions");
    let path = dir.join(format!("{sid}.jsonl"));
    let _lock = lock_file_at(
        &dir.join(format!("{sid}.lock")),
        std::time::Duration::from_secs(30),
    )?;
    let mut content = std::fs::read_to_string(&path)?;
    if !content.ends_with('\n')
        && let Some(cut) = content.rfind('\n')
    {
        let f = std::fs::OpenOptions::new().write(true).open(&path)?;
        f.set_len((cut + 1) as u64)?;
        f.sync_all()?;
        log::warn!(
            "{}: truncated torn tail to last complete record",
            path.display()
        );
        content.truncate(cut + 1);
    }
    let mut lines = content.lines().filter(|l| !l.trim().is_empty());
    if lines.next().is_none() {
        anyhow::bail!("session {sid:?} has no entries");
    }
    if !content.ends_with('\n') {
        anyhow::bail!("session has an incomplete tail; repair before appending");
    }
    let mut max_id: Option<u64> = None;
    let mut last_id: Option<u64> = None;
    let mut seen_first = false;
    for (n, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if !seen_first {
            seen_first = true;
            continue;
        }
        let lineno = n + 1;
        let v: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("corrupt session entry at line {lineno}: {e}"))?;
        let id = v.get("entry_id").and_then(|x| x.as_u64()).ok_or_else(|| {
            anyhow::anyhow!("corrupt session entry at line {lineno}: missing entry_id")
        })?;
        max_id = Some(max_id.map_or(id, |m: u64| m.max(id)));
        last_id = Some(id);
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

pub fn gray_bin_abs() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("GRAY_BIN").map(PathBuf::from) {
        anyhow::ensure!(
            p.is_file(),
            "GRAY_BIN points at {} which does not exist",
            p.display()
        );
        return Ok(std::fs::canonicalize(&p).unwrap_or(p));
    }
    let raw = crate::gateway::service::find_in_path("gray").ok_or_else(|| {
        anyhow::anyhow!("cannot resolve the gray binary: set GRAY_BIN or put gray on PATH")
    })?;
    Ok(std::fs::canonicalize(&raw).unwrap_or(raw))
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
    if let Some(front) = trimmed.strip_prefix("---") {
        for line in front.lines() {
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

/// A chat platform `gray gateway setup` can offer: an installed plugin the
/// gateway hosts as a sidecar, with a `setup` verb to walk through.
pub struct SetupApp {
    pub name: String,
    /// The plugin's CLI binary when one can be found. `setup` execs it
    /// directly, so a platform still configures when `gray <name>`
    /// forwarding can't — catalog sidecar installs have no `cli_argv`.
    pub exe: Option<PathBuf>,
}

/// Installed plugins usable as gateway chat apps, sorted by name.
pub fn setup_apps(home: &Path) -> Vec<SetupApp> {
    let Ok(raw) = std::fs::read_to_string(home.join("plugins/lock.json")) else {
        return Vec::new();
    };
    let Ok(lock) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(plugins) = lock.get("plugins").and_then(|p| p.as_object()) {
        for (name, entry) in plugins {
            if name == "gateway" || entry.get("enabled") == Some(&serde_json::Value::Bool(false)) {
                continue;
            }
            let dir = home.join("plugins").join(name);
            let Some((manifest, exe)) = app_manifest(home, name, entry, &dir) else {
                continue;
            };
            if is_setup_app(&manifest, entry, &dir) {
                out.push(SetupApp {
                    name: name.clone(),
                    exe,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The plugin's manifest plus its CLI binary: the cached
/// `<name>-manifest.json` when present, else a `manifest` probe of the
/// plugin's executables (a catalog sidecar install never wrote the cache).
fn app_manifest(
    home: &Path,
    name: &str,
    entry: &serde_json::Value,
    dir: &Path,
) -> Option<(serde_json::Value, Option<PathBuf>)> {
    let cached = home.join("plugins").join(format!("{name}-manifest.json"));
    if let Some(m) = std::fs::read_to_string(&cached)
        .ok()
        .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
    {
        return Some((m, exe_candidates(name, entry, dir).into_iter().next()));
    }
    for cand in exe_candidates(name, entry, dir) {
        if let Some(m) = probe_manifest(&cand, name) {
            return Some((m, Some(cand)));
        }
    }
    None
}

/// Does the manifest describe a gateway chat app — a sidecar plugin with a
/// `setup` verb? The gateway itself has no sidecar shape, so this never
/// lists it as its own platform.
fn is_setup_app(manifest: &serde_json::Value, entry: &serde_json::Value, dir: &Path) -> bool {
    let has_setup = ["completion", "subcommands"].iter().any(|key| {
        manifest[*key]
            .as_array()
            .into_iter()
            .flatten()
            .any(|v| v.as_str() == Some("setup"))
    });
    let sidecar = manifest["sidecar_args"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
        || entry["argv"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|v| v.as_str() == Some("sidecar"))
        || dir.join("plugin.sh").is_file();
    has_setup && sidecar
}

/// Executables that might carry this plugin's CLI, best first: the
/// registered argvs, then `gray-<name>`-style binaries in the plugin dir
/// (one level of per-target subdirs covered). `plugin.sh` is skipped — it
/// launches the sidecar, never a `manifest` reply.
fn exe_candidates(name: &str, entry: &serde_json::Value, dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for key in ["cli_argv", "argv"] {
        if let Some(p) = entry[key]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
        {
            let path = PathBuf::from(p);
            if path.is_file() && !out.contains(&path) {
                out.push(path);
            }
        }
    }
    let mut named = Vec::new();
    let mut rest = Vec::new();
    let mut scan = |d: &Path| {
        let Ok(rd) = std::fs::read_dir(d) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Ok(sub) = std::fs::read_dir(&p) {
                    for se in sub.flatten() {
                        let sp = se.path();
                        if sp.is_file()
                            && is_exec(&sp)
                            && sp.file_name() != Some("plugin.sh".as_ref())
                        {
                            rest.push(sp);
                        }
                    }
                }
            } else if is_exec(&p) && p.file_name() != Some("plugin.sh".as_ref()) {
                let stem = p.file_stem().and_then(|s| s.to_str());
                if stem == Some(format!("gray-{name}").as_str()) || stem == Some(name) {
                    named.push(p);
                } else {
                    rest.push(p);
                }
            }
        }
    };
    scan(dir);
    named.sort();
    rest.sort();
    out.extend(named);
    out.extend(rest);
    out
}

#[cfg(unix)]
fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && std::fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_exec(p: &Path) -> bool {
    p.is_file()
}

/// Run `<exe> manifest` and keep the reply when it parses and names this
/// plugin. Bounded so a wedged binary can't hang setup.
fn probe_manifest(exe: &Path, name: &str) -> Option<serde_json::Value> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = std::process::Command::new(exe)
        .arg("manifest")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = Vec::new();
    child.stdout.take()?.read_to_end(&mut out).ok()?;
    let m: serde_json::Value = serde_json::from_slice(&out).ok()?;
    (m["name"].as_str() == Some(name)).then_some(m)
}

pub fn gray_cli(args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(std::process::Command::new(gray_bin()).args(args).output()?)
}
