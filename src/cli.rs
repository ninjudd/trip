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

    /// List sessions
    Ls,

    /// Attach to a session
    Attach {
        /// Session name
        name: String,
    },

    /// Print the current session name
    Current,

    /// Detach all clients from a session (defaults to current session)
    Detach {
        /// Session name (omit to detach from current session)
        name: Option<String>,
    },

    /// Show session log/screen contents
    Log {
        /// Session name
        name: String,

        /// Show raw PTY transcript bytes
        #[arg(long)]
        raw: bool,

        /// Follow new output
        #[arg(long, short)]
        follow: bool,
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

    /// Kill a session
    Kill {
        /// Session name
        name: String,
    },

    /// Start the daemon (typically auto-started)
    #[command(hide = true)]
    Daemon,
}
