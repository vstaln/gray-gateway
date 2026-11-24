#![allow(dead_code)]

pub mod control;
pub mod file_trigger;
pub mod send_message;
pub mod webhooks;

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use serde_json::{Value, json};

use crate::parts::Parts;

static INSTALLED: OnceLock<Arc<Inbound>> = OnceLock::new();

static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

pub struct Inbound {
    wh: Arc<webhooks::Shared>,
    ctl: Arc<control::Shared>,
    ft: file_trigger::State,
}

impl Inbound {
    fn new(out: Arc<Mutex<std::io::Stdout>>) -> Arc<Self> {
        let mut wh = webhooks::Shared::new();
        wh.out = out.clone();
        let mut ctl = control::Shared::new();
        ctl.out = out.clone();
        ctl.counter = wh.counter.clone();
        let ft = file_trigger::State {
            stash: Mutex::new(HashMap::new()),
            rules: Mutex::new(file_trigger::load_rules()),
        };
        Arc::new(Self {
            wh: Arc::new(wh),
            ctl: Arc::new(ctl),
            ft,
        })
    }

    fn send_message(&self, req: &Value) -> (Option<Value>, bool) {
        let wh = &self.wh;
        let mut say = |t: &str| {
            if SHUTTING_DOWN.load(Ordering::SeqCst) {
                return Ok(());
            }
            send_message::host_call(
                &wh.out,
                &wh.pending,
                &wh.counter,
                "host/say",
                json!({ "text": t }),
            )
            .map(|_| ())
        };
        send_message::handle_send_message(req, &mut say)
    }

    fn touch_session(&self, params: &Value) {
        if params.get("session").is_some() {
            control::note_session(&self.ctl, params);
            control::ensure_listener(&self.ctl);
        }
    }

    fn before_start(&self, parts: &Parts, req: &Value) -> Value {
        let mut replies = Vec::new();
        if parts.on("send-message") {
            replies.push(self.send_message(req).0);
        }
        if parts.on("webhooks") {
            replies.push(webhooks::handle_webhooks(&self.wh, req).0);
        }
        if parts.on("control") {
            replies.push(control::handle_control(&self.ctl, req).0);
        }
        let blocks: Vec<String> = replies
            .into_iter()
            .filter_map(|r| {
                let t = r?
                    .get("result")?
                    .get("text")?
                    .as_str()?
                    .trim_end()
                    .to_string();
                (!t.is_empty()).then_some(t)
            })
            .collect();
        if blocks.is_empty() {
            json!({})
        } else {
            json!({ "text": blocks.join("\n\n") })
        }
    }

    fn tool_event(&self, parts: &Parts, req: &Value) -> Value {
        if !parts.on("file-trigger") {
            return match req.get("method").and_then(Value::as_str) {
                Some("tool/before") => json!({"decision": "allow"}),
                _ => json!({}),
            };
        }
        file_trigger::handle_file_trigger(&self.ft, req)
            .0
            .unwrap_or_else(|| json!({}))
    }

    fn call_tool(&self, parts: &Parts, req: &Value) -> Option<Value> {
        let name = req
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or("");
        match name {
            "webhook_emit" if parts.on("webhooks") => {
                Some(result_of(webhooks::handle_webhooks(&self.wh, req).0))
            }
            "send_to_session" if parts.on("control") => {
                Some(result_of(control::handle_control(&self.ctl, req).0))
            }
            _ => None,
        }
    }

    fn run_command(&self, parts: &Parts, req: &Value) -> Option<Value> {
        let name = req
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_start_matches('/');
        match name {
            "tell" if parts.on("send-message") => Some(result_of(self.send_message(req).0)),
            "webhook" if parts.on("webhooks") => {
                Some(result_of(webhooks::handle_webhooks(&self.wh, req).0))
            }
            "trigger" if parts.on("file-trigger") => Some(result_of(
                file_trigger::handle_file_trigger(&self.ft, req).0,
            )),
            "control" if parts.on("control") => {
                Some(result_of(control::handle_control(&self.ctl, req).0))
            }
            _ => None,
        }
    }

    fn stop(&self) {
        self.wh.stop.store(true, Ordering::SeqCst);
        self.ctl.stop.store(true, Ordering::SeqCst);
        if let Some(sock) = self.ctl.session.lock().expect("session").sock.take() {
            let _ = std::fs::remove_file(sock);
        }
    }
}

fn result_of(reply: Option<Value>) -> Value {
    reply
        .and_then(|r| r.get("result").cloned())
        .unwrap_or_else(|| json!({}))
}

pub fn part_manifest(name: &str) -> Value {
    match name {
        "send-message" => send_message::manifest_send_message(),
        "webhooks" => webhooks::manifest_webhooks(),
        "file-trigger" => file_trigger::manifest_file_trigger(),
        "control" => control::manifest_control(),
        _ => Value::Null,
    }
}

pub fn command_descriptions(parts: &Parts) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    if parts.on("send-message") {
        out.push(("/tell", "queue a message for the next turn"));
    }
    if parts.on("webhooks") {
        out.push(("/webhook", "add and inspect inbound webhooks"));
    }
    if parts.on("file-trigger") {
        out.push(("/trigger", "list or reload per-path reminder rules"));
    }
    if parts.on("control") {
        out.push(("/control", "show and manage the remote-control socket"));
    }
    out
}

pub fn is_part(name: &str) -> bool {
    matches!(
        name,
        "send-message" | "webhooks" | "file-trigger" | "control"
    )
}

pub fn any_on(parts: &Parts) -> bool {
    ["send-message", "webhooks", "file-trigger", "control"]
        .iter()
        .any(|p| parts.on(p))
}

pub fn owns_hook(parts: &Parts, method: &str) -> bool {
    match method {
        "agent/before_start" => {
            parts.on("send-message") || parts.on("webhooks") || parts.on("control")
        }
        "tool/before" | "tool/after" => parts.on("file-trigger"),
        _ => false,
    }
}

pub fn owns_tool(parts: &Parts, name: &str) -> bool {
    match name {
        "webhook_emit" => parts.on("webhooks"),
        "send_to_session" => parts.on("control"),
        _ => false,
    }
}

pub fn owns_command(parts: &Parts, name: &str) -> bool {
    match name.trim_start_matches('/') {
        "tell" => parts.on("send-message"),
        "webhook" => parts.on("webhooks"),
        "trigger" => parts.on("file-trigger"),
        "control" => parts.on("control"),
        _ => false,
    }
}

pub struct Worker {
    tx: mpsc::Sender<Value>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    pub fn sender(&self) -> mpsc::Sender<Value> {
        self.tx.clone()
    }
}

pub fn install(parts: &Parts, out: Arc<Mutex<std::io::Stdout>>) -> Option<Worker> {
    if !any_on(parts) {
        return None;
    }
    let st = Inbound::new(out.clone());
    let _ = INSTALLED.set(st.clone());
    let (tx, rx) = mpsc::channel::<Value>();
    let parts = parts.clone();
    let join = std::thread::spawn(move || {
        while let Ok(req) = rx.recv() {
            let Some(st) = INSTALLED.get() else { break };
            let Some(reply) = handle(st, &parts, &req) else {
                continue;
            };
            write_frame(&out, &reply);
        }
    });
    Some(Worker {
        tx,
        join: Some(join),
    })
}

impl Drop for Worker {
    fn drop(&mut self) {
        SHUTTING_DOWN.store(true, Ordering::SeqCst);
        let (dead, _) = mpsc::channel::<Value>();
        let _ = std::mem::replace(&mut self.tx, dead);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn handle(st: &Inbound, parts: &Parts, req: &Value) -> Option<Value> {
    let id = req.get("id")?.clone();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    if parts.on("control")
        && let Some(params) = req.get("params")
    {
        st.touch_session(params);
    }
    let result = match method {
        "agent/before_start" => st.before_start(parts, req),
        "tool/before" | "tool/after" => st.tool_event(parts, req),
        "tool/call" => st.call_tool(parts, req)?,
        "command/run" => st.run_command(parts, req)?,
        _ => return None,
    };
    Some(json!({ "id": id, "result": result }))
}

pub fn route_reply(frame: &Value) -> bool {
    let Some(st) = INSTALLED.get() else {
        return false;
    };
    let Some(id) = frame.get("id").and_then(Value::as_str) else {
        return false;
    };
    if let Some(tx) = st.wh.pending.lock().expect("pending").remove(id) {
        let _ = tx.send(frame.clone());
    }
    true
}

pub fn shutdown() {
    if let Some(st) = INSTALLED.get() {
        st.stop();
    }
}

pub fn write_frame(out: &Arc<Mutex<std::io::Stdout>>, frame: &Value) {
    let mut o = out.lock().expect("stdout");
    let _ = writeln!(o, "{frame}");
    let _ = o.flush();
}
