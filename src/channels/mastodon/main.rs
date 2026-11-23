//! mastodon — post + read via the Mastodon REST API.
//! Config: ~/.gray/mastodon/config.json {"instance","token"} or env
//! MASTODON_INSTANCE / MASTODON_TOKEN.

use serde_json::{Value, json};

const NAME: &str = "mastodon";

pub fn manifest() -> Value {
    json!({
        "name": NAME, "display_name": "Mastodon",
        "version": env!("CARGO_PKG_VERSION"), "protocol": "1.1",
        "tools": [
            {"name": "mastodon_post", "label": "Posted", "preview": "{visibility}\n{text|firstline}",
             "description": "Post a status. visibility: public|unlisted|private|direct (default unlisted).",
             "parameters": {"type": "object", "properties": {
                "text": {"type": "string"}, "visibility": {"type": "string"},
                "reply_to": {"type": "string", "description": "status id to reply to"},
                "spoiler": {"type": "string"}}, "required": ["text"]}},
            {"name": "mastodon_notifications", "label": "Mastodon notifications",
             "description": "Recent notifications (mentions, boosts, favs, follows).",
             "parameters": {"type": "object", "properties": {
                "limit": {"type": "integer"}}}},
            {"name": "mastodon_timeline", "label": "Mastodon timeline", "preview": "{scope}",
             "description": "Read a timeline: home|local|public (default home).",
             "parameters": {"type": "object", "properties": {
                "scope": {"type": "string"}, "limit": {"type": "integer"}}}},
            {"name": "mastodon_search", "label": "Mastodon search", "preview": "{query}",
             "description": "Search statuses/accounts/hashtags.",
             "parameters": {"type": "object", "properties": {
                "query": {"type": "string"}, "limit": {"type": "integer"}},
             "required": ["query"]}},
        ],
        "commands": ["/mastodon"], "subcommands": ["mastodon"],
        "command_descriptions": {"/mastodon": "Mastodon bridge: post and read"},
    })
}

fn config_path() -> std::path::PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".gray")))
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|h| std::path::PathBuf::from(h).join(".gray"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join("mastodon").join("config.json")
}
fn read_config() -> Value {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}
fn cfg(k: &str, env: &str) -> Option<String> {
    std::env::var(env)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| read_config()[k].as_str().map(str::to_string))
}

fn api(method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let inst = cfg("instance", "MASTODON_INSTANCE")
        .ok_or("no instance — `mastodon setup --instance mastodon.social --token …`")?;
    let tok = cfg("token", "MASTODON_TOKEN").ok_or("no token configured")?;
    let url = format!("{}/api/v1{}", inst.trim_end_matches('/'), path);
    let req = match method {
        "GET" => ureq::get(&url),
        _ => ureq::post(&url),
    }
    .timeout(std::time::Duration::from_secs(20))
    .set("Authorization", &format!("Bearer {tok}"));
    let resp = match body {
        Some(b) => req.set("Content-Type", "application/json").send_json(b),
        None => req.call(),
    };
    match resp {
        Ok(r) => r.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(c, r)) => Err(format!(
            "HTTP {c}: {}",
            r.into_string()
                .unwrap_or_default()
                .chars()
                .take(250)
                .collect::<String>()
        )),
        Err(e) => Err(e.to_string()),
    }
}

fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut tag = false;
    for c in s.chars() {
        match c {
            '<' => tag = true,
            '>' => tag = false,
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fmt_status(s: &Value) -> String {
    format!(
        "@{} · {}\n  {}",
        s["account"]["acct"].as_str().unwrap_or("?"),
        s["created_at"].as_str().unwrap_or(""),
        strip_html(s["content"].as_str().unwrap_or(""))
            .chars()
            .take(200)
            .collect::<String>()
    )
}

fn tool(name: &str, args: &Value) -> Value {
    let r = match name {
        "mastodon_post" => {
            let mut b = json!({"status": args["text"].as_str().unwrap_or(""),
                "visibility": args["visibility"].as_str().unwrap_or("unlisted")});
            if let Some(r) = args["reply_to"].as_str() {
                b["in_reply_to_id"] = json!(r);
            }
            if let Some(s) = args["spoiler"].as_str() {
                b["spoiler_text"] = json!(s);
            }
            api("POST", "/statuses", Some(&b))
                .map(|v| format!("posted → {}", v["url"].as_str().unwrap_or("(no url)")))
        }
        "mastodon_notifications" => {
            let n = args["limit"].as_u64().unwrap_or(15).min(40);
            api("GET", &format!("/notifications?limit={n}"), None).map(|v| {
                v.as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|n| {
                        format!(
                            "[{}] @{} {}",
                            n["type"].as_str().unwrap_or("?"),
                            n["account"]["acct"].as_str().unwrap_or("?"),
                            n["status"]["content"]
                                .as_str()
                                .map(|c| strip_html(c).chars().take(120).collect::<String>())
                                .unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        "mastodon_timeline" => {
            let scope = args["scope"].as_str().unwrap_or("home");
            let n = args["limit"].as_u64().unwrap_or(15).min(40);
            let path = match scope {
                "local" => format!("/timelines/public?local=true&limit={n}"),
                "public" => format!("/timelines/public?limit={n}"),
                _ => format!("/timelines/home?limit={n}"),
            };
            api("GET", &path, None).map(|v| {
                v.as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(fmt_status)
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
        }
        "mastodon_search" => {
            let q = args["query"].as_str().unwrap_or("");
            let n = args["limit"].as_u64().unwrap_or(10).min(40);
            api(
                "GET",
                &format!(
                    "/search?q={}&limit={n}&resolve=true",
                    q.as_bytes()
                        .iter()
                        .map(|b| match b {
                            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' =>
                                (*b as char).to_string(),
                            _ => format!("%{b:02X}"),
                        })
                        .collect::<String>()
                ),
                None,
            )
            .map(|v| {
                v["statuses"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(fmt_status)
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
        }
        t => Err(format!("unknown tool {t}")),
    };
    match r {
        Ok(c) => json!({"result": {"content": if c.is_empty() {"(empty)".into()} else {c}}}),
        Err(e) => json!({"result": {"content": e, "is_error": true}}),
    }
}

pub fn cli(argv: &[String]) -> Result<String, String> {
    match argv.first().map(String::as_str) {
        Some("setup") => {
            let mut c = read_config();
            let mut it = argv[1..].iter();
            while let Some(f) = it.next() {
                match f.as_str() {
                    "--instance" => {
                        c["instance"] = json!(it.next().ok_or("--instance needs host")?)
                    }
                    "--token" => c["token"] = json!(it.next().ok_or("--token needs value")?),
                    o => return Err(format!("unknown flag {o}")),
                }
            }
            std::fs::create_dir_all(config_path().parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(config_path(), serde_json::to_string_pretty(&c).unwrap())
                .map_err(|e| e.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(config_path(), std::fs::Permissions::from_mode(0o600));
            }
            Ok("saved".into())
        }
        Some("status") | None => Ok(format!(
            "Mastodon\n  instance: {}\n  token: {}",
            cfg("instance", "MASTODON_INSTANCE").unwrap_or("MISSING".into()),
            if cfg("token", "MASTODON_TOKEN").is_some() {
                "set"
            } else {
                "MISSING"
            }
        )),
        Some("-h" | "--help" | "help") => Ok("usage: mastodon setup | status".into()),
        Some(o) => Err(format!("unknown verb {o}")),
    }
}

/// Answer one protocol method the gateway routes to this channel.
pub fn dispatch(method: &str, params: &Value) -> Value {
    if method == "plugin/manifest" {
        return manifest();
    }
    if method == "tool/call" {
        let name = params["name"].as_str().unwrap_or("");
        let mut answer = tool(name, &params["args"]);
        return answer["result"].take();
    }
    if method == "command/run" {
        let argv: Vec<String> = params["argv"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let text = cli(&argv).unwrap_or_else(|e| e);
        return json!({"text": text});
    }
    json!({"error": {"code": -32601, "message": format!("method not found: {method}")}})
}
