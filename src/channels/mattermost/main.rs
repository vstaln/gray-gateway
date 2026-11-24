//! mattermost — post + read via the Mattermost REST API (v4).
//! Config: ~/.gray/mattermost/config.json {"url","token","team"} or env
//! MATTERMOST_URL / MATTERMOST_TOKEN / MATTERMOST_TEAM.

use serde_json::{Value, json};
use std::collections::HashMap;

const NAME: &str = "mattermost";

pub fn manifest() -> Value {
    json!({
        "name": NAME, "display_name": "Mattermost",
        "version": env!("CARGO_PKG_VERSION"), "protocol": "1.1",
        "tools": [
            {"name": "mm_post",
             "description": "Post a message to a channel (name or id).",
             "parameters": {"type": "object", "properties": {
                "channel": {"type": "string", "description": "channel name or id"},
                "text": {"type": "string"}}, "required": ["channel", "text"]}},
            {"name": "mm_posts",
             "description": "Read recent posts from a channel (name or id).",
             "parameters": {"type": "object", "properties": {
                "channel": {"type": "string"}, "limit": {"type": "integer"}},
             "required": ["channel"]}},
            {"name": "mm_channels",
             "description": "List channels in the configured team.",
             "parameters": {"type": "object", "properties": {}}},
            {"name": "mm_users",
             "description": "List users in the configured team.",
             "parameters": {"type": "object", "properties": {
                "limit": {"type": "integer"}}}},
            {"name": "mm_react",
             "description": "Add an emoji reaction to a post.",
             "parameters": {"type": "object", "properties": {
                "post_id": {"type": "string"}, "emoji": {"type": "string",
                 "description": "emoji name, with or without colons"}},
             "required": ["post_id", "emoji"]}},
            {"name": "mm_search",
             "description": "Search posts in the configured team.",
             "parameters": {"type": "object", "properties": {
                "query": {"type": "string"}, "limit": {"type": "integer"}},
             "required": ["query"]}},
        ],
        "commands": ["/mattermost"], "subcommands": ["mattermost"],
        "command_descriptions": {"/mattermost": "set up Mattermost and check status"},
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
    home.join("mattermost").join("config.json")
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
    let base = cfg("url", "MATTERMOST_URL")
        .ok_or("no url — `mattermost setup --url https://mm.example.com --token … --team …`")?;
    let tok = cfg("token", "MATTERMOST_TOKEN").ok_or("no token configured")?;
    let url = format!("{}/api/v4{}", base.trim_end_matches('/'), path);
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

fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char)
        } else {
            o.push_str(&format!("%{b:02X}"))
        }
    }
    o
}

fn is_id(s: &str) -> bool {
    s.len() == 26 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn team_id() -> Result<String, String> {
    let team = cfg("team", "MATTERMOST_TEAM").ok_or("no team configured")?;
    if is_id(&team) {
        return Ok(team);
    }
    api("GET", &format!("/teams/name/{}", enc(&team)), None)?["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("team {team} not found"))
}

fn channel_id(channel: &str) -> Result<String, String> {
    let ch = channel.trim_start_matches('#');
    if ch.is_empty() {
        return Err("empty channel".into());
    }
    if is_id(ch) {
        return Ok(ch.into());
    }
    let team = cfg("team", "MATTERMOST_TEAM").ok_or("no team configured")?;
    api(
        "GET",
        &format!("/teams/name/{}/channels/name/{}", enc(&team), enc(ch)),
        None,
    )?["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("channel {ch} not found"))
}

fn usernames(ids: &[&str]) -> HashMap<String, String> {
    api("POST", "/users/ids", Some(&json!(ids)))
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|u| {
            u["id"]
                .as_str()
                .zip(u["username"].as_str())
                .map(|(i, n)| (i.to_string(), n.to_string()))
        })
        .collect()
}

fn fmt_posts(v: &Value) -> String {
    let posts = v["posts"].as_object().cloned().unwrap_or_default();
    let order = v["order"].as_array().cloned().unwrap_or_default();
    let mut ids: Vec<&str> = posts
        .values()
        .filter_map(|p| p["user_id"].as_str())
        .collect();
    ids.sort();
    ids.dedup();
    let names = usernames(&ids);
    order
        .iter()
        .filter_map(|i| posts.get(i.as_str().unwrap_or("")))
        .map(|p| {
            let u = p["user_id"].as_str().unwrap_or("");
            format!(
                "@{}  {}\n  {}",
                names.get(u).map(String::as_str).unwrap_or(u),
                p["id"].as_str().unwrap_or(""),
                p["message"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(300)
                    .collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn tool(name: &str, args: &Value) -> Value {
    let r = match name {
        "mm_post" => {
            let ch = args["channel"].as_str().unwrap_or("");
            let text = args["text"].as_str().unwrap_or("");
            channel_id(ch)
                .and_then(|id| {
                    api(
                        "POST",
                        "/posts",
                        Some(&json!({"channel_id": id, "message": text})),
                    )
                })
                .map(|v| format!("posted → {}", v["id"].as_str().unwrap_or("(no id)")))
        }
        "mm_posts" => {
            let ch = args["channel"].as_str().unwrap_or("");
            let n = args["limit"].as_u64().unwrap_or(15).min(60);
            channel_id(ch)
                .and_then(|id| api("GET", &format!("/channels/{id}/posts?per_page={n}"), None))
                .map(|v| fmt_posts(&v))
        }
        "mm_channels" => team_id().and_then(|t| {
            api(
                "GET",
                &format!("/users/me/teams/{t}/channels?per_page=200"),
                None,
            )
            .map(|v| {
                v.as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|c| {
                        format!(
                            "{}{}  {}  {}",
                            match c["type"].as_str().unwrap_or("") {
                                "D" | "G" => "@",
                                _ => "#",
                            },
                            c["name"].as_str().unwrap_or("?"),
                            c["display_name"].as_str().unwrap_or(""),
                            c["id"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }),
        "mm_users" => {
            let n = args["limit"].as_u64().unwrap_or(30).min(200);
            team_id().and_then(|t| {
                api("GET", &format!("/users?in_team={t}&per_page={n}"), None).map(|v| {
                    v.as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|u| {
                            format!(
                                "@{}  {}  {}",
                                u["username"].as_str().unwrap_or("?"),
                                u["nickname"].as_str().unwrap_or(""),
                                u["id"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            })
        }
        "mm_react" => {
            let post = args["post_id"].as_str().unwrap_or("");
            let emoji = args["emoji"].as_str().unwrap_or("").trim_matches(':');
            api("GET", "/users/me", None)
                .and_then(|me| {
                    let uid = me["id"].as_str().ok_or("no user id".to_string())?;
                    api(
                        "POST",
                        "/reactions",
                        Some(&json!({
                    "user_id": uid, "post_id": post, "emoji_name": emoji})),
                    )
                })
                .map(|_| format!("reacted :{emoji}: → {post}"))
        }
        "mm_search" => {
            let q = args["query"].as_str().unwrap_or("");
            let n = args["limit"].as_u64().unwrap_or(20).min(60);
            team_id()
                .and_then(|t| {
                    api(
                        "POST",
                        &format!("/teams/{t}/posts/search"),
                        Some(&json!({
                    "terms": q, "is_or_search": false, "per_page": n})),
                    )
                })
                .map(|v| fmt_posts(&v))
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
                    "--url" => c["url"] = json!(it.next().ok_or("--url needs value")?),
                    "--token" => c["token"] = json!(it.next().ok_or("--token needs value")?),
                    "--team" => c["team"] = json!(it.next().ok_or("--team needs value")?),
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
            "Mattermost\n  url: {}\n  team: {}\n  token: {}",
            cfg("url", "MATTERMOST_URL").unwrap_or("MISSING".into()),
            cfg("team", "MATTERMOST_TEAM").unwrap_or("MISSING".into()),
            if cfg("token", "MATTERMOST_TOKEN").is_some() {
                "set"
            } else {
                "MISSING"
            }
        )),
        Some(o) => Err(format!("unknown verb {o}")),
    }
}

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
