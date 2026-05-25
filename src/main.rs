mod cli;
mod client;
mod common;
mod daemon;

use clap::Parser;
use cli::{Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::New { name, command } => {
            let cmd = if command.is_empty() {
                None
            } else {
                Some(command)
            };
            client::create_session(name, cmd).await?;
        }
        Command::Ls => {
            client::list_sessions().await?;
        }
        Command::Attach { name } => {
            client::attach::attach(name).await?;
        }
        Command::Detach { name } => {
            client::detach_session(name).await?;
        }
        Command::Kill { name } => {
            client::kill_session(name).await?;
        }
        Command::Daemon => {
            daemon::run().await?;
        }
    }

    Ok(())
}
