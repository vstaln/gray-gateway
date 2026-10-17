#[derive(clap::Subcommand, Debug, Clone)]
pub enum AgentCmd {
    #[command(about = "Send the main agent a message and wait for its reply")]
    Send {
        #[arg(required = true, trailing_var_arg = true, value_name = "TEXT")]
        text: Vec<String>,

        #[arg(
            long,
            default_value_t = 900,
            value_name = "SECS",
            help = "Seconds to wait for a reply (0 = don't wait)"
        )]
        wait: u64,
    },

    #[command(about = "Trigger a turn without a user message (fires on wake)")]
    Wake {
        #[arg(required = true, trailing_var_arg = true, value_name = "TEXT")]
        text: Vec<String>,
    },

    #[command(about = "Show recent gateway activity")]
    Activity {
        #[arg(short = 'n', long, default_value_t = 30, help = "Number of rows")]
        lines: usize,
    },

    #[command(about = "Pause wake/trigger turns (messages still run)")]
    Pause,

    #[command(about = "Resume wake/trigger turns")]
    Resume,

    #[command(about = "Pull and ack pending outbox messages")]
    Inbox,
}

use std::path::Path;
use std::time::Duration;

use super::event::{self, Event, Kind, MAIN};
use super::outbox::{self, Intent, LOCAL};
use crate::cron::store::Origin as Route;

pub async fn run(cmd: AgentCmd) -> anyhow::Result<()> {
    let home = crate::vendor::gray_home()?;
    let dir = super::state_dir(&home);
    std::fs::create_dir_all(&dir)?;
    match cmd {
        AgentCmd::Send { text, wait } => send(&home, &dir, &text.join(" "), wait).await?,
        AgentCmd::Wake { text } => {
            event::admit(
                &dir,
                &Event::new(Kind::Trigger, MAIN, &text.join(" "), None),
            )?;
            println!("woke main; see `gray gateway activity`");
            warn_if_down(&home);
        }
        AgentCmd::Activity { lines } => {
            let rows = super::activity::tail(&dir, lines);
            if rows.is_empty() {
                println!("no activity yet");
            }
            for row in rows {
                println!("{}", super::activity::render(&row));
            }
        }
        AgentCmd::Pause => {
            std::fs::write(dir.join("PAUSED"), "")?;
            println!("paused: wake and trigger turns wait; messages still run");
        }
        AgentCmd::Resume => {
            let _ = std::fs::remove_file(dir.join("PAUSED"));
            println!("resumed");
        }
        AgentCmd::Inbox => {
            let got = outbox::pull(&dir, LOCAL, crate::cron::now_secs(), 100);
            if got.is_empty() {
                println!("inbox empty");
            }
            for i in &got {
                println!("{}  {}", when(i.created_at), label(i));
            }
            outbox::ack(&dir, &got.iter().map(|i| i.id.clone()).collect::<Vec<_>>());
        }
    }
    Ok(())
}

async fn send(home: &Path, dir: &Path, text: &str, wait: u64) -> anyhow::Result<()> {
    let route = Route {
        platform: LOCAL.into(),
        chat: "cli".into(),
        thread: None,
        route: None,
    };
    let sent_at = crate::cron::now_secs();
    event::admit(dir, &Event::new(Kind::User, MAIN, text, Some(route)))?;
    if super::pid::running(home).is_none() {
        println!("queued; the gateway isn't running (`gray gateway start`)");
        return Ok(());
    }
    if wait == 0 {
        println!("sent");
        return Ok(());
    }
    let deadline = sent_at + wait as i64;
    loop {
        let now = crate::cron::now_secs();
        let got = outbox::pull(dir, LOCAL, now, 50);
        for i in &got {
            println!("{}", label(i));
        }
        outbox::ack(dir, &got.iter().map(|i| i.id.clone()).collect::<Vec<_>>());
        if answered(&got, &super::activity::tail(dir, 50), sent_at) {
            return Ok(());
        }
        if now >= deadline {
            println!("no reply yet; `gray gateway inbox` later");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn answered(got: &[Intent], activity: &[serde_json::Value], sent_at: i64) -> bool {
    got.iter()
        .any(|i| i.kind == Kind::User && i.key == MAIN && i.created_at >= sent_at)
        || activity.iter().any(|r| {
            r["what"] == "suppressed"
                && r["key"] == MAIN
                && r["kind"] == "user"
                && r["at"].as_i64().is_some_and(|at| at >= sent_at)
        })
}

fn label(i: &Intent) -> String {
    if i.kind == Kind::User {
        i.text.clone()
    } else {
        format!("[{}] {}", i.kind.as_str(), i.text)
    }
}

fn when(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn warn_if_down(home: &Path) {
    if super::pid::running(home).is_none() {
        println!("(the gateway isn't running; it picks this up on `gray gateway start`)");
    }
}
