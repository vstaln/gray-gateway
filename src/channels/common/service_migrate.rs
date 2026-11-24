use std::path::{Path, PathBuf};

pub struct Definition {
    pub path: PathBuf,
    pub body: String,
    pub kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Runit,
    Systemd,
}

pub struct Names {
    pub svc: &'static str,
    pub unit: &'static str,
}

pub fn names(channel: &str) -> Option<Names> {
    let (svc, unit) = match channel {
        "discord" => (
            crate::channels::discord::service::SVC,
            crate::channels::discord::service::NAME,
        ),
        "telegram" => (
            crate::channels::telegram::service::SVC,
            crate::channels::telegram::service::NAME,
        ),
        "slack" => (
            crate::channels::slack::service::SVC,
            crate::channels::slack::service::NAME,
        ),
        "signal" => (
            crate::channels::signal::service::SVC,
            crate::channels::signal::service::NAME,
        ),
        "matrix" => (
            crate::channels::matrix::service::SVC,
            crate::channels::matrix::service::NAME,
        ),
        "irc" => (
            crate::channels::irc::service::SVC,
            crate::channels::irc::service::NAME,
        ),
        "email" => (
            crate::channels::email::service::SVC,
            crate::channels::email::service::NAME,
        ),
        "ntfy" => (
            crate::channels::ntfy::service::SVC,
            crate::channels::ntfy::service::NAME,
        ),
        _ => return None,
    };
    Some(Names { svc, unit })
}

fn runit_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("SVDIR") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(h) = crate::channels::common::config::user_home() {
        dirs.push(h.join(".config/service"));
        dirs.push(h.join("service"));
        dirs.push(h.join(".gray/service"));
        dirs.push(h.join(".gray/plugins/service"));
    }
    dirs.push(PathBuf::from("/var/service"));
    dirs
}

fn systemd_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(d).join("systemd/user"));
    }
    crate::channels::common::config::user_home().map(|h| h.join(".config/systemd/user"))
}

pub fn find(channel: &str) -> Vec<Definition> {
    let mut out = Vec::new();
    let Some(n) = names(channel) else {
        return out;
    };
    for dir in runit_dirs() {
        let run = dir.join(n.svc).join("run");
        if let Ok(body) = std::fs::read_to_string(&run) {
            out.push(Definition {
                path: run,
                body,
                kind: Kind::Runit,
            });
        }
    }
    if let Some(dir) = systemd_dir() {
        let mut names = vec![
            n.unit.to_string(),
            format!("gray-{channel}-plugin.service"),
            format!("gray-{channel}.service"),
        ];
        names.sort();
        names.dedup();
        for name in names {
            let path = dir.join(&name);
            if let Ok(body) = std::fs::read_to_string(&path) {
                out.push(Definition {
                    path,
                    body,
                    kind: Kind::Systemd,
                });
            }
        }
    }
    out
}

fn exec_word(def: &Definition) -> Option<String> {
    match def.kind {
        Kind::Runit => def
            .body
            .lines()
            .find_map(|l| l.trim().strip_prefix("exec "))
            .and_then(first_shell_word),
        Kind::Systemd => def
            .body
            .lines()
            .find_map(|l| l.trim().strip_prefix("ExecStart="))
            .and_then(first_shell_word),
    }
}

fn first_shell_word(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('\'') {
        let end = stripped.find('\'')?;
        return Some(stripped[..end].to_string());
    }
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        return Some(stripped[..end].to_string());
    }
    rest.split_whitespace().next().map(str::to_string)
}

fn current_binary() -> Option<PathBuf> {
    std::fs::canonicalize(std::env::current_exe().ok()?).ok()
}

fn exec_argv(def: &Definition) -> Vec<String> {
    let line = match def.kind {
        Kind::Runit => def
            .body
            .lines()
            .find_map(|l| l.trim().strip_prefix("exec ")),
        Kind::Systemd => def
            .body
            .lines()
            .find_map(|l| l.trim().strip_prefix("ExecStart=")),
    };
    line.map(split_words).unwrap_or_default()
}

fn split_words(rest: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut chars = rest.trim().chars().peekable();
    let mut cur = String::new();
    let mut quoted: Option<char> = None;
    while let Some(c) = chars.next() {
        match quoted {
            Some(q) if c == q => quoted = None,
            Some('"') if c == '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => quoted = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

pub fn is_current(def: &Definition, channel: &str, exe: &Path) -> bool {
    let argv = exec_argv(def);
    let Some(first) = argv.first() else {
        return false;
    };
    let Ok(canonical) = std::fs::canonicalize(first) else {
        return false;
    };
    canonical == exe && argv.get(1).map(String::as_str) == Some(channel)
}

pub fn rewrite(def: &Definition, channel: &str, exe: &Path) -> Option<String> {
    let exe_s = exe.to_string_lossy();
    match def.kind {
        Kind::Runit => {
            let mut out = String::new();
            let mut done = false;
            for line in def.body.lines() {
                let trimmed = line.trim_start();
                if !done && let Some(rest) = trimmed.strip_prefix("exec ") {
                    let tail = tail_after_old_argv(rest, channel, Kind::Runit);
                    out.push_str(&format!("exec {} {}", sh_quote(&exe_s), sh_quote(channel)));
                    if !tail.is_empty() {
                        out.push(' ');
                        out.push_str(&tail);
                    }
                    out.push('\n');
                    done = true;
                    continue;
                }
                out.push_str(line);
                out.push('\n');
            }
            done.then_some(out)
        }
        Kind::Systemd => {
            let mut out = String::new();
            let mut done = false;
            for line in def.body.lines() {
                let trimmed = line.trim_start();
                if !done && let Some(rest) = trimmed.strip_prefix("ExecStart=") {
                    let tail = tail_after_old_argv(rest, channel, Kind::Systemd);
                    out.push_str(&format!(
                        "ExecStart={} {}",
                        systemd_quote(&exe_s),
                        systemd_quote(channel)
                    ));
                    if !tail.is_empty() {
                        out.push(' ');
                        out.push_str(&tail);
                    }
                    out.push('\n');
                    done = true;
                    continue;
                }
                out.push_str(line);
                out.push('\n');
            }
            done.then_some(out)
        }
    }
}

fn tail_after_old_argv(rest: &str, channel: &str, _kind: Kind) -> String {
    let mut tail = skip_words(rest, 1);
    if first_word(tail).as_deref() == Some(channel) {
        tail = skip_words(tail, 1);
    }
    tail.trim().to_string()
}

fn skip_words(s: &str, n: usize) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut skipped = 0;
    while skipped < n {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return "";
        }
        let mut quoted: Option<u8> = None;
        while i < bytes.len() {
            let c = bytes[i];
            match quoted {
                Some(q) if c == q => {
                    quoted = None;
                    i += 1;
                }
                Some(b'"') if c == b'\\' => i += 2,
                Some(_) => i += 1,
                None if c == b'\'' || c == b'"' => {
                    quoted = Some(c);
                    i += 1;
                }
                None if c.is_ascii_whitespace() => break,
                None => i += 1,
            }
        }
        skipped += 1;
    }
    &s[i.min(s.len())..]
}

fn first_word(s: &str) -> Option<String> {
    split_words(s).into_iter().next()
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn systemd_quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}

pub fn adopt(channel: &str) -> Vec<PathBuf> {
    let changed = rewrite_changed(channel);
    if !changed.is_empty() {
        reload(channel, &changed);
    }
    changed.into_iter().map(|(d, _)| d.path).collect()
}

fn rewrite_changed(channel: &str) -> Vec<(Definition, Vec<String>)> {
    let Some(exe) = current_binary() else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    for def in find(channel) {
        if is_current(&def, channel, &exe) {
            continue;
        }
        let was = exec_argv(&def);
        let Some(new_body) = rewrite(&def, channel, &exe) else {
            continue;
        };
        if new_body == def.body {
            continue;
        }
        if write_preserving_mode(&def.path, &new_body).is_ok() {
            changed.push((def, was));
        }
    }
    changed
}

#[cfg(unix)]
fn write_preserving_mode(path: &Path, body: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    let tmp = path.with_extension("gray-tmp");
    std::fs::write(&tmp, body)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)
}

#[cfg(not(unix))]
fn write_preserving_mode(path: &Path, body: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("gray-tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

fn have(tool: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|d| d.join(tool).is_file())
}

fn reload(channel: &str, changed: &[(Definition, Vec<String>)]) {
    let Some(n) = names(channel) else {
        return;
    };
    let dirs = runit_dirs();
    for (def, was) in changed {
        let dir = def.path.parent().unwrap_or(&def.path);
        if let Some(svdir) = dirs.iter().find(|d| d.join(n.svc) == *dir) {
            if was.is_empty() || !is_running_argv(was) || !have("sv") {
                continue;
            }
            let _ = std::process::Command::new("sv")
                .arg("-w")
                .arg("10")
                .arg("restart")
                .arg(svdir.join(n.svc))
                .output();
            continue;
        }
        if !have("systemctl") {
            continue;
        }
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        if !was.is_empty() && is_running_argv(was) {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "restart", n.unit])
                .output();
        }
    }
}

fn is_running_argv(argv: &[String]) -> bool {
    let Some(binary) = argv.first() else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let words: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        if words.first().map(|w| String::from_utf8_lossy(w)) == Some(binary.as_str().into()) {
            return true;
        }
    }
    false
}
