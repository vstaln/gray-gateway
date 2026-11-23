//! webchat — a local chat page for the agent. The sidecar hosts
//! 127.0.0.1:PORT (default 8889); POST /send enqueues a message, a
//! writer thread turns it into `host/run` (needs the `host.turn`
//! capability — `gray plugin capabilities webchat --all`), and the
//! agent's reply lands in the ring buffer that GET /log serves.

use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const NAME: &str = "webchat";
const DEFAULT_PORT: u16 = 8889;

pub fn manifest() -> Value {
    json!({
        "name": NAME, "display_name": "WebChat",
        "version": env!("CARGO_PKG_VERSION"), "protocol": "1.1",
        "capabilities": ["host.turn"],
        "description": "chat with gray from a local web page",
        "commands": ["/webchat"], "subcommands": ["webchat"],
        "command_descriptions": {"/webchat": "serve a browser chat for this session"},
        "tools": [],
    })
}

const PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>gray webchat</title>
<style>body{font:14px system-ui;max-width:720px;margin:2em auto;background:#111;color:#eee}
#log{white-space:pre-wrap;border:1px solid #333;padding:1em;min-height:20em;border-radius:8px}
.you{color:#8fd} .agent{color:#cde} .sys{color:#888;font-style:italic}
input{width:80%;padding:.6em;background:#222;color:#eee;border:1px solid #444;border-radius:6px}
button{padding:.6em 1em}</style>
<div id=log></div><br><input id=q autofocus placeholder="message the agent…"><button onclick=send()>send</button>
<script>
let n=0;
async function poll(){const r=await fetch('/log?after='+n);const d=await r.json();
for(const m of d.messages){n=m.n;const el=document.getElementById('log');
el.innerHTML+=`<div class=${m.who}>[${m.who}] ${esc(m.text)}</div>`;el.scrollTop=el.scrollHeight}setTimeout(poll,1200)}
function esc(s){return s.replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]))}
async function send(){const q=document.getElementById('q');if(!q.value)return;
await fetch('/send',{method:'POST',body:JSON.stringify({text:q.value})});q.value=''}
document.getElementById('q').addEventListener('keydown',e=>{if(e.key==='Enter')send()});
poll()</script>"#;

struct Shared {
    queue: VecDeque<(u64, String)>,
    /// wc-N → the log sink the reply should land in
    waiters: HashMap<String, u64>,
    log: Vec<Value>,
    next_n: AtomicU64,
    bound: Option<Result<u16, String>>,
}

fn config() -> u16 {
    let home = std::env::var_os("GRAY_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let p = home.join("webchat").join("config.json");
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|c| c["port"].as_u64().map(|p| p as u16))
        .or_else(|| {
            std::env::var("WEBCHAT_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(DEFAULT_PORT)
}

fn resp(s: &mut TcpStream, code: u16, body: &str, ctype: &str) {
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        match code {
            200 => "OK",
            202 => "Accepted",
            404 => "Not Found",
            _ => "Error",
        },
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body.as_bytes());
}

fn http_conn(mut s: TcpStream, sh: Arc<Mutex<Shared>>, cv: Arc<std::sync::Condvar>) {
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut parts = head.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut clen = 0usize;
    for l in head.lines().skip(1) {
        if let Some((k, v)) = l.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            clen = v.trim().parse().unwrap_or(0).min(1024 * 1024);
        }
    }
    while buf.len() < head_end + clen {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = String::from_utf8_lossy(&buf[head_end..buf.len().min(head_end + clen)]).into_owned();
    match (method.as_str(), path.split('?').next().unwrap_or("/")) {
        ("GET", "/") => resp(&mut s, 200, PAGE, "text/html"),
        ("GET", "/log") => {
            let after: u64 = path
                .split("after=")
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let g = sh.lock().unwrap();
            let msgs: Vec<Value> = g
                .log
                .iter()
                .filter(|m| m["n"].as_u64().unwrap_or(0) > after)
                .cloned()
                .collect();
            resp(
                &mut s,
                200,
                &json!({"messages": msgs}).to_string(),
                "application/json",
            );
        }
        ("POST", "/send") => {
            let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
            let text = v["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                resp(&mut s, 400, "{\"error\":\"empty\"}", "application/json");
                return;
            }
            let mut g = sh.lock().unwrap();
            let id = g.next_n.fetch_add(1, Ordering::SeqCst) + 1;
            g.log.push(json!({"n": id, "who": "you", "text": text}));
            g.queue.push_back((id, text));
            cv.notify_one();
            resp(
                &mut s,
                202,
                &json!({"id": id}).to_string(),
                "application/json",
            );
        }
        _ => resp(&mut s, 404, "{}", "application/json"),
    }
}

fn append_log(sh: &Arc<Mutex<Shared>>, who: &str, text: String) {
    let mut g = sh.lock().unwrap();
    let n = g.next_n.fetch_add(1, Ordering::SeqCst) + 1;
    g.log.push(json!({"n": n, "who": who, "text": text}));
    if g.log.len() > 400 {
        g.log.remove(0);
    }
}

/// Pump thread: blocks on the condvar until /send enqueues, then writes
/// one `host/run` per queued message through the shared wire writer —
/// independent of stdin traffic, so messages flow while the host is idle.
fn pump(sh: Arc<Mutex<Shared>>, cv: Arc<std::sync::Condvar>, out: Arc<Mutex<std::io::Stdout>>) {
    let mut counter = 0u64;
    let mut g = sh.lock().unwrap();
    loop {
        while g.queue.is_empty() {
            g = cv.wait(g).unwrap();
        }
        let (num, text) = g.queue.pop_front().unwrap();
        counter += 1;
        let wid = format!("wc-{num}-{counter}");
        g.waiters.insert(wid.clone(), num);
        drop(g);
        let mut w = out.lock().unwrap();
        let _ = writeln!(
            w,
            "{}",
            json!({"id": wid, "method": "host/run",
            "params": {"prompt": text}})
        );
        let _ = w.flush();
        drop(w);
        g = sh.lock().unwrap();
    }
}

fn fulfill_waiters(sh: &Arc<Mutex<Shared>>, id: &str, result: &Value) {
    let found = sh.lock().unwrap().waiters.remove(id).is_some();
    if found {
        let text = result["text"]
            .as_str()
            .or_else(|| result["output"].as_str())
            .or_else(|| result.pointer("/result/text").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| result.to_string());
        append_log(sh, "agent", text);
    }
}

/// The gateway sidecar's webchat entry: it hosts the page and pumps
/// `host/run`, so a message from the browser reaches the session without the
/// plugin copy of this loop.
pub fn serve() -> std::io::Result<()> {
    let sh = Arc::new(Mutex::new(Shared {
        queue: VecDeque::new(),
        waiters: HashMap::new(),
        log: Vec::new(),
        next_n: AtomicU64::new(0),
        bound: None,
    }));
    let cv = Arc::new(std::sync::Condvar::new());
    let out = Arc::new(Mutex::new(std::io::stdout()));
    {
        let (sh2, cv2, out2) = (sh.clone(), cv.clone(), out.clone());
        std::thread::spawn(move || pump(sh2, cv2, out2));
    }
    let port = config();
    let _ = PORT.set(port);
    let _ = BOUND.set(Mutex::new(None));
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => {
            let (sh2, cv2) = (sh.clone(), cv.clone());
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    let (sh3, cv3) = (sh2.clone(), cv2.clone());
                    std::thread::spawn(move || http_conn(s, sh3, cv3));
                }
            });
            sh.lock().unwrap().bound = Some(Ok(port));
            if let Some(b) = BOUND.get() {
                *b.lock().unwrap() = Some(Ok(port));
            }
            append_log(
                &sh,
                "sys",
                format!("webchat listening on http://127.0.0.1:{port}"),
            );
        }
        Err(e) => {
            sh.lock().unwrap().bound = Some(Err(e.to_string()));
            if let Some(b) = BOUND.get() {
                *b.lock().unwrap() = Some(Err(e.to_string()));
            }
            append_log(&sh, "sys", format!("bind :{port} failed: {e}"));
        }
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = v["method"].as_str().unwrap_or("");
        match v.get("id").cloned() {
            None if method == "plugin/shutdown" => return Ok(()),
            None => {}
            Some(id) => {
                // string ids = our own host/run replies
                if let Some(sid) = id.as_str()
                    && sid.starts_with("wc-")
                {
                    if let Some(res) = v.get("result") {
                        fulfill_waiters(&sh, sid, res);
                    } else if let Some(e) = v.get("error") {
                        append_log(
                            &sh,
                            "sys",
                            format!(
                                "host/run failed: {} (grant host.turn: `gray plugin capabilities webchat --all`)",
                                e["message"].as_str().unwrap_or("?")
                            ),
                        );
                    }
                    continue;
                }
                let reply = if method == "plugin/manifest" {
                    json!({"id": id, "result": manifest()})
                } else if method == "command/run" {
                    json!({"id": id, "result": dispatch("command/run", &v["params"])})
                } else {
                    json!({"id": id, "error": {"code": -32601,
                        "message": format!("method not found: {method}")}})
                };
                let mut w = out.lock().unwrap();
                writeln!(w, "{reply}")?;
                w.flush()?;
            }
        }
    }
    Ok(())
}

/// `gray-gateway webchat <args…>`: the former binary's verbs, with the
/// bare invocation still meaning "serve".
pub fn run(argv: &[String]) -> i32 {
    let args: Vec<String> = match argv.first().map(String::as_str) {
        Some("webchat") => argv[1..].to_vec(),
        _ => argv.to_vec(),
    };
    match args.first().map(String::as_str) {
        Some("manifest") => {
            println!("{}", manifest());
            0
        }
        Some("sidecar") | None => {
            if args.is_empty() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                println!(
                    "webchat — local chat at http://127.0.0.1:{} (lives while gray runs)",
                    DEFAULT_PORT
                );
                return 0;
            }
            let _ = serve();
            0
        }
        Some("-h" | "--help" | "help") => {
            println!(
                "webchat — local chat at http://127.0.0.1:{} (lives while gray runs)",
                DEFAULT_PORT
            );
            0
        }
        Some(other) => {
            eprintln!("unknown verb {other}");
            1
        }
    }
}

static BOUND: std::sync::OnceLock<Mutex<Option<Result<u16, String>>>> = std::sync::OnceLock::new();
static PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

/// Answer one protocol method the gateway routes to this channel.
pub fn dispatch(method: &str, _params: &Value) -> Value {
    if method == "plugin/manifest" {
        return manifest();
    }
    if method == "command/run" {
        let port = PORT.get().copied().unwrap_or(DEFAULT_PORT);
        let text = match BOUND.get().and_then(|b| b.lock().ok()?.clone()) {
            Some(Ok(p)) => format!("webchat at http://127.0.0.1:{p}"),
            Some(Err(e)) => format!("webchat not listening — bind 127.0.0.1:{port} failed: {e}"),
            None => format!("webchat starting on 127.0.0.1:{port}"),
        };
        return json!({"text": text});
    }
    json!({"error": {"code": -32601, "message": format!("method not found: {method}")}})
}
