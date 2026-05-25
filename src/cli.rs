use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "drip", about = "Persistent terminal sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a new session
    New {
        /// Session name
        name: String,

        /// Command to run (defaults to $SHELL)
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// Enter a persistent session (create or attach)
    Enter {
        /// Session name (derived from workspace if omitted)
        name: Option<String>,

        /// Command to run if creating (defaults to $SHELL)
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

    /// Start the daemon (typically auto-started)
    #[command(hide = true)]
    Daemon,
}

pub fn parse_duration(s: &str) -> anyhow::Result<f64> {
    let s = s.trim();
    let (num, unit) = if s.ends_with('s') {
        (&s[..s.len() - 1], 1.0)
    } else if s.ends_with('m') {
        (&s[..s.len() - 1], 60.0)
    } else if s.ends_with('h') {
        (&s[..s.len() - 1], 3600.0)
    } else {
        (s, 1.0)
    };
    let n: f64 = num.parse().map_err(|_| anyhow::anyhow!("invalid duration: {}", s))?;
    Ok(n * unit)
}
