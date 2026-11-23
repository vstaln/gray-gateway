//! Point an existing channel service at the gateway binary.
//!
//! A channel that was installed before the parts merge has a runit `run`
//! script or a systemd user unit whose exec line names its own standalone
//! binary (for example `~/.gray/plugins/gray-telegram/bin/gray-telegram`).
//! That binary goes away when the plugin is replaced, so the unit is
//! repointed at `gray-gateway <channel> run`, keeping every other argument.

use std::path::{Path, PathBuf};

/// One service definition found on disk.
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

/// Where this user's services live, in the order the channel services
/// themselves resolve them.
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

/// Every definition for `channel` that exists, regardless of what it execs.
pub fn find(channel: &str) -> Vec<Definition> {
    let mut out = Vec::new();
    for dir in runit_dirs() {
        let run = dir.join(format!("gray-{channel}")).join("run");
        if let Ok(body) = std::fs::read_to_string(&run) {
            out.push(Definition {
                path: run,
                body,
                kind: Kind::Runit,
            });
        }
    }
    if let Some(dir) = systemd_dir() {
        for name in [
            format!("gray-{channel}-plugin.service"),
            format!("gray-{channel}.service"),
        ] {
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

/// The argv word this definition executes, if it can be read.
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

/// The argv of a definition's exec line, shell-unquoted.
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

/// Whether this definition already runs the gateway binary for `channel`.
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

/// Rewrite one definition so it execs `<exe> <channel> <rest>`.
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

/// Everything after the old binary, dropping a `<channel>` already there so a
/// second pass cannot stack prefixes.
fn tail_after_old_argv(rest: &str, channel: &str, kind: Kind) -> String {
    let mut words = split_words(rest);
    if !words.is_empty() {
        words.remove(0);
    }
    if words.first().map(String::as_str) == Some(channel) {
        words.remove(0);
    }
    let quote = |w: &String| match kind {
        Kind::Runit => sh_quote(w),
        Kind::Systemd => systemd_quote(w),
    };
    words.iter().map(quote).collect::<Vec<_>>().join(" ")
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

/// Point every existing definition for `channel` at this build. Returns the
/// paths rewritten; a definition that already names this binary is left
/// untouched, so running it twice rewrites nothing the second time.
pub fn adopt(channel: &str) -> Vec<PathBuf> {
    let Some(exe) = current_binary() else {
        return Vec::new();
    };
    let mut touched = Vec::new();
    for def in find(channel) {
        if is_current(&def, channel, &exe) {
            continue;
        }
        let Some(new_body) = rewrite(&def, channel, &exe) else {
            continue;
        };
        if new_body == def.body {
            continue;
        }
        if std::fs::write(&def.path, &new_body).is_ok() {
            touched.push(def.path);
        }
    }
    touched
}
