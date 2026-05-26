use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "drip", about = "Persistent terminal sessions")]
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

    /// Create a session without attaching
    Create {
        /// Session name
        name: String,

        /// Command to run (defaults to $SHELL)
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// List sessions
    Ls,

    /// Attach to a session
    Attach {
        /// Session name
        name: String,
    },

    /// Print the current session name
    Current,

    /// Browse screen snapshots
    Screens {
        /// Session name
        name: String,

        /// Show a specific snapshot by index
        index: Option<usize>,
    },

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

    /// Initialize shell environment (called from precmd)
    #[command(hide = true)]
    Init,

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
