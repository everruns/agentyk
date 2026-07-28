//! OSBB — run one shared terminal conversation.
//!
//! ```sh
//! OPENAI_API_KEY=... cargo run -p osbb
//! ```

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;

use agentyk::{EventLog, InMemoryEventLog, JsonlEventLog};
use clap::Parser;
use osbb::{Args, Config, Room, RoomAction, build_agent};

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("osbb: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let config = Config::resolve(args, |name| std::env::var(name).ok())?;
    let log: Arc<dyn EventLog> = match &config.log {
        Some(path) => Arc::new(JsonlEventLog::new(path).map_err(|error| error.to_string())?),
        None => Arc::new(InMemoryEventLog::new()),
    };
    let agent = build_agent(config.model, &config.agent_name, &config.people)
        .map_err(|error| error.to_string())?;
    let mut session = agent.session_with_log(log);
    let mut room = Room::new(config.people, &config.actor)?;

    println!(
        "\x1b[1mOSBB\x1b[0m · openai/{} · people: {} · agent: {}",
        agent.model().model,
        room.roster(),
        config.agent_name
    );
    println!("Type /help for commands.\n");

    loop {
        print!("\x1b[33m{}>\x1b[0m ", room.active_name());
        io::stdout().flush().map_err(|error| error.to_string())?;
        let mut line = String::new();
        if io::stdin()
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            break;
        }
        match room.submit(&line) {
            Ok(RoomAction::Message(message)) => {
                if let Err(error) = session.run(*message).await {
                    eprintln!("\x1b[31mturn failed:\x1b[0m {error}");
                }
            }
            Ok(RoomAction::Notice(notice)) => println!("{notice}"),
            Ok(RoomAction::Empty) => {}
            Ok(RoomAction::Quit) => break,
            Err(error) => eprintln!("{error}"),
        }
    }

    session.close().await.map_err(|error| error.to_string())?;
    println!("\nBoard closed.");
    Ok(())
}
