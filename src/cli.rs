use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "trip", about = "Persistent terminal sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Enter the canonical workspace session (create or attach)
    Enter {
        /// Session name (derived from workspace if omitted)
        name: Option<String>,

        /// Command to run if creating (defaults to $SHELL)
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// Open a new durable terminal (auto-numbered)
    New {
        /// Base session name (derived from workspace if omitted)
        name: Option<String>,

        /// Command to run (defaults to $SHELL)
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// Wrap a command with JSONL protocol (stdin/stdout become structured events)
    Wrap {
        /// Session name (derived from workspace if omitted)
        name: Option<String>,

        /// Command to run (defaults to $SHELL)
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// Create a session without attaching
    Create {
        /// Session name
        name: String,

        /// Command to run (defaults to $SHELL)
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// List sessions
    Ls {
        /// Show all sessions including hidden background ones
        #[arg(short, long)]
        all: bool,
    },

    /// Attach to a session
    Attach {
        /// Session name
        name: String,
    },

    /// Print the current session name
    Current,

    /// Show current terminal screen
    Screen {
        /// Session name
        name: String,

        /// Stream screen updates
        #[arg(long, short)]
        watch: bool,
    },

    /// Show session recording
    Log {
        /// Session name
        name: String,

        /// Show raw JSONL events
        #[arg(long)]
        raw: bool,

        /// Show tool results in agent logs
        #[arg(long, short)]
        verbose: bool,

        /// Follow new events
        #[arg(long, short)]
        follow: bool,

        /// Show events from the last duration (e.g. 10m, 1h)
        #[arg(long)]
        since: Option<String>,
    },

    /// Send input to a session
    Send {
        /// Session name
        name: String,

        /// Input text to send
        input: String,

        /// Send raw bytes without appending Enter
        #[arg(long)]
        raw: bool,
    },

    /// Return to the previous session (opposite of enter)
    Return,

    /// Detach all clients from a session (defaults to current session)
    Detach {
        /// Session name (omit to detach from current session)
        name: Option<String>,
    },

    /// Kill a session
    Kill {
        /// Session name
        name: String,
    },

    /// Stop the daemon and kill all sessions
    Shutdown {
        /// Skip confirmation
        #[arg(long)]
        yes: bool,
    },

    /// Connect trip to the current agent's structured log
    On,

    /// Sync shell environment (called from preexec hook)
    #[command(hide = true)]
    Env,

    /// Start the daemon (typically auto-started)
    #[command(hide = true)]
    Daemon,
}

pub fn parse_duration(s: &str) -> anyhow::Result<f64> {
    let s = s.trim();
    let (num, unit) = if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (s, 1.0)
    };
    let n: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration: {}", s))?;
    Ok(n * unit)
}
