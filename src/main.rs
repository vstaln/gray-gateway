mod cron;
mod cron_cli;
mod cron_fire;
mod cron_serve;
mod cron_status;
mod cron_tool;
mod gateway;
mod redact;
mod session;
mod sidecar;
mod vendor;

use clap::Parser;

#[derive(clap::Subcommand, Debug, Clone)]
pub enum CronCmd {
    #[command(about = "List cron jobs")]
    List {
        #[arg(long, help = "Emit machine-readable JSON for plugin consumers")]
        json: bool,
    },

    #[command(
        about = "Add a scheduled job",
        after_help = "Reminder (one command, no model turn):\n  gray cron add \"in 2m\" \"clean my roo\" --reminder\nThe text is stored and delivered verbatim: never reword it, never fix typos, never pass --name.\n\nSchedules: \"in 10m\", \"every 1h\", \"daily 9am\", \"weekdays 9:00\", \"every monday 6pm\", an RFC3339 timestamp, or a 5-field cron expression."
    )]
    Add {
        #[arg(
            value_name = "SCHEDULE",
            help = "\"in 10m\", \"every 1h\", \"daily 9am\", RFC3339, or 5-field cron"
        )]
        schedule: String,

        #[arg(value_name = "PROMPT", help = "Prompt the agent runs on each fire")]
        prompt: String,

        #[arg(long, help = "Where the result goes: 'local', 'origin', or a target")]
        deliver: Option<String>,

        #[arg(
            long = "origin-session",
            value_name = "ID",
            help = "Deliver to this session instead"
        )]
        origin_session: Option<String>,

        #[arg(long, help = "Job name (default: first line of the prompt)")]
        name: Option<String>,

        #[arg(
            long = "in",
            value_name = "DIR",
            help = "Working directory for the job"
        )]
        workdir: Option<std::path::PathBuf>,

        #[arg(
            long,
            value_name = "LIST",
            help = "Comma-separated skills to load for this job"
        )]
        skills: Option<String>,

        #[arg(
            long,
            value_name = "PATH",
            help = "Run this script file as the job instead of a prompt"
        )]
        script: Option<std::path::PathBuf>,

        #[arg(
            long,
            help = "Store-and-deliver reminder — no model turn, text goes out verbatim"
        )]
        reminder: bool,
    },

    #[command(about = "Run every due job once (what the ticker calls each minute)")]
    Tick {
        #[arg(long, help = "Emit JSON instead of text")]
        json: bool,
    },

    #[command(about = "Run the cron ticker in the foreground")]
    Serve,

    #[command(about = "Pause a job")]
    Pause {
        #[arg(value_name = "ID")]
        id: String,
    },

    #[command(about = "Resume a paused job")]
    Resume {
        #[arg(value_name = "ID")]
        id: String,
    },

    #[command(about = "Fire a job right now")]
    Run {
        #[arg(value_name = "ID")]
        id: String,
    },

    #[command(about = "Show one job's schedule, delivery and last result")]
    Show {
        #[arg(value_name = "ID")]
        id: String,
    },

    #[command(about = "Delete a job")]
    Remove {
        #[arg(value_name = "ID")]
        id: String,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum LifecycleCmd {
    #[command(about = "Record a gateway boot for DIR")]
    Boot {
        #[arg(long, value_name = "DIR")]
        dir: std::path::PathBuf,

        #[arg(
            long,
            default_value_t = 0,
            help = "How many turns were interrupted mid-flight"
        )]
        interrupted: usize,
    },

    #[command(about = "Record a gateway stop for DIR")]
    Stop {
        #[arg(long, value_name = "DIR")]
        dir: std::path::PathBuf,
    },

    #[command(about = "Mark DIR as wanting a restart")]
    Restart {
        #[arg(long, value_name = "DIR")]
        dir: std::path::PathBuf,
    },

    #[command(about = "Clear a pending restart for DIR")]
    ClearRestart {
        #[arg(long, value_name = "DIR")]
        dir: std::path::PathBuf,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum GatewayCmd {
    #[command(about = "Connect a chat platform (e.g. telegram, discord)")]
    Setup {
        #[arg(
            value_name = "PLATFORM",
            help = "Platform to set up; omit to choose from a numbered list"
        )]
        platform: Option<String>,
    },

    #[command(about = "Run the gateway in the foreground")]
    Run,

    #[command(about = "Show gateway, cron and service status")]
    Status,

    #[command(about = "Start the installed service")]
    Start,

    #[command(about = "Stop the running gateway")]
    Stop,

    #[command(about = "Restart the service")]
    Restart,

    #[command(about = "Install the user service (runit/systemd) so the gateway survives logout")]
    Install {
        #[arg(long, help = "Install but don't start it")]
        no_start: bool,

        #[arg(long, help = "Print the service definition instead of installing")]
        print: bool,
    },

    #[command(about = "Remove the installed service")]
    Uninstall,

    #[command(about = "Enable the gateway (the master switch)")]
    On,

    #[command(about = "Disable the gateway — run/start refuse to start while off")]
    Off,

    #[command(
        subcommand,
        about = "Service-supervisor lifecycle hooks (used internally)"
    )]
    Lifecycle(LifecycleCmd),

    #[command(flatten)]
    Agent(gateway::cli::AgentCmd),

    #[command(subcommand, about = "Manage scheduled jobs (what `gray cron` runs)")]
    Cron(CronCmd),

    #[command(
        subcommand,
        about = "Send into or read gray sessions (what `gray send` / `gray read` run)"
    )]
    Session(session::SessionCmd),
}

#[derive(Parser, Debug)]
#[command(
    name = "gray-gateway",
    version,
    about = "The always-on gateway daemon: chat apps, cron, scheduling"
)]
struct Cli {
    #[command(subcommand)]
    cmd: GatewayCmd,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("manifest") => {
            println!("{}", sidecar::manifest());
            return Ok(());
        }
        Some("sidecar") | None => {
            if argv.is_empty() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                use clap::CommandFactory;
                Cli::command().print_long_help().ok();
                std::process::exit(2);
            }
            env_logger::init();
            return sidecar::run().await;
        }
        _ => {}
    }
    let cli = Cli::try_parse_from(std::iter::once("gray-gateway".to_string()).chain(argv.clone()));
    let cli = match cli {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    env_logger::init();
    match cli.cmd {
        GatewayCmd::Cron(cmd) => cron_cli::run_cron(cmd).await,
        GatewayCmd::Session(cmd) => session::run(cmd).await,
        cmd => gateway::run_cli(cmd).await,
    }
}
