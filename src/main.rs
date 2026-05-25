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
        Command::Enter { name, command } => {
            let cmd = if command.is_empty() {
                None
            } else {
                Some(command)
            };
            client::enter(name, cmd).await?;
        }
        Command::Ls => {
            client::list_sessions().await?;
        }
        Command::Attach { name } => {
            client::attach::attach(name).await?;
        }
        Command::Current => {
            match std::env::var("DRIP_SESSION") {
                Ok(name) => println!("{}", name),
                Err(_) => std::process::exit(1),
            }
        }
        Command::Screen { name, watch } => {
            client::get_screen(name, watch).await?;
        }
        Command::Log { name, raw, follow, since } => {
            let since_ts = match since {
                Some(ref s) => {
                    let secs = cli::parse_duration(s)?;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs_f64();
                    Some(now - secs)
                }
                None => None,
            };
            client::get_log(name, raw, follow, since_ts).await?;
        }
        Command::Send { name, input, raw } => {
            client::send_input(name, input, raw).await?;
        }
        Command::Detach { name } => {
            let name = match name {
                Some(n) => n,
                None => std::env::var("DRIP_SESSION")
                    .map_err(|_| anyhow::anyhow!("not in a drip session (use: drip detach <name>)"))?,
            };
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
