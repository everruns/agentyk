//! codenko — wire the pieces together and hand the screen to the UI.
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo run -p codenko -- --dir .
//! ```

use std::process::ExitCode;
use std::sync::Arc;

use agentyk::{EventLog, InMemoryEventLog, JsonlEventLog};
use codenko::agent::{build_agent, spawn_agent_task};
use codenko::config::{Args, Config, USAGE};
use codenko::ui::App;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("codenko: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args().skip(1))?;
    if args.help {
        print!("{USAGE}");
        return Ok(());
    }
    let config = Config::resolve(args, |name| std::env::var(name).ok())?;

    // The event log is the persistence seam: point `--log` at a file and the
    // session can be replayed or resumed from it alone.
    let log: Arc<dyn EventLog> = match &config.log {
        Some(path) => Arc::new(JsonlEventLog::new(path).map_err(|error| error.to_string())?),
        None => Arc::new(InMemoryEventLog::new()),
    };

    let (events, inbox) = mpsc::unbounded_channel();
    let agent = build_agent(&config.dir, config.model, events.clone())
        .map_err(|error| error.to_string())?;
    let prompts = spawn_agent_task(agent, log, events);

    // Read the size before the runner takes the screen; resizes arrive as
    // events from there on.
    let size = crossterm::terminal::size().unwrap_or((80, 24));
    App::new(prompts, inbox, config.banner, size)
        .run()
        .await
        .map_err(|error| error.to_string())
}
