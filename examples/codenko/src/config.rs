//! Command-line configuration.
//!
//! Hand-rolled rather than pulling in an argument parser: the whole surface is
//! five flags, and the example is meant to be readable end to end.

use std::path::PathBuf;

use agentyk::{DriverId, ModelSpec};

pub const USAGE: &str = "\
codenko — a small terminal coding agent

Usage: codenko [options]

Options:
  -C, --dir <path>       Workspace directory the agent works in (default: .)
      --provider <name>  anthropic | openai (default: whichever key is set)
      --model <id>       Model id (default: per provider, see below)
      --base-url <url>   Override the provider endpoint (compatible servers,
                         gateways, local runtimes)
      --reasoning-effort <level>
                         Reasoning effort, where the driver supports it
                         (openai: none | low | medium | high)
      --log <path>       Append the session's JSONL event log here
  -h, --help             Show this message

Environment:
  ANTHROPIC_API_KEY      Enables --provider anthropic (default claude-sonnet-5)
  OPENAI_API_KEY         Enables --provider openai (default gpt-5.5)
  ANTHROPIC_BASE_URL     Default for --base-url on --provider anthropic
  OPENAI_BASE_URL        Default for --base-url on --provider openai
";

const ANTHROPIC_DEFAULT_MODEL: &str = "claude-sonnet-5";
const OPENAI_DEFAULT_MODEL: &str = "gpt-5.5";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
}

impl Provider {
    fn key_var(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
        }
    }

    /// The conventional endpoint-override variable for this provider — the
    /// same name the official SDKs read, so an existing gateway setup works
    /// without extra flags.
    fn base_url_var(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_BASE_URL",
            Provider::OpenAi => "OPENAI_BASE_URL",
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            Provider::Anthropic => ANTHROPIC_DEFAULT_MODEL,
            Provider::OpenAi => OPENAI_DEFAULT_MODEL,
        }
    }

    fn driver(self) -> DriverId {
        match self {
            Provider::Anthropic => DriverId::anthropic(),
            Provider::OpenAi => DriverId::openai(),
        }
    }

    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "anthropic" => Ok(Provider::Anthropic),
            "openai" => Ok(Provider::OpenAi),
            other => Err(format!(
                "unknown provider `{other}` (expected anthropic or openai)"
            )),
        }
    }
}

/// Parsed flags, before any environment is read.
#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub dir: Option<PathBuf>,
    pub provider: Option<Provider>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub reasoning_effort: Option<String>,
    pub log: Option<PathBuf>,
    pub help: bool,
}

impl Args {
    pub fn parse<I>(arguments: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut args = Args::default();
        let mut arguments = arguments.into_iter();
        while let Some(flag) = arguments.next() {
            let mut value = || {
                arguments
                    .next()
                    .ok_or_else(|| format!("`{flag}` needs a value"))
            };
            match flag.as_str() {
                "-h" | "--help" => args.help = true,
                "-C" | "--dir" => args.dir = Some(PathBuf::from(value()?)),
                "--provider" => args.provider = Some(Provider::parse(&value()?)?),
                "--model" => args.model = Some(value()?),
                "--base-url" => args.base_url = Some(value()?),
                "--reasoning-effort" => args.reasoning_effort = Some(value()?),
                "--log" => args.log = Some(PathBuf::from(value()?)),
                other => return Err(format!("unknown argument `{other}`")),
            }
        }
        Ok(args)
    }
}

/// A resolved run: where to work, which model to use, where to log.
#[derive(Debug)]
pub struct Config {
    pub dir: PathBuf,
    pub model: ModelSpec,
    /// Shown in the header, e.g. `anthropic/claude-sonnet-5 · ~/src/agentyk`.
    pub banner: String,
    pub log: Option<PathBuf>,
}

impl Config {
    /// Resolve flags against the environment. `lookup` reads an environment
    /// variable — injected so the resolution is testable.
    pub fn resolve(args: Args, lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let provider = match args.provider {
            Some(provider) => provider,
            // No explicit provider: pick the one whose key is present, so the
            // common case is `codenko` with no flags at all.
            None => [Provider::Anthropic, Provider::OpenAi]
                .into_iter()
                .find(|provider| lookup(provider.key_var()).is_some())
                .ok_or_else(|| {
                    "no API key found — set ANTHROPIC_API_KEY or OPENAI_API_KEY".to_string()
                })?,
        };
        let api_key = lookup(provider.key_var()).ok_or_else(|| {
            format!(
                "--provider {} needs {}",
                match provider {
                    Provider::Anthropic => "anthropic",
                    Provider::OpenAi => "openai",
                },
                provider.key_var()
            )
        })?;

        let model_id = args
            .model
            .unwrap_or_else(|| provider.default_model().to_string());
        let mut model = ModelSpec::new(provider.driver(), &model_id).api_key(api_key);
        if let Some(url) = args.base_url.or_else(|| lookup(provider.base_url_var())) {
            model = model.base_url(url);
        }
        // Passed through verbatim — the provider validates the level, and its
        // rejection is more informative than anything guessed here. `none`
        // matters on OpenAI's gpt-5.6 family, which refuses function tools on
        // chat completions at any other level.
        if let Some(effort) = args.reasoning_effort {
            model = model.reasoning_effort(effort);
        }

        let dir = args.dir.unwrap_or_else(|| PathBuf::from("."));
        let dir = dir
            .canonicalize()
            .map_err(|error| format!("{}: {error}", dir.display()))?;

        let banner = format!("{}/{model_id} · {}", provider.driver(), dir.display());
        Ok(Config {
            dir,
            model,
            banner,
            log: args.log,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(input: &[&str]) -> Result<Args, String> {
        Args::parse(input.iter().map(|s| s.to_string()))
    }

    #[test]
    fn flags_parse() {
        let parsed = args(&[
            "--provider",
            "openai",
            "--model",
            "gpt-5",
            "-C",
            "/tmp",
            "--base-url",
            "http://localhost:11434/v1",
        ])
        .unwrap();
        assert_eq!(parsed.provider, Some(Provider::OpenAi));
        assert_eq!(parsed.model.as_deref(), Some("gpt-5"));
        assert_eq!(parsed.dir, Some(PathBuf::from("/tmp")));
        assert_eq!(
            parsed.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
    }

    #[test]
    fn a_flag_without_a_value_is_an_error() {
        assert!(args(&["--model"]).is_err());
        assert!(args(&["--nope"]).is_err());
    }

    #[test]
    fn provider_is_inferred_from_the_available_key() {
        let config = Config::resolve(Args::default(), |name| {
            (name == "OPENAI_API_KEY").then(|| "sk-test".to_string())
        })
        .unwrap();
        assert_eq!(config.model.driver, DriverId::openai());
        assert_eq!(config.model.model, OPENAI_DEFAULT_MODEL);
        assert_eq!(config.model.api_key.as_deref(), Some("sk-test"));
        assert_eq!(config.model.base_url, None);
    }

    #[test]
    fn the_endpoint_comes_from_the_flag_then_the_environment() {
        let from_env = Config::resolve(Args::default(), |name| match name {
            "ANTHROPIC_API_KEY" => Some("k".to_string()),
            "ANTHROPIC_BASE_URL" => Some("http://127.0.0.1:8787".to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(
            from_env.model.base_url.as_deref(),
            Some("http://127.0.0.1:8787")
        );

        let from_flag = Config::resolve(
            Args {
                base_url: Some("http://localhost:11434".to_string()),
                ..Args::default()
            },
            |name| match name {
                "ANTHROPIC_API_KEY" => Some("k".to_string()),
                "ANTHROPIC_BASE_URL" => Some("http://ignored".to_string()),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(
            from_flag.model.base_url.as_deref(),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn reasoning_effort_reaches_the_model_spec() {
        let config = Config::resolve(
            Args {
                provider: Some(Provider::OpenAi),
                model: Some("gpt-5.6-terra".into()),
                reasoning_effort: Some("none".into()),
                ..Args::default()
            },
            |_| Some("sk-test".to_string()),
        )
        .unwrap();
        assert_eq!(
            config
                .model
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.effort.as_deref()),
            Some("none")
        );
        assert!(config.banner.contains("gpt-5.6-terra"));
    }

    #[test]
    fn reasoning_is_unset_unless_asked_for() {
        let config = Config::resolve(Args::default(), |_| Some("k".to_string())).unwrap();
        assert!(config.model.reasoning.is_none());
    }

    #[test]
    fn anthropic_wins_when_both_keys_are_set() {
        let config = Config::resolve(Args::default(), |_| Some("k".to_string())).unwrap();
        assert_eq!(config.model.driver, DriverId::anthropic());
        assert_eq!(config.model.model, ANTHROPIC_DEFAULT_MODEL);
    }

    #[test]
    fn an_explicit_provider_requires_its_key() {
        let error = Config::resolve(
            Args {
                provider: Some(Provider::Anthropic),
                ..Args::default()
            },
            |_| None,
        )
        .unwrap_err();
        assert!(error.contains("ANTHROPIC_API_KEY"), "{error}");
    }

    #[test]
    fn no_key_at_all_explains_itself() {
        let error = Config::resolve(Args::default(), |_| None).unwrap_err();
        assert!(error.contains("ANTHROPIC_API_KEY"), "{error}");
        assert!(error.contains("OPENAI_API_KEY"), "{error}");
    }
}
