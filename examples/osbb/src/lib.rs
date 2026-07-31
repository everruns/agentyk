//! OSBB — one shared conversation between a building's co-owners and the
//! association that answers them.
//!
//! An [OSBB](https://uk.wikipedia.org/wiki/Об%27єднання_співвласників_багатоквартирного_будинку)
//! is a Ukrainian condominium association: the co-owners of one apartment
//! building, acting together. Here the agent speaks for the association and
//! the named people are co-owners of the building — five of them share the
//! conversation, and two bring the same noise complaint from different
//! apartments.
//!
//! The example keeps one [`agentyk::Session`] and attributes each co-owner's
//! message with [`agentyk::ExternalActor`]. The event log therefore retains the
//! raw message plus structured identity, while model-facing requests label each
//! speaker.

use std::io::{self, Write};
use std::path::PathBuf;

use agentyk::{
    Agent, Event, EventData, EventListener, ExternalActor, Message, ModelSpec, OpenAiDriver,
};
use async_trait::async_trait;
use clap::Parser;

const DEFAULT_MODEL: &str = "gpt-5.6-terra";
const DEFAULT_AGENT: &str = "Manager";
const DEFAULT_PEOPLE: &str = "Olena,Petro,Iryna,Mykola,Sofia";
const DEFAULT_BUILDING: &str = "Sadova 12";

/// House rules the association applies. Kept in the prompt rather than behind a
/// tool: the example is about multi-actor sessions, and a grounded reply needs
/// something concrete to cite.
const HOUSE_RULES: &str = "\
  - Quiet hours are 22:00-08:00 every day.\n\
  - Renovation work is allowed 09:00-19:00 on weekdays only, never on Sundays \
    or public holidays.\n\
  - A complaint is logged with the apartment number, the dates, and the times; \
    complaints from more than one apartment go to the next board meeting.\n\
  - The association can send a written warning and call the neighbour in; it \
    escalates to the police or a fine only after a warning goes unanswered.\n\
  - Only the co-owners' general meeting can change the rules or the budget.";

const HELP: &str = "\
Commands:
  /as NAME  speak as another co-owner
  /who      show everyone in the conversation
  /help     show this help
  /quit     leave the conversation";

/// Command-line arguments before environment-backed configuration is resolved.
#[derive(Debug, Parser)]
#[command(
    name = "osbb",
    version,
    about = "One shared conversation between a building's co-owners and their association.",
    after_help = "Environment:\n  OPENAI_API_KEY   Required\n  OPENAI_BASE_URL  Optional compatible endpoint"
)]
pub struct Args {
    /// Comma-separated display names of the co-owners allowed to speak.
    #[arg(long, default_value = DEFAULT_PEOPLE)]
    pub people: String,

    /// Co-owner selected when the conversation opens.
    #[arg(long)]
    pub actor: Option<String>,

    /// Display name of the agent answering for the association.
    #[arg(long, default_value = DEFAULT_AGENT)]
    pub agent_name: String,

    /// Building the association manages.
    #[arg(long, default_value = DEFAULT_BUILDING)]
    pub building: String,

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

/// Validated configuration for one conversation.
#[derive(Debug)]
pub struct Config {
    /// Co-owners allowed to speak, in display order.
    pub people: Vec<String>,
    /// Initially selected co-owner.
    pub actor: String,
    /// Name shown for assistant output.
    pub agent_name: String,
    /// Building the association speaks for.
    pub building: String,
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
        if args.building.trim().is_empty() {
            return Err("--building cannot be empty".into());
        }
        Ok(Self {
            people,
            actor,
            agent_name: args.agent_name,
            building: args.building,
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
    /// A named co-owner said something for the association to consider.
    Message(Box<Message>),
    /// Local output that does not invoke the model.
    Notice(String),
    /// No input was supplied.
    Empty,
    /// End the session.
    Quit,
}

/// Named co-owners sharing one conversation with the association's agent.
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

    /// Name of the co-owner whose prompt is currently active.
    pub fn active_name(&self) -> &str {
        &self.people[self.active]
    }

    /// Display all co-owners, marking the active speaker.
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

    /// Parse a command or attribute plain text to the active co-owner.
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
                .ok_or_else(|| format!("`{requested}` does not live here"))?;
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

/// Compose the live OpenAI agent that speaks for the association.
pub fn build_agent(
    model: ModelSpec,
    agent_name: &str,
    building: &str,
    people: &[String],
) -> agentyk::Result<Agent> {
    Agent::builder()
        .name(agent_name)
        .system_prompt(system_prompt(agent_name, building, people))
        .model(model)
        .driver(OpenAiDriver::new())
        .listener(ConsoleListener::new(agent_name))
        .build()
}

/// The association's standing instructions: who it speaks for, which rules it
/// applies, and how it must treat a room full of co-owners.
pub fn system_prompt(agent_name: &str, building: &str, people: &[String]) -> String {
    let roster = people.join(", ");
    format!(
        "You are {agent_name}, speaking for the OSBB (the co-owners' association) of {building}. \
         You are in one shared conversation with its co-owners: {roster}.\n\n\
         House rules you apply:\n{HOUSE_RULES}\n\n\
         User messages are prefixed with the speaker's name in brackets. Track who said what, \
         address people by name, and never merge or invent their accounts — two co-owners \
         reporting the same disturbance are two separate reports, and their details may differ. \
         Co-owners often speak to each other rather than to you; answer the thread, not only the \
         last line, and do not assume silence from the others is agreement.\n\n\
         Answer as the association would: acknowledge the complaint, state which rule applies, \
         say what the association will actually do next and by when, and ask for the specific \
         detail you still need (apartment number, dates, times). Stay concise and neutral. Do not \
         take the side of one co-owner against a neighbour before the facts are recorded, do not \
         promise fines, evictions, or spending the association has no authority to decide alone, \
         and say plainly when something needs the board or the general meeting. You are \
         {agent_name}: never prefix your own reply with a co-owner's label or speak as them."
    )
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

    fn house() -> Vec<String> {
        DEFAULT_PEOPLE.split(',').map(str::to_string).collect()
    }

    #[test]
    fn room_switches_only_to_known_co_owners() {
        let mut room = Room::new(house(), "Olena").unwrap();
        assert_eq!(
            room.submit("/as petro").unwrap(),
            RoomAction::Notice("Petro is speaking now.".into())
        );
        assert_eq!(room.active_name(), "Petro");
        assert_eq!(
            room.submit("/as Sofia").unwrap(),
            RoomAction::Notice("Sofia is speaking now.".into())
        );
        assert!(
            room.submit("/as Mykhailo")
                .unwrap_err()
                .contains("does not live here")
        );
    }

    #[test]
    fn the_roster_marks_one_speaker_among_all_five() {
        let mut room = Room::new(house(), "Olena").unwrap();
        room.submit("/as Iryna").unwrap();
        assert_eq!(
            room.roster(),
            "Olena, Petro, Iryna (speaking), Mykola, Sofia"
        );
    }

    #[test]
    fn messages_carry_the_active_co_owners_identity() {
        let mut room = Room::new(house(), "Petro").unwrap();
        let RoomAction::Message(message) = room
            .submit("The drilling above me started again at 07:00.")
            .unwrap()
        else {
            panic!("expected a message");
        };
        assert_eq!(
            message.text(),
            "The drilling above me started again at 07:00."
        );
        let actor = message.external_actor.unwrap();
        assert_eq!(actor.actor_id, "Petro");
        assert_eq!(actor.display_label(), "Petro");
        assert_eq!(actor.source, "osbb");
    }

    #[tokio::test]
    async fn two_co_owners_complain_on_one_session_labelled_only_for_the_model() {
        let driver = Arc::new(SimDriver::new([
            SimTurn::text("Logged apartment 41's night noise."),
            SimTurn::text("Two apartments now report the same nights."),
        ]));
        let people = house();
        let agent = Agent::builder()
            .name(DEFAULT_AGENT)
            .system_prompt(system_prompt(DEFAULT_AGENT, DEFAULT_BUILDING, &people))
            .model(ModelSpec::llmsim())
            .driver(SharedSim(driver.clone()))
            .build()
            .unwrap();
        let mut room = Room::new(people, "Olena").unwrap();
        let mut session = agent.session();

        let RoomAction::Message(olena) = room
            .submit("Apartment 41 plays music past midnight; I am in 37 and cannot sleep.")
            .unwrap()
        else {
            panic!("expected Olena's message");
        };
        session.run(*olena).await.unwrap();
        room.submit("/as Petro").unwrap();
        let RoomAction::Message(petro) = room
            .submit("Same here in 45 - Tuesday and Thursday, around 00:30.")
            .unwrap()
        else {
            panic!("expected Petro's message");
        };
        session.run(*petro).await.unwrap();

        // Durable history keeps exactly what each co-owner typed.
        assert_eq!(
            session.messages()[0].text(),
            "Apartment 41 plays music past midnight; I am in 37 and cannot sleep."
        );
        assert_eq!(
            session.messages()[2].text(),
            "Same here in 45 - Tuesday and Thursday, around 00:30."
        );

        // The provider sees who is speaking, on one shared history, under the
        // association's standing instructions.
        let requests = driver.recorded_requests();
        let system = requests[0].system_prompt.as_deref().unwrap();
        assert!(system.contains("association) of Sadova 12"));
        assert!(system.contains("Olena, Petro, Iryna, Mykola, Sofia"));
        assert_eq!(
            requests[0].messages[0].text(),
            "[Olena] Apartment 41 plays music past midnight; I am in 37 and cannot sleep."
        );
        assert_eq!(
            requests[1].messages[0].text(),
            "[Olena] Apartment 41 plays music past midnight; I am in 37 and cannot sleep."
        );
        assert_eq!(
            requests[1].messages[2].text(),
            "[Petro] Same here in 45 - Tuesday and Thursday, around 00:30."
        );
    }

    #[test]
    fn config_defaults_to_five_co_owners_and_terra() {
        let args = Args::try_parse_from(["osbb"]).unwrap();
        let config = Config::resolve(args, |name| {
            (name == "OPENAI_API_KEY").then(|| "test-key".into())
        })
        .unwrap();

        assert_eq!(
            config.people,
            ["Olena", "Petro", "Iryna", "Mykola", "Sofia"]
        );
        assert_eq!(config.actor, "Olena");
        assert_eq!(config.agent_name, "Manager");
        assert_eq!(config.building, "Sadova 12");
        assert_eq!(config.model.model, DEFAULT_MODEL);
        assert_eq!(
            config.model.reasoning.unwrap().effort.as_deref(),
            Some("none")
        );
    }

    #[test]
    fn the_prompt_binds_the_agent_to_the_association_and_its_rules() {
        let prompt = system_prompt(DEFAULT_AGENT, DEFAULT_BUILDING, &house());
        assert!(prompt.contains("co-owners' association) of Sadova 12"));
        assert!(prompt.contains("Quiet hours are 22:00-08:00"));
        assert!(prompt.contains("general meeting"));
    }
}
