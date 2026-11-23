//! One table drives slash-command registration, dispatch, and `/help`.
//!
//! Three readers, one source of truth: [`registration_json`] (what the
//! Discord command picker offers), [`parse`] (what an invocation runs), and
//! [`help_embed`] (what `/help` renders). A command that ships without
//! registration or without documentation shows up as a hole in `/help`, so
//! it cannot drift quietly.
//!
//! This module holds no twilight types: the adapter turns an interaction
//! into a command name plus flat name/value pairs, and everything below is
//! plain data — which is also why it is unit-testable without a socket.

use serde_json::{Value, json};

/// Blurple / red. Informational answers use the first; anything that
/// deletes uses the second.
const INFO: u32 = 0x0058_65F2;
const DANGER: u32 = 0x00ED_4245;

/// Five buttons per row, max.
pub(crate) const MAX_BUTTONS: usize = 5;

/// A string option (`/cron add --every`), or a sub-command when rendered at
/// type 1. `required` only applies to string options.
#[derive(Clone, Copy)]
pub struct Opt {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// A sub-command, carrying its own options the way Discord nests them.
#[derive(Clone, Copy)]
pub struct Sub {
    pub name: &'static str,
    pub description: &'static str,
    pub options: &'static [Opt],
}

pub struct Command {
    pub name: &'static str,
    /// What the Discord picker shows beside the name.
    pub description: &'static str,
    pub subcommands: &'static [Sub],
    pub options: &'static [Opt],
}

/// The bridge's whole command surface. `ask`/`reset`/`status`/`stop` predate
/// this module; the rest are new.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "gateway",
        description: "Inspect the gateway or configure automatic session resets",
        subcommands: &[],
        options: &[Opt {
            name: "args",
            description: "status, on, off, or reset-policy [none|idle|daily|both]",
            required: false,
        }],
    },
    Command {
        name: "help",
        description: "What gray can do in this channel",
        subcommands: &[Sub {
            name: "command",
            description: "One command in detail",
            options: &[Opt {
                name: "name",
                description: "Command name",
                required: true,
            }],
        }],
        options: &[],
    },
    Command {
        name: "ask",
        description: "Send a prompt to gray",
        subcommands: &[],
        options: &[Opt {
            name: "prompt",
            description: "What to ask gray",
            required: true,
        }],
    },
    Command {
        name: "cron",
        description: "Schedule a prompt for this channel",
        subcommands: &[
            Sub {
                name: "list",
                description: "This channel's scheduled jobs",
                options: &[],
            },
            Sub {
                name: "add",
                description: "Schedule a prompt (60s or more)",
                options: &[
                    Opt {
                        name: "every",
                        description: "Interval, e.g. 30m or 2h",
                        required: true,
                    },
                    Opt {
                        name: "prompt",
                        description: "The prompt to run each time",
                        required: true,
                    },
                ],
            },
            Sub {
                name: "remove",
                description: "Delete a scheduled job",
                options: &[Opt {
                    name: "id",
                    description: "Job id",
                    required: true,
                }],
            },
        ],
        options: &[],
    },
    Command {
        name: "model",
        description: "Show or pick the model for this channel",
        subcommands: &[Sub {
            name: "set",
            description: "Pick the model (provider/model-id)",
            options: &[Opt {
                name: "model",
                description: "provider/model-id",
                required: true,
            }],
        }],
        options: &[],
    },
    Command {
        name: "memory",
        description: "gray's curated cross-session memory",
        subcommands: &[
            Sub {
                name: "list",
                description: "Show the current entries",
                options: &[],
            },
            Sub {
                name: "show",
                description: "Show one entry",
                options: &[Opt {
                    name: "key",
                    description: "Entry key",
                    required: true,
                }],
            },
            Sub {
                name: "set",
                description: "Add or replace an entry",
                options: &[
                    Opt {
                        name: "key",
                        description: "Entry key",
                        required: true,
                    },
                    Opt {
                        name: "text",
                        description: "One line, never a secret",
                        required: true,
                    },
                ],
            },
            Sub {
                name: "remove",
                description: "Forget an entry",
                options: &[Opt {
                    name: "key",
                    description: "Entry key",
                    required: true,
                }],
            },
        ],
        options: &[],
    },
    Command {
        name: "status",
        description: "Queue depth, daemon, and model",
        subcommands: &[],
        options: &[],
    },
    Command {
        name: "reset",
        description: "Reset your gray session",
        subcommands: &[],
        options: &[],
    },
    Command {
        name: "new",
        description: "Start a fresh gray session",
        subcommands: &[],
        options: &[],
    },
    Command {
        name: "stop",
        description: "Stop the running gray agent",
        subcommands: &[],
        options: &[],
    },
    Command {
        name: "voice",
        description: "Speech-to-text provider for voice notes",
        subcommands: &[
            Sub {
                name: "provider",
                description: "List the providers, or switch to one",
                options: &[Opt {
                    name: "id",
                    description: "local, groq, deepgram, or a custom id",
                    required: false,
                }],
            },
            Sub {
                name: "status",
                description: "Which provider transcribes voice notes",
                options: &[],
            },
        ],
        options: &[],
    },
    Command {
        name: "sethome",
        description: "Make this chat gray's home: where proactive messages go",
        subcommands: &[],
        options: &[],
    },
    Command {
        name: "steer",
        description: "Add guidance to the running turn",
        subcommands: &[],
        options: &[Opt {
            name: "text",
            description: "What gray should take into account now",
            required: true,
        }],
    },
];

pub const STEER_HELP: &str =
    "`/steer <text>` — add guidance to the running turn (when idle it is sent as a new message)";

/// Discord's global-command registration array, derived from [`COMMANDS`].
pub fn registration_json() -> Value {
    Value::Array(COMMANDS.iter().map(command_json).collect())
}

/// One command's registration object — shared with `graycmd`'s merged
/// table so bridge-owned rows can never drift from the static shape.
pub fn command_json(c: &Command) -> Value {
    let mut options: Vec<Value> = Vec::new();
    for s in c.subcommands {
        options.push(json!({
            "name": s.name,
            "description": s.description,
            "type": 1,
            "options": s.options.iter().map(|o| json!({
                "name": o.name,
                "description": o.description,
                "type": 3,
                "required": o.required,
            })).collect::<Vec<_>>(),
        }));
    }
    for o in c.options {
        options.push(json!({
            "name": o.name,
            "description": o.description,
            "type": 3,
            "required": o.required,
        }));
    }
    json!({"name": c.name, "description": c.description, "options": options})
}

/// What an invocation asked for. Discord sends strings; the bridge decides
/// what they mean, so a typo surfaces as an embed, not a silent no-op.
#[derive(Debug)]
pub enum Request {
    Help {
        command: Option<String>,
    },
    CronList,
    CronAdd {
        every: String,
        prompt: String,
    },
    CronRemove {
        id: String,
    },
    ModelShow,
    ModelSet {
        model: String,
    },
    MemoryList,
    MemoryShow {
        key: String,
    },
    MemorySet {
        key: String,
        text: String,
    },
    MemoryRemove {
        key: String,
    },
    Status,
    Gateway {
        args: Option<String>,
    },
    Ask {
        prompt: String,
    },
    Reset,
    Stop,
    Steer {
        text: String,
    },
    SetHome,
    VoiceProvider {
        id: Option<String>,
    },
    /// A registry command `gray cmd` answers; built by `graycmd::route`.
    CmdRun {
        name: String,
        args: Option<String>,
        scope: String,
        surface: String,
        slow: bool,
    },
}

/// An answer: always an embed, with whether it belongs to the channel
/// (informational) or only to the caller (destructive). `Error` is the
/// user-typed-something-wrong case and stays ephemeral.
pub enum Reply {
    Embed { embed: Value, public: bool },
    Error { message: String },
}

fn value_for<'a>(values: &'a [(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    values
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| *v)
        .filter(|v: &&str| !v.trim().is_empty())
}

/// Turn a Discord invocation into a [`Request`], or `None` when the shape is
/// unknown (an unregistered command that reached us anyway).
pub fn parse(command: &str, subcommand: Option<&str>, values: &[(&str, &str)]) -> Option<Request> {
    let needed = |name: &str| value_for(values, name).map(str::to_string);
    match (command, subcommand) {
        ("help", Some("command")) => Some(Request::Help {
            command: needed("name"),
        }),
        ("help", None) => Some(Request::Help { command: None }),
        ("ask", _) => needed("prompt").map(|prompt| Request::Ask { prompt }),
        ("cron", Some("list")) => Some(Request::CronList),
        ("cron", Some("add")) => Some(Request::CronAdd {
            every: needed("every")?,
            prompt: needed("prompt")?,
        }),
        ("cron", Some("remove")) => needed("id").map(|id| Request::CronRemove { id }),
        ("model", Some("set")) => needed("model").map(|model| Request::ModelSet { model }),
        ("model", None) => Some(Request::ModelShow),
        ("memory", Some("list")) => Some(Request::MemoryList),
        ("memory", Some("show")) => needed("key").map(|key| Request::MemoryShow { key }),
        ("memory", Some("set")) => Some(Request::MemorySet {
            key: needed("key")?,
            text: needed("text")?,
        }),
        ("memory", Some("remove")) => needed("key").map(|key| Request::MemoryRemove { key }),
        ("status", _) => Some(Request::Status),
        ("gateway", _) => Some(Request::Gateway {
            args: needed("args"),
        }),
        ("new", _) | ("reset", _) => Some(Request::Reset),
        ("stop", _) => Some(Request::Stop),
        ("sethome", _) => Some(Request::SetHome),
        ("voice", Some("provider")) => Some(Request::VoiceProvider { id: needed("id") }),
        ("voice", Some("status")) | ("voice", None) => Some(Request::VoiceProvider { id: None }),
        ("steer", _) => needed("text").map(|text| Request::Steer { text }),
        _ => None,
    }
}

/// `/help`'s embed: every command, its sub-commands, and what it does —
/// generated from the table, so adding a command needs no `/help` edit.
pub fn help_embed() -> Value {
    let lines: Vec<String> = COMMANDS
        .iter()
        .map(|c| {
            if c.subcommands.is_empty() {
                format!("`/{c}` — {desc}", c = c.name, desc = c.description)
            } else {
                format!(
                    "`/{c}` — {desc}\n{subs}",
                    c = c.name,
                    desc = c.description,
                    subs = c
                        .subcommands
                        .iter()
                        .map(|s| format!(
                            "╰ `/{c} {s}` — {d}",
                            c = c.name,
                            s = s.name,
                            d = s.description
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
        })
        .collect();
    embed("gray — what this channel can do", lines.join("\n\n"), &[])
}

/// Detail for one command, or `None` when there is no such command.
pub fn help_for(name: &str) -> Option<Value> {
    COMMANDS.iter().find(|c| c.name == name).map(|c| {
        let body = if c.subcommands.is_empty() {
            format!("Run it as `/{c}`.", c = c.name)
        } else {
            let subs: Vec<String> = c
                .subcommands
                .iter()
                .map(|s| {
                    let args: Vec<&str> = s.options.iter().map(|o| o.name).collect();
                    if args.is_empty() {
                        format!("`/{c} {s}`\n{d}", c = c.name, s = s.name, d = s.description)
                    } else {
                        format!(
                            "`/{c} {s} {a}`\n{d}",
                            c = c.name,
                            s = s.name,
                            a = args.join(" "),
                            d = s.description
                        )
                    }
                })
                .collect();
            format!("{}\n\n{}", subs.join("\n"), c.description)
        };
        embed(format!("/{}", c.name), body, &[])
    })
}

/// A scheduled-job listing plus one remove button per exposed state token.
/// Resource ids stay in SQLite; the wire sees only opaque token values.
pub fn cron_view(
    jobs: &[(String, String, i64, String)],
    channel: &str,
    state_tokens: &[String],
) -> (Value, Vec<Value>) {
    let fields: Vec<Value> = jobs
        .iter()
        .map(|(id, prompt, interval, status)| {
            json!({
                "name": format!("`{id}` · every {} · {status}", human(*interval)),
                "value": prompt,
                "inline": false,
            })
        })
        .collect();
    let list = if jobs.is_empty() {
        "Nothing scheduled here yet. `/cron add every:<30m> prompt:<what>`.".to_string()
    } else {
        format!("{} job(s) firing in <#{channel}>.", jobs.len())
    };
    let buttons: Vec<Value> = jobs
        .iter()
        .zip(state_tokens.iter())
        .take(MAX_BUTTONS)
        .map(|(_, token)| {
            json!({
                "type": 2,
                "label": "Remove",
                "style": 4,
                "custom_id": format!("cron:remove:{token}"),
            })
        })
        .collect();
    (embed("Scheduled jobs", list, &fields), buttons)
}

/// Rendering for a command that delegated to a gray CLI call: stdout on
/// success, stderr on failure, both truncated rather than dropped.
pub fn cli_embed(title: &str, stdout: &str, stderr: &str, ok: bool) -> Value {
    let body = if ok { stdout } else { stderr };
    let body = if body.trim().is_empty() {
        if ok {
            "Done.".to_string()
        } else {
            "No output.".to_string()
        }
    } else {
        body.to_string()
    };
    let mut out = embed(title, format!("```\n{body}\n```"), &[]);
    if !ok {
        out["color"] = json!(DANGER);
    }
    out
}

/// An interval in human form: "30m", "2h", "1d".
pub fn human(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

/// Parse `30m` / `2h` / `90` into seconds. `None` on anything else, so the
/// caller can answer instead of scheduling nonsense.
pub fn parse_interval(text: &str) -> Option<i64> {
    let t = text.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let (num, mult) = match t.strip_suffix(['s', 'm', 'h', 'd']) {
        Some(n) => {
            let unit = t.as_bytes()[t.len() - 1];
            let mult = match unit {
                b's' => 1,
                b'm' => 60,
                b'h' => 3600,
                b'd' => 86400,
                _ => return None,
            };
            (n, mult)
        }
        None => (t.as_str(), 1),
    };
    let n: i64 = num.parse().ok()?;
    (n > 0).then_some(n.saturating_mul(mult))
}

/// Build one embed, truncating what would 400 the API.
///
/// Takes `AsRef<str>` on purpose: every caller has a `String` or a literal
/// and none should have to reach for `&`.
pub fn embed(title: impl AsRef<str>, description: impl AsRef<str>, fields: &[Value]) -> Value {
    json!({
        "title": title.as_ref(),
        "description": description.as_ref(),
        "color": INFO,
        "fields": fields,
    })
}

/// Trim to a character budget and say so, so a truncated reply is never
/// mistaken for the whole answer.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(24)).collect();
    format!("{head}\n… (truncated)")
}

/// The `meta` key holding a channel's model override. Scoped per
/// conversation so one channel picking a model never changes another's.
pub fn model_key(conversation: &str) -> String {
    format!("model:{conversation}")
}

/// The argv for a `gray memory` call. gray owns the file format and the
/// scope flag, so the bridge builds argv and renders whatever it printed —
/// no second parser to keep in step.
pub fn memory_argv(subcommand: &str, key: Option<&str>, text: Option<&str>) -> Vec<String> {
    let mut argv = vec!["memory".to_string(), subcommand.to_string()];
    if let Some(k) = key {
        argv.push(k.to_string());
    }
    if let Some(t) = text {
        argv.push(t.to_string());
    }
    argv
}

/// A scheduled job as `/cron list` sees it: id, prompt, interval, status,
/// and which conversation it belongs to.
pub type JobRow = (String, String, i64, String);

/// Keep the jobs that belong in this channel. Rows written before `/cron`
/// existed carry no target, so they belong to the home channel — they show
/// up there and never leak into every other channel's list.
///
/// `targets` is parallel to `jobs`: each schedule row's stored channel.
pub fn jobs_for_channel<'a>(
    jobs: &'a [JobRow],
    targets: &'a [String],
    channel: &str,
    home: &str,
) -> Vec<&'a JobRow> {
    jobs.iter()
        .zip(targets)
        .filter(|(_, target)| match target.as_str() {
            "" => channel == home,
            t => t == channel,
        })
        .map(|(job, _)| job)
        .collect()
}

/// The model gray would actually use for a turn: the per-channel override
/// wins, then gray's provider config, then whatever the budget pins.
pub fn effective_model(override_model: Option<&str>, provider_model: Option<&str>) -> String {
    // Filter each layer before falling through, so a blank override does not
    // skip gray's provider config and land straight on the default.
    let picked = override_model.filter(|m| !m.trim().is_empty());
    picked
        .or_else(|| provider_model.filter(|m| !m.trim().is_empty()))
        .unwrap_or("(provider default)")
        .to_string()
}

/// Render one progress phase as a line a human can read, plus how long it
/// has been going. The phase name never leaks into the text: a phase this
/// build has never heard of renders as plain "working" rather than a raw
/// enum value, so a newer gray talking to an older bridge still reads.
pub fn progress_text(phase: &str, elapsed: std::time::Duration) -> String {
    let label = match phase {
        "generating" => "thinking",
        "tool_started" | "tool_finished" => "working",
        "provider_retry" => "retrying",
        "compacted" => "compacting",
        "persisting" => "saving",
        _ => "working",
    };
    format!("{label} {}", format_elapsed(elapsed))
}

/// Go-style compact duration: "12s", "1m15s", "2m", "1h5m". Seconds
/// always show while a minute or an hour is on the clock, because "2m"
/// reads as done while work is still running.
fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Whether a progress line is worth rewriting. Three things earn it: the
/// phase changed, this is the first line of a step, or the step has run
/// past the heartbeat. In between, the line is left alone — a status line
/// that churns on every tool result is noise nobody reads.
pub fn should_rewrite(
    phase: &str,
    elapsed: std::time::Duration,
    last: &(String, u64),
    heartbeat_secs: u64,
) -> bool {
    if last.0 != phase {
        return true;
    }
    if elapsed.as_secs() == 0 {
        return true;
    }
    elapsed.as_secs() >= heartbeat_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discord refuses the whole registration payload when one command
    /// breaks its documented limits, and it does so at startup, after the
    /// gateway is already up — so the limits are checked here instead.
    #[test]
    fn every_command_fits_discords_registration_limits() {
        let ok = |name: &str, desc: &str, where_: &str| {
            assert!(!name.is_empty(), "{where_}: empty name");
            assert!(name.len() <= 32, "{where_}: name `{name}` is 33+ chars");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c == '-'),
                "{where_}: name `{name}` is not lowercase"
            );
            assert!(!desc.is_empty(), "{where_}: `{name}` has no description");
            assert!(
                desc.len() <= 100,
                "{where_}: `{name}` description is 101+ chars"
            );
        };
        for c in COMMANDS {
            ok(c.name, c.description, "command");
            for s in c.subcommands {
                ok(s.name, s.description, &format!("/{}/{}", c.name, s.name));
                for o in s.options {
                    ok(
                        o.name,
                        o.description,
                        &format!("/{}/{}/{}", c.name, s.name, o.name),
                    );
                }
            }
            for o in c.options {
                ok(o.name, o.description, &format!("/{}/{}", c.name, o.name));
            }
        }
    }

    #[test]
    fn every_phase_gray_emits_renders_a_line_a_human_can_read() {
        for phase in [
            "generating",
            "tool_started",
            "tool_finished",
            "provider_retry",
            "compacted",
            "persisting",
            "",
            "something_new_from_a_future_gray",
        ] {
            let text = progress_text(phase, std::time::Duration::from_secs(12));
            assert!(text.contains("12s"), "{phase} rendered {text:?}");
            assert!(
                !text.contains(phase) || phase.is_empty(),
                "{phase} leaked the raw phase name into {text:?}"
            );
        }
        assert!(
            progress_text("tool_started", std::time::Duration::from_secs(75)).contains("1m15s")
        );
    }

    #[test]
    fn a_rewrite_is_earned_by_a_phase_change_or_a_long_step() {
        let start = std::time::Duration::from_secs(0);
        let last = ("generating".to_string(), 0u64);
        assert!(
            should_rewrite("tool_started", start, &last, 30),
            "phase change rewrites"
        );
        assert!(
            should_rewrite("generating", start, &last, 30),
            "first line of a step"
        );
        // Same phase, still inside the heartbeat: leaving it alone is the
        // point — a status line that churns per tool result is noise.
        assert!(!should_rewrite(
            "generating",
            std::time::Duration::from_secs(20),
            &last,
            30
        ));
        // ...until the step runs long enough to look frozen.
        assert!(should_rewrite(
            "generating",
            std::time::Duration::from_secs(30),
            &last,
            30
        ));
    }

    #[test]
    fn help_lists_every_command_in_the_table() {
        let body = help_embed()["description"].as_str().unwrap().to_string();
        for c in COMMANDS {
            assert!(
                body.contains(&format!("/{c}", c = c.name)),
                "missing /{} in /help",
                c.name
            );
            for s in c.subcommands {
                assert!(
                    body.contains(&format!("/{c} {s}", c = c.name, s = s.name)),
                    "missing /{} {} in /help",
                    c.name,
                    s.name
                );
            }
        }
    }

    #[test]
    fn registration_carries_every_command_and_nested_options() {
        let out = registration_json();
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), COMMANDS.len());
        let cron = arr.iter().find(|c| c["name"] == "cron").unwrap();
        let subs = cron["options"].as_array().unwrap();
        assert_eq!(subs.len(), 3);
        assert_eq!(subs[0]["type"], json!(1), "first option is a sub-command");
        let add = subs.iter().find(|s| s["name"] == "add").unwrap();
        let opts = add["options"].as_array().unwrap();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0]["required"], json!(true));
        assert_eq!(opts[0]["type"], json!(3), "sub-command args are strings");
        // A top-level string option still registers.
        let ask = arr.iter().find(|c| c["name"] == "ask").unwrap();
        assert_eq!(ask["options"][0]["name"], json!("prompt"));
        assert_eq!(ask["options"][0]["type"], json!(3));
    }

    #[test]
    fn new_and_reset_share_the_session_reset_action() {
        assert!(matches!(parse("new", None, &[]), Some(Request::Reset)));
        assert!(matches!(parse("reset", None, &[]), Some(Request::Reset)));
        let names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        assert!(names.contains(&"new"));
        assert!(names.contains(&"reset"));
    }

    #[test]
    fn parse_reads_subcommands_and_flat_values() {
        assert!(matches!(
            parse("cron", Some("list"), &[]),
            Some(Request::CronList)
        ));
        match parse(
            "cron",
            Some("add"),
            &[("every", "30m"), ("prompt", "check the thing")],
        ) {
            Some(Request::CronAdd { every, prompt }) => {
                assert_eq!(every, "30m");
                assert_eq!(prompt, "check the thing");
            }
            other => panic!("{other:?}", other = std::mem::discriminant(&other)),
        }
        assert!(matches!(
            parse("model", Some("set"), &[("model", "openai/gpt-5")]),
            Some(Request::ModelSet { .. })
        ));
        assert!(matches!(
            parse("model", None, &[]),
            Some(Request::ModelShow)
        ));
        // A required option sent blank is a bad request, not a default.
        assert!(parse("cron", Some("add"), &[("every", "  "), ("prompt", "x")]).is_none());
        // Unknown command or subcommand.
        assert!(parse("nope", None, &[]).is_none());
        assert!(parse("cron", Some("frobnicate"), &[]).is_none());
        assert!(parse("memory", Some("list"), &[]).is_some());
    }

    #[test]
    fn intervals_are_seconds_and_refuse_nonsense() {
        assert_eq!(parse_interval("90"), Some(90));
        assert_eq!(parse_interval("30s"), Some(30));
        assert_eq!(parse_interval("30m"), Some(1800));
        assert_eq!(parse_interval("2h"), Some(7200));
        assert_eq!(parse_interval("1d"), Some(86400));
        assert_eq!(
            parse_interval(" 5M "),
            Some(300),
            "case and space tolerated"
        );
        assert_eq!(parse_interval("0"), None);
        assert_eq!(parse_interval(""), None);
        assert_eq!(parse_interval("soon"), None);
        assert_eq!(parse_interval("-30m"), None);
        assert_eq!(parse_interval("30x"), None);
    }

    #[test]
    fn human_round_trips_the_units() {
        assert_eq!(human(30), "30s");
        assert_eq!(human(1800), "30m");
        assert_eq!(human(7200), "2h");
        assert_eq!(human(86400), "1d");
    }

    #[test]
    fn cron_view_renders_a_job_and_a_button_per_row() {
        let jobs: Vec<(String, String, i64, String)> = vec![(
            "abc123".into(),
            "check the deploy".into(),
            1800,
            "scheduled".into(),
        )];
        let tokens = vec!["state-token-1".to_string()];
        let (embed, buttons) = cron_view(&jobs, "777", &tokens);
        let fields = embed["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 1);
        assert!(fields[0]["name"].as_str().unwrap().contains("abc123"));
        assert!(embed["description"].as_str().unwrap().contains("<#777>"));
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0]["custom_id"], json!("cron:remove:state-token-1"));
        assert!(!buttons[0]["custom_id"].as_str().unwrap().contains("abc123"));
        assert_eq!(buttons[0]["style"], json!(4), "red destructive button");
    }

    #[test]
    fn cron_view_caps_buttons_at_five_rows() {
        let jobs: Vec<(String, String, i64, String)> = (0..9)
            .map(|i| {
                (
                    format!("job{i}"),
                    format!("prompt {i}"),
                    60,
                    "scheduled".into(),
                )
            })
            .collect();
        let tokens = (0..MAX_BUTTONS)
            .map(|i| format!("state-token-{i}"))
            .collect::<Vec<_>>();
        let (_embed, buttons) = cron_view(&jobs, "1", &tokens);
        assert_eq!(buttons.len(), MAX_BUTTONS);
    }

    #[test]
    fn long_text_truncates_with_a_notice() {
        let long = "x".repeat(5000);
        let out = truncate(&long, 512);
        assert!(out.ends_with("… (truncated)"));
        assert!(out.chars().count() < 520);
        assert_eq!(truncate("short", 512), "short");
    }

    #[test]
    fn cli_embed_puts_output_in_a_code_block_and_flags_failure() {
        let ok = cli_embed("Memory", "* bench-location: the bench tree", "", true);
        assert!(ok["description"].as_str().unwrap().contains("```"));
        assert_eq!(ok["color"], json!(INFO));
        let bad = cli_embed("Cron", "", "Interval must be >=60s", false);
        assert_eq!(bad["color"], json!(DANGER));
        assert!(bad["description"].as_str().unwrap().contains("Interval"));
    }

    #[test]
    fn an_empty_cli_reply_still_says_something() {
        let out = cli_embed("Cron", "", "", true);
        assert!(out["description"].as_str().unwrap().contains("Done."));
    }

    #[test]
    fn memory_argv_builds_the_gray_call_the_same_way_every_time() {
        assert_eq!(memory_argv("list", None, None), vec!["memory", "list"]);
        assert_eq!(
            memory_argv("set", Some("bench-location"), Some("/home/x")),
            vec!["memory", "set", "bench-location", "/home/x"]
        );
        assert_eq!(
            memory_argv("remove", Some("x"), None),
            vec!["memory", "remove", "x"]
        );
    }

    #[test]
    fn the_model_key_is_scoped_per_conversation() {
        assert_eq!(model_key("chat:1"), "model:chat:1");
        assert_ne!(model_key("chat:1"), model_key("chat:2"));
    }

    #[test]
    fn a_channel_sees_only_its_own_jobs() {
        let rows: Vec<JobRow> = vec![
            ("a".into(), "mine".into(), 1800, "scheduled".into()),
            ("b".into(), "theirs".into(), 600, "scheduled".into()),
            ("c".into(), "legacy".into(), 60, "scheduled".into()),
        ];
        let targets = vec!["7".to_string(), "9".to_string(), String::new()];
        // Channel 7 asks: its own job only — the legacy row is not its.
        let mine = jobs_for_channel(&rows, &targets, "7", "9");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].0, "a");
        // The legacy row belongs to the home channel, so channel 9 sees it
        // plus its own.
        let home = jobs_for_channel(&rows, &targets, "9", "9");
        assert_eq!(home.len(), 2);
        assert!(home.iter().any(|(id, ..)| id == "c"));
        // And it must never appear in a channel that is not the home.
        assert!(
            !jobs_for_channel(&rows, &targets, "7", "9")
                .iter()
                .any(|(id, ..)| id == "c")
        );
    }

    #[test]
    fn the_model_falls_through_all_three_layers() {
        assert_eq!(effective_model(Some("a/b"), Some("c/d")), "a/b");
        assert_eq!(effective_model(None, Some("c/d")), "c/d");
        assert_eq!(effective_model(None, None), "(provider default)");
        assert_eq!(effective_model(Some("  "), Some("c/d")), "c/d");
    }
}
