//! OSBB — one shared bulletin board for named people and an agent.
//!
//! The example keeps one [`agentyk::Session`] and attributes each user message
//! with [`agentyk::ExternalActor`]. The event log therefore retains the raw
//! message plus structured identity, while model-facing requests label each
//! speaker.

use std::io::{self, Write};
use std::path::PathBuf;

use agentyk::{
    Agent, Event, EventData, EventListener, ExternalActor, Message, ModelSpec, OpenAiDriver,
};
use async_trait::async_trait;
use clap::Parser;

const DEFAULT_MODEL: &str = "gpt-5.6-terra";
const DEFAULT_AGENT: &str = "Archivist";
const DEFAULT_PEOPLE: &str = "Ada,Grace";

const HELP: &str = "\
Commands:
  /as NAME  speak as another person
  /who      show everyone on the board
  /help     show this help
  /quit     leave the board";

/// Command-line arguments before environment-backed configuration is resolved.
#[derive(Debug, Parser)]
#[command(
    name = "osbb",
    version,
    about = "One shared bulletin board for named people and an agent.",
    after_help = "Environment:\n  OPENAI_API_KEY   Required\n  OPENAI_BASE_URL  Optional compatible endpoint"
)]
pub struct Args {
    /// Comma-separated display names allowed to speak.
    #[arg(long, default_value = DEFAULT_PEOPLE)]
    pub people: String,

    /// Person selected when the room opens.
    #[arg(long)]
    pub actor: Option<String>,

    /// Display name of the agent.
    #[arg(long, default_value = DEFAULT_AGENT)]
    pub agent_name: String,

    /// OpenAI model id.
    #[arg(long, default_value = DEFAULT_MODEL)]
    pub model: String,

    /// Override the OpenAI-compatible endpoint.
    #[arg(long)]
    pub base_url: Option<String>,

    /// Reasoning effort sent to OpenAI.
    #[arg(long, default_value = "none")]
    pub reasoning_effort: String,

    /// Append the session's JSONL event log here.
    #[arg(long)]
    pub log: Option<PathBuf>,
}

/// Validated configuration for one room.
#[derive(Debug)]
pub struct Config {
    /// People allowed to speak, in display order.
    pub people: Vec<String>,
    /// Initially selected person.
    pub actor: String,
    /// Name shown for assistant output.
    pub agent_name: String,
    /// Live model specification.
    pub model: ModelSpec,
    /// Optional durable event log path.
    pub log: Option<PathBuf>,
}

impl Config {
    /// Resolve parsed arguments against an injected environment lookup.
    pub fn resolve(args: Args, lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let people = parse_people(&args.people)?;
        let actor = args.actor.unwrap_or_else(|| people[0].clone());
        let actor = people
            .iter()
            .find(|name| name.eq_ignore_ascii_case(&actor))
            .cloned()
            .ok_or_else(|| format!("initial actor `{actor}` is not in --people"))?;
        let api_key =
            lookup("OPENAI_API_KEY").ok_or_else(|| "OPENAI_API_KEY is not set".to_string())?;
        let mut model = ModelSpec::openai(args.model)
            .api_key(api_key)
            .reasoning_effort(args.reasoning_effort);
        if let Some(base_url) = args.base_url.or_else(|| lookup("OPENAI_BASE_URL")) {
            model = model.base_url(base_url);
        }

        if args.agent_name.trim().is_empty() {
            return Err("--agent-name cannot be empty".into());
        }
        Ok(Self {
            people,
            actor,
            agent_name: args.agent_name,
            model,
            log: args.log,
        })
    }
}

fn parse_people(value: &str) -> Result<Vec<String>, String> {
    let mut people = Vec::new();
    for name in value.split(',').map(str::trim) {
        if name.is_empty() {
            return Err("--people contains an empty name".into());
        }
        if people
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            return Err(format!("duplicate person `{name}`"));
        }
        people.push(name.to_string());
    }
    if people.is_empty() {
        return Err("--people needs at least one name".into());
    }
    Ok(people)
}

/// Result of submitting one line to a [`Room`].
#[derive(Debug, Clone, PartialEq)]
pub enum RoomAction {
    /// A named person said something for the agent to consider.
    Message(Box<Message>),
    /// Local output that does not invoke the model.
    Notice(String),
    /// No input was supplied.
    Empty,
    /// End the session.
    Quit,
}

/// Named people sharing one conversation with an agent.
#[derive(Debug)]
pub struct Room {
    people: Vec<String>,
    active: usize,
}

impl Room {
    /// Create a room and select its initial speaker.
    pub fn new(people: Vec<String>, actor: &str) -> Result<Self, String> {
        let active = people
            .iter()
            .position(|name| name.eq_ignore_ascii_case(actor))
            .ok_or_else(|| format!("actor `{actor}` is not in the room"))?;
        Ok(Self { people, active })
    }

    /// Name of the person whose prompt is currently active.
    pub fn active_name(&self) -> &str {
        &self.people[self.active]
    }

    /// Display all people, marking the active speaker.
    pub fn roster(&self) -> String {
        self.people
            .iter()
            .enumerate()
            .map(|(index, name)| {
                if index == self.active {
                    format!("{name} (speaking)")
                } else {
                    name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Parse a command or attribute plain text to the active person.
    pub fn submit(&mut self, line: &str) -> Result<RoomAction, String> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(RoomAction::Empty);
        }
        if line == "/quit" {
            return Ok(RoomAction::Quit);
        }
        if line == "/help" {
            return Ok(RoomAction::Notice(HELP.to_string()));
        }
        if line == "/who" {
            return Ok(RoomAction::Notice(self.roster()));
        }
        if let Some(requested) = line.strip_prefix("/as ") {
            let requested = requested.trim();
            self.active = self
                .people
                .iter()
                .position(|name| name.eq_ignore_ascii_case(requested))
                .ok_or_else(|| format!("`{requested}` is not on this board"))?;
            return Ok(RoomAction::Notice(format!(
                "{} is speaking now.",
                self.active_name()
            )));
        }
        if line.starts_with('/') {
            return Err(format!("unknown command `{line}`; try /help"));
        }

        let name = self.active_name();
        Ok(RoomAction::Message(Box::new(
            Message::user(line).with_external_actor(ExternalActor::new("osbb", name).name(name)),
        )))
    }
}

/// Compose the live OpenAI agent used by the example.
pub fn build_agent(
    model: ModelSpec,
    agent_name: &str,
    people: &[String],
) -> agentyk::Result<Agent> {
    let roster = people.join(", ");
    Agent::builder()
        .name(agent_name)
        .system_prompt(format!(
            "You are {agent_name}, participating in one shared bulletin board with {roster}.\n\
             User messages are prefixed with the speaker's name in brackets. Track who said what, \
             address people by name when useful, and never merge or invent their views. People may \
             be speaking to each other rather than asking you a direct question. Respond as a \
             concise, constructive participant. You are {agent_name}: never prefix your own reply \
             with another participant's label or speak as them."
        ))
        .model(model)
        .driver(OpenAiDriver::new())
        .listener(ConsoleListener::new(agent_name))
        .build()
}

/// Streams assistant output to stdout under the configured agent name.
pub struct ConsoleListener {
    agent_name: String,
}

impl ConsoleListener {
    /// Create a terminal listener for one named agent.
    pub fn new(agent_name: impl Into<String>) -> Self {
        Self {
            agent_name: agent_name.into(),
        }
    }
}

#[async_trait]
impl EventListener for ConsoleListener {
    async fn on_event(&self, event: &Event) {
        match &event.data {
            EventData::OutputMessageStarted { .. } => {
                print!("\x1b[36m{}>\x1b[0m ", self.agent_name);
                let _ = io::stdout().flush();
            }
            EventData::OutputMessageDelta { delta, .. } => {
                print!("{delta}");
                let _ = io::stdout().flush();
            }
            EventData::OutputMessageCompleted { .. } => println!(),
            _ => {}
        }
    }
}

/// Help text shown at startup and by `/help`.
pub fn help() -> &'static str {
    HELP
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agentyk::{ChatDriver, ChatRequest, ChatResponse, SimDriver, SimTurn};

    use super::*;

    struct SharedSim(Arc<SimDriver>);

    #[async_trait]
    impl ChatDriver for SharedSim {
        fn id(&self) -> agentyk::DriverId {
            agentyk::DriverId::llmsim()
        }

        async fn complete(&self, request: ChatRequest) -> agentyk::Result<ChatResponse> {
            self.0.complete(request).await
        }
    }

    #[test]
    fn room_switches_only_to_known_named_people() {
        let mut room = Room::new(vec!["Ada".into(), "Grace".into()], "Ada").unwrap();
        assert_eq!(
            room.submit("/as grace").unwrap(),
            RoomAction::Notice("Grace is speaking now.".into())
        );
        assert_eq!(room.active_name(), "Grace");
        assert!(room.submit("/as Linus").unwrap_err().contains("not on"));
    }

    #[test]
    fn messages_carry_the_active_persons_identity() {
        let mut room = Room::new(vec!["Ada".into(), "Grace".into()], "Grace").unwrap();
        let RoomAction::Message(message) = room.submit("Ada, I prefer Wednesday.").unwrap() else {
            panic!("expected a message");
        };
        assert_eq!(message.text(), "Ada, I prefer Wednesday.");
        let actor = message.external_actor.unwrap();
        assert_eq!(actor.actor_id, "Grace");
        assert_eq!(actor.display_label(), "Grace");
        assert_eq!(actor.source, "osbb");
    }

    #[tokio::test]
    async fn one_session_labels_two_people_for_the_model_but_not_history() {
        let driver = Arc::new(SimDriver::new([
            SimTurn::text("Ada reply"),
            SimTurn::text("Grace reply"),
        ]));
        let agent = Agent::builder()
            .name("Archivist")
            .model(ModelSpec::llmsim())
            .driver(SharedSim(driver.clone()))
            .build()
            .unwrap();
        let mut room = Room::new(vec!["Ada".into(), "Grace".into()], "Ada").unwrap();
        let mut session = agent.session();

        let RoomAction::Message(ada) = room.submit("Tuesday?").unwrap() else {
            panic!("expected Ada's message");
        };
        session.run(*ada).await.unwrap();
        room.submit("/as Grace").unwrap();
        let RoomAction::Message(grace) = room.submit("Wednesday is safer.").unwrap() else {
            panic!("expected Grace's message");
        };
        session.run(*grace).await.unwrap();

        assert_eq!(session.messages()[0].text(), "Tuesday?");
        assert_eq!(session.messages()[2].text(), "Wednesday is safer.");
        let requests = driver.recorded_requests();
        assert_eq!(requests[0].messages[0].text(), "[Ada] Tuesday?");
        assert_eq!(requests[1].messages[0].text(), "[Ada] Tuesday?");
        assert_eq!(
            requests[1].messages[2].text(),
            "[Grace] Wednesday is safer."
        );
    }

    #[test]
    fn config_defaults_to_named_people_and_terra() {
        let args = Args::try_parse_from(["osbb"]).unwrap();
        let config = Config::resolve(args, |name| {
            (name == "OPENAI_API_KEY").then(|| "test-key".into())
        })
        .unwrap();

        assert_eq!(config.people, ["Ada", "Grace"]);
        assert_eq!(config.actor, "Ada");
        assert_eq!(config.agent_name, "Archivist");
        assert_eq!(config.model.model, DEFAULT_MODEL);
        assert_eq!(
            config.model.reasoning.unwrap().effort.as_deref(),
            Some("none")
        );
    }
}
