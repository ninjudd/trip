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

    /// Detach all clients from a session
    Detach {
        /// Session name
        name: String,
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
