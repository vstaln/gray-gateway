//! Ported wholesale from gray-control. Entry points renamed: manifest/handle/main → *_control.
//! gray-control — per-session remote control over a unix socket.
//!
//! Each session binds `~/.gray/control/<sid>.sock` (lazily, on the first
//! hook that names a session) serving line-delimited JSON-RPC. On unix
//! `<sid>.sock` is a bound AF_UNIX socket; on other platforms it is a
//! small file holding the port of a 127.0.0.1 TCP listener, so discovery
//! still works by globbing the control dir:
//!
//!   {"method":"send","text":"..."}  → enqueue for the NEXT agent/before_start
//!                                     {text} injection (the reliable path),
//!                                     plus fire-and-forget host/say when the
//!                                     capability is granted
//!   {"method":"status"}             → sid, cwd, uptime, sock, pending count
//!   {"method":"stop"}               → reply ok, then stop the listener
//!
//! `send_to_session` {sid, text} writes to another session's socket (list
//! sockets = glob the control dir). `/control` prints the socket path and an
//! `nc` usage line. The socket file is removed on plugin/shutdown.

use std::io::{BufRead, BufReader, Write};
#[cfg(not(unix))]
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

// ---------- state -------------------------------------------------------------

/// The user's home dir: `$HOME`, or `%USERPROFILE%` on Windows where
/// HOME is typically unset for a natively-spawned process.
fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| user_home().map(|h| h.join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn control_dir() -> PathBuf {
    gray_home().join("control")
}

/// Keep a session id safe as a filename component.
fn sanitize(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() { "session".into() } else { s }
}

fn sock_path(sid: &str) -> PathBuf {
    control_dir().join(format!("{}.sock", sanitize(sid)))
}

/// Every live control socket → session id (filename minus `.sock`).
fn list_sessions() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(control_dir()) else {
        return Vec::new();
    };
    let mut v: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.strip_suffix(".sock").map(str::to_string)
        })
        .collect();
    v.sort();
    v
}

#[derive(Default)]
pub struct Session {
    pub sid: String,
    pub cwd: String,
    pub started: Option<Instant>,
    pub sock: Option<PathBuf>,
}

pub struct Shared {
    pub out: Arc<Mutex<std::io::Stdout>>,
    pub counter: Arc<Mutex<u64>>,
    pub queue: Mutex<Vec<String>>,
    pub session: Mutex<Session>,
    pub listener_up: AtomicBool,
    pub listen_err: Mutex<Option<String>>,
    pub stop: Arc<AtomicBool>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            out: Arc::new(Mutex::new(std::io::stdout())),
            counter: Arc::new(Mutex::new(0)),
            queue: Mutex::new(Vec::new()),
            session: Mutex::new(Session::default()),
            listener_up: AtomicBool::new(false),
            listen_err: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub fn note_session(sh: &Shared, params: &Value) {
    let Some(sess) = params.get("session") else {
        return;
    };
    let sid = sess
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let cwd = sess
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut s = sh.session.lock().expect("session");
    if !sid.is_empty() && s.sid.is_empty() {
        s.sid = sid;
        s.started = Some(Instant::now());
    }
    if !cwd.is_empty() {
        s.cwd = cwd;
    }
}

fn next_id(counter: &Arc<Mutex<u64>>) -> String {
    let mut n = counter.lock().expect("counter");
    *n += 1;
    format!("q{n}")
}

fn host_send(out: &Arc<Mutex<std::io::Stdout>>, method: &str, params: Value, id: Option<&str>) {
    let req = match id {
        Some(id) => json!({"id": id, "method": method, "params": params}),
        None => json!({"method": method, "params": params}),
    };
    let mut o = out.lock().expect("stdout");
    let _ = writeln!(o, "{req}");
    let _ = o.flush();
}

// ---------- socket listener ---------------------------------------------------

/// Control transport: an `AF_UNIX` socket bound at `<sid>.sock` on unix; a
/// loopback TCP listener elsewhere — its port is written into the same
/// `<sid>.sock` rendezvous file, so discovery still globs the control dir.
#[cfg(unix)]
type Listener = UnixListener;
#[cfg(not(unix))]
type Listener = TcpListener;
#[cfg(unix)]
type Stream = UnixStream;
#[cfg(not(unix))]
type Stream = TcpStream;

#[cfg(unix)]
fn bind_control(sock: &PathBuf) -> std::io::Result<Listener> {
    let _ = std::fs::remove_file(sock); // stale socket from a dead run
    UnixListener::bind(sock)
}

#[cfg(not(unix))]
fn bind_control(sock: &PathBuf) -> std::io::Result<Listener> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    std::fs::write(sock, listener.local_addr()?.port().to_string())?;
    Ok(listener)
}

#[cfg(unix)]
fn connect_control(sock: &PathBuf) -> std::io::Result<Stream> {
    UnixStream::connect(sock)
}

#[cfg(not(unix))]
fn connect_control(sock: &PathBuf) -> std::io::Result<Stream> {
    let port: u16 = std::fs::read_to_string(sock)?.trim().parse().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "bad control port file")
    })?;
    TcpStream::connect(("127.0.0.1", port))
}

/// The one-line client hint `/control` prints.
#[cfg(unix)]
fn usage_line(sock: &Path) -> String {
    format!(
        "echo '{{\"method\":\"send\",\"text\":\"hi\"}}' | nc -U {}",
        sock.display()
    )
}

#[cfg(not(unix))]
fn usage_line(sock: &PathBuf) -> String {
    let port = std::fs::read_to_string(sock)
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "<port>".into());
    format!("echo {{\"method\":\"send\",\"text\":\"hi\"}} | nc 127.0.0.1 {port}")
}

pub fn ensure_listener(sh: &Arc<Shared>) {
    if sh.listener_up.swap(true, Ordering::SeqCst) {
        return;
    }
    let sid = sh.session.lock().expect("session").sid.clone();
    if sid.is_empty() {
        sh.listener_up.store(false, Ordering::SeqCst);
        return;
    }
    let dir = control_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        sh.listener_up.store(false, Ordering::SeqCst);
        return;
    }
    let sock = dir.join(format!("{}.sock", sanitize(&sid)));
    match bind_control(&sock) {
        Ok(listener) => {
            sh.session.lock().expect("session").sock = Some(sock.clone());
            let sh2 = sh.clone();
            std::thread::spawn(move || accept_loop(sh2, listener, sock));
        }
        Err(e) => {
            *sh.listen_err.lock().expect("listen_err") = Some(e.to_string());
            sh.listener_up.store(false, Ordering::SeqCst);
        }
    }
}

fn accept_loop(sh: Arc<Shared>, listener: Listener, sock: PathBuf) {
    let _ = listener.set_nonblocking(true);
    while !sh.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => handle_conn(&sh, stream),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(_) => break,
        }
    }
    let _ = std::fs::remove_file(&sock);
}

fn handle_conn(sh: &Arc<Shared>, stream: Stream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let Ok(mut w) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let resp = dispatch_rpc(sh, &line);
        if writeln!(w, "{resp}").is_err() {
            break;
        }
        let _ = w.flush();
        if sh.stop.load(Ordering::SeqCst) {
            break;
        }
    }
}

/// One request line → one response line (JSON-RPC-ish; `id` echoed when sent).
fn dispatch_rpc(sh: &Arc<Shared>, line: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return json!({"ok": false, "error": "invalid JSON"}).to_string();
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let method = v
        .get("method")
        .or_else(|| v.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut result = match method {
        "send" => rpc_send(sh, &v),
        "status" => rpc_status(sh),
        "stop" => {
            sh.stop.store(true, Ordering::SeqCst);
            json!({"ok": true, "stopping": true})
        }
        other => json!({"ok": false, "error": format!("unknown method: {other}")}),
    };
    if !id.is_null() {
        result["id"] = id;
    }
    result.to_string()
}

fn rpc_send(sh: &Arc<Shared>, v: &Value) -> Value {
    let text = v
        .get("text")
        .or_else(|| v.get("message"))
        .or_else(|| v.pointer("/params/text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return json!({"ok": false, "error": "send needs a non-empty text"});
    }
    sh.queue.lock().expect("queue").push(text.clone());
    // Best-effort immediate surface: fire-and-forget — a dropped/erroring
    // host/say (capability ungranted) must not affect the queue path.
    let id = next_id(&sh.counter);
    host_send(
        &sh.out,
        "host/say",
        json!({"text": format!("[control] {text}")}),
        Some(&id),
    );
    json!({"ok": true, "queued": true})
}

fn rpc_status(sh: &Arc<Shared>) -> Value {
    let s = sh.session.lock().expect("session");
    let pending = sh.queue.lock().expect("queue").len();
    json!({
        "ok": true,
        "sid": s.sid,
        "cwd": s.cwd,
        "uptime_s": s.started.map(|t| t.elapsed().as_secs()).unwrap_or(0),
        "sock": s.sock.as_ref().map(|p| p.to_string_lossy().to_string()),
        "pending": pending,
    })
}

// ---------- hooks / tool / command ---------------------------------------------

fn before_start(sh: &Arc<Shared>, params: &Value) -> Value {
    note_session(sh, params);
    ensure_listener(sh);
    let msgs = std::mem::take(&mut *sh.queue.lock().expect("queue"));
    if msgs.is_empty() {
        json!({})
    } else {
        json!({"text": msgs.join("\n\n")})
    }
}

fn send_to_session(args: &Value) -> Result<String, String> {
    let sid = args.get("sid").and_then(Value::as_str).unwrap_or("").trim();
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if sid.is_empty() {
        return Err("missing required argument: sid".into());
    }
    if text.is_empty() {
        return Err("missing required argument: text".into());
    }
    let sock = sock_path(sid);
    if !sock.exists() {
        let others = list_sessions();
        return Err(if others.is_empty() {
            format!(
                "no control socket for '{sid}' (no live control sockets at all in {})",
                control_dir().display()
            )
        } else {
            format!(
                "no control socket for '{sid}' — live sessions: {}",
                others.join(", ")
            )
        });
    }
    let mut stream = connect_control(&sock)
        .map_err(|e| format!("couldn't connect to {}: {e}", sock.display()))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let req = json!({"method": "send", "text": text, "id": "send_to_session"});
    writeln!(stream, "{req}").map_err(|e| format!("write failed: {e}"))?;
    stream.flush().map_err(|e| format!("flush failed: {e}"))?;
    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(|e| format!("no reply from {sid}: {e}"))?;
    let v: Value =
        serde_json::from_str(reply.trim()).map_err(|e| format!("bad reply from {sid}: {e}"))?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(format!("sent to {sid} (queued for its next turn)"))
    } else {
        Err(format!(
            "{sid} refused: {}",
            v.get("error").and_then(Value::as_str).unwrap_or("unknown")
        ))
    }
}

fn run_command(sh: &Arc<Shared>, argv: &[&str]) -> String {
    match argv.first().copied() {
        Some("list") => {
            let sids = list_sessions();
            if sids.is_empty() {
                format!("no live control sockets in {}", control_dir().display())
            } else {
                format!("live control sessions:\n{}", sids.join("\n"))
            }
        }
        Some("stop") => {
            sh.stop.store(true, Ordering::SeqCst);
            "stopping the control listener (socket file removed on exit)".into()
        }
        _ => {
            let s = sh.session.lock().expect("session");
            let sock = s.sock.clone().unwrap_or_else(|| {
                sock_path(if s.sid.is_empty() {
                    "<pending>"
                } else {
                    &s.sid
                })
            });
            let err = sh.listen_err.lock().expect("listen_err").clone();
            let listening = if s.sock.is_some() {
                "listening".to_string()
            } else if let Some(e) = err {
                format!("bind failed: {e}")
            } else {
                "not bound yet".to_string()
            };
            format!(
                "gray-control {} — {}\nsock: {}\nusage: {}\nmethods: send {{text}} · status · stop — also: /control list, /control stop",
                env!("CARGO_PKG_VERSION"),
                listening,
                sock.display(),
                usage_line(&sock),
            )
        }
    }
}

// ---------- wire --------------------------------------------------------------

pub fn manifest_control() -> Value {
    json!({
        "name": "control",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "capabilities": ["host.say"],
        "tools": [{
            "name": "send_to_session",
            "description": "Send a message to another running gray session through its control socket (~/.gray/control/<sid>.sock). It lands as a user message on that session's next turn. List live session ids with `/control list` or by globbing the control dir.",
            "parameters": {
                "type": "object",
                "properties": {
                    "sid":  {"type": "string", "description": "Target session id (the <sid> in <sid>.sock)."},
                    "text": {"type": "string", "description": "Message delivered as a user message in the target session."}
                },
                "required": ["sid", "text"]
            }
        }],
        "commands": ["/control"],
        "hooks": ["agent/before_start"],
    })
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_control(sh: &Arc<Shared>, req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        if method == "plugin/shutdown" {
            sh.stop.store(true, Ordering::SeqCst);
        }
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_control(),
        "agent/before_start" => before_start(sh, &params),
        "tool/call" => {
            note_session(sh, &params);
            ensure_listener(sh);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match name {
                "send_to_session" => match send_to_session(&args) {
                    Ok(text) => json!({ "content": text }),
                    Err(e) => json!({ "content": e, "is_error": true }),
                },
                other => json!({ "content": format!("unknown tool: {other}"), "is_error": true }),
            }
        }
        "command/run" => {
            note_session(sh, &params);
            ensure_listener(sh);
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(sh, &argv) })
        }
        "plugin/shutdown" => {
            sh.stop.store(true, Ordering::SeqCst);
            if let Some(sock) = sh.session.lock().expect("session").sock.take() {
                let _ = std::fs::remove_file(sock);
            }
            return (Some(json!({ "id": id, "result": {} })), true);
        }
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}
