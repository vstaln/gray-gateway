//! Config-file plumbing the bridges share: where a channel's JSON config
//! lives, an atomic 0600 write, and the integer range check every
//! `validate_config` runs.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// The user's home directory — `$HOME` on unix, `USERPROFILE` on Windows,
/// the same places gray itself resolves.
pub fn user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// `<home>/.config/gray-<channel>/config.json`, the path every channel kept
/// as a standalone plugin.
pub fn default_path(channel: &str) -> PathBuf {
    user_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!(".config/gray-{channel}/config.json"))
}

/// The directory a channel's config lives in — where its pid file, log and
/// lifecycle markers sit.
pub fn config_dir_of(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Write JSON (pretty + trailing newline) atomically: parents 0700,
/// temp file + rename, replacement mode 0600.
pub fn atomic_json(path: &Path, data: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(parent)
            .map_err(|_| "cannot create config directory".to_string())?;
    }
    let mut text =
        serde_json::to_string_pretty(data).map_err(|_| "cannot encode config".to_string())?;
    text.push('\n');
    let tmp_path = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&tmp_path)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(|_| "cannot write config".to_string())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| "cannot protect config".to_string())?;
    }
    std::fs::rename(&tmp_path, path).map_err(|_| "cannot replace config".to_string())?;
    Ok(())
}

/// Read + parse a config file; any failure maps to the setup hint.
pub fn load(path: &Path) -> Result<Value, String> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "Configuration missing or invalid; run setup".to_string())
}

pub fn int_in(data: &Value, key: &str, low: u64, high: u64) -> Result<(), String> {
    if let Some(v) = data.get(key) {
        let ok = v.as_u64().is_some_and(|n| (low..=high).contains(&n));
        if !ok {
            return Err(format!("{key} must be an integer between {low} and {high}"));
        }
    }
    Ok(())
}

pub fn bool_opt(data: &Value, key: &str) -> Result<(), String> {
    if data.get(key).is_some_and(|v| !v.is_boolean()) {
        return Err(format!("{key} must be true or false"));
    }
    Ok(())
}
