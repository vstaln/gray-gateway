//! `gray gateway session send|read` — the SEND side of gray's session
//! mailbox, plus a read-only transcript viewer. Core keeps the RECEIVE side
//! (claim/steer/hold-back/deliver); this module must stay byte-compatible with
//! core's `session_send.rs` / `session_mailbox.rs` / `session_store.rs`.

mod read;
mod send;
mod store;

pub use read::run as run_read;
pub use send::run as run_send;

#[derive(clap::Subcommand, Debug, Clone)]
pub enum SessionCmd {
    #[command(about = "Send a message (and attachments) into a live session")]
    Send {
        #[arg(value_name = "SESSION_ID", help = "Session id or unique prefix")]
        session: String,

        #[arg(value_name = "TEXT", default_value = "", help = "Message text")]
        text: String,

        #[arg(
            long = "attach",
            value_name = "PATH",
            help = "Attach a file (repeatable)"
        )]
        attach: Vec<std::path::PathBuf>,

        #[arg(long, value_name = "LABEL", help = "Sender label shown in the session")]
        from: Option<String>,

        #[arg(long, value_name = "SECS", help = "Wait up to SECS for the reply")]
        wait: Option<u64>,
    },

    #[command(
        about = "Read a session's recent conversation, or list live sessions",
        after_help = "Examples:\n  gray read --live            live sessions: id, pid, cwd, model, last activity\n  gray read <SESSION_ID>      last 20 messages, tool calls summarized\n  gray read <SESSION_ID> --last 50 --json\nReads the owner's session store ($GRAY_USER_HOME, else $GRAY_HOME). A session is live only while its .open lock is held."
    )]
    Read {
        #[arg(value_name = "SESSION_ID", help = "Session id or unique prefix")]
        session: Option<String>,

        #[arg(long, help = "List live sessions instead of reading one")]
        live: bool,

        #[arg(
            long,
            value_name = "N",
            default_value_t = 20,
            help = "Show the last N messages"
        )]
        last: usize,

        #[arg(long, help = "Emit JSON instead of rendered text")]
        json: bool,
    },
}

pub async fn run(cmd: SessionCmd) -> anyhow::Result<()> {
    match cmd {
        SessionCmd::Send {
            session,
            text,
            attach,
            from,
            wait,
        } => run_send(&session, &text, &attach, from.as_deref(), wait).await,
        SessionCmd::Read {
            session,
            live,
            last,
            json,
        } => run_read(session.as_deref(), live, last, json),
    }
}

/// Core's `owner_home()`: `$GRAY_USER_HOME` (a subagent's owner), else
/// `$GRAY_HOME`, else `~/.gray`.
pub(crate) fn owner_home() -> anyhow::Result<std::path::PathBuf> {
    match std::env::var_os("GRAY_USER_HOME").filter(|v| !v.to_string_lossy().trim().is_empty()) {
        Some(home) => Ok(std::path::PathBuf::from(home)),
        None => crate::vendor::gray_home(),
    }
}
