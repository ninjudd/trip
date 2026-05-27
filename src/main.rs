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
        Command::Enter { name, command } => {
            let cmd = if command.is_empty() {
                None
            } else {
                Some(command)
            };
            client::enter(name, cmd).await?;
        }
        Command::New { name, command } => {
            let cmd = if command.is_empty() {
                None
            } else {
                Some(command)
            };
            client::new_session(name, cmd).await?;
        }
        Command::Wrap { name, command } => {
            let name = match name {
                Some(n) => n,
                None => client::derive_session_name()?,
            };
            let cmd = if command.is_empty() {
                None
            } else {
                Some(command)
            };
            client::wrap::wrap(name, cmd).await?;
        }
        Command::Create { name, command } => {
            let cmd = if command.is_empty() {
                None
            } else {
                Some(command)
            };
            client::create_session(name, cmd).await?;
        }
        Command::Ls { all } => {
            client::list_sessions(all).await?;
        }
        Command::Attach { name } => {
            client::attach::attach(name).await?;
        }
        Command::Current => match std::env::var("TRIP_SESSION") {
            Ok(name) => println!("{}", name),
            Err(_) => std::process::exit(1),
        },
        Command::Screens { name, index } => {
            client::list_screens(name, index).await?;
        }
        Command::Screen { name, watch } => {
            client::get_screen(name, watch).await?;
        }
        Command::Log {
            name,
            raw,
            follow,
            since,
        } => {
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
        Command::Return => {
            let name = std::env::var("TRIP_SESSION")
                .map_err(|_| anyhow::anyhow!("not in a trip session"))?;
            client::return_session(name).await?;
        }
        Command::Detach { name } => {
            let name = match name {
                Some(n) => n,
                None => std::env::var("TRIP_SESSION").map_err(|_| {
                    anyhow::anyhow!("not in a trip session (use: trip detach <name>)")
                })?,
            };
            client::detach_session(name).await?;
        }
        Command::Kill { name } => {
            client::kill_session(name).await?;
        }
        Command::Shutdown { yes } => {
            client::shutdown(yes).await?;
        }
        Command::On => {
            client::agent_on()?;
        }
        Command::Env => {
            if let Ok(name) = std::env::var("TRIP_SESSION") {
                let path = common::terminal_env_path(&name);
                if path.exists() {
                    let content = std::fs::read_to_string(&path).unwrap_or_default();
                    print!("{}", content);
                }
                // Clean up agent.json if no agent is running
                if std::env::var("CLAUDE_CODE_SESSION_ID").is_err()
                    && std::env::var("CODEX_THREAD_ID").is_err()
                {
                    let agent_path = common::session_dir(&name).join("agent.json");
                    if agent_path.exists() {
                        let _ = std::fs::remove_file(&agent_path);
                    }
                }
            }
        }
        Command::Daemon => {
            daemon::run().await?;
        }
    }

    Ok(())
}
