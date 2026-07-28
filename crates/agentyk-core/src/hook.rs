//! User hooks at stable agent lifecycle boundaries.
//!
//! Hooks are behavior-bearing values attached to an agent. The six events
//! mirror Everruns' user-hook surface: session start/end, user prompt submit,
//! pre/post tool use, and turn end. A hook may be an ordinary Rust value, a
//! shell-backed implementation supplied by the `agentyk` facade, or an
//! adapter owned by another host.
//!
//! This module is deliberately host-neutral. It defines payloads, matching,
//! decisions, and the [`Hook`] trait, but never spawns a process. Executors
//! belong in integration code because a durable or sandboxed host will run
//! them differently from the in-process host.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::id::{SessionId, TurnId};

/// A stable lifecycle point at which a user hook can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before the first turn executed by a newly created session.
    SessionStart,
    /// After a session is explicitly closed.
    SessionEnd,
    /// After a turn id is allocated but before its input enters history.
    UserPromptSubmit,
    /// Before one model-requested tool call is executed.
    PreToolUse,
    /// After one tool call returns and before its result enters history.
    PostToolUse,
    /// After a turn reaches a terminal outcome.
    TurnEnd,
}

impl HookEvent {
    /// Stable snake-case spelling used in payloads and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::TurnEnd => "turn_end",
        }
    }

    /// Whether this event carries a tool name and arguments.
    pub fn supports_matcher(self) -> bool {
        matches!(self, Self::PreToolUse | Self::PostToolUse)
    }

    /// Whether a block decision has preventive meaning at this point.
    pub fn can_block(self) -> bool {
        matches!(self, Self::UserPromptSubmit | Self::PreToolUse)
    }
}

/// Session and optional turn identity supplied to a hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookContext {
    /// Session whose lifecycle is being intercepted.
    pub session_id: SessionId,
    /// Turn being intercepted, absent for session lifecycle events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
}

impl HookContext {
    /// Context for a session-level event.
    pub fn session(session_id: SessionId) -> Self {
        Self {
            session_id,
            turn_id: None,
        }
    }

    /// Context for an event inside a turn.
    pub fn turn(session_id: SessionId, turn_id: TurnId) -> Self {
        Self {
            session_id,
            turn_id: Some(turn_id),
        }
    }
}

/// Structured input delivered to one hook invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookPayload {
    /// Lifecycle point being fired.
    pub event: HookEvent,
    /// Stable id of the receiving hook.
    pub hook_id: String,
    /// Session and turn correlation. Flattened to top-level `session_id` and
    /// `turn_id` fields on the JSON wire.
    #[serde(flatten)]
    pub context: HookContext,
    /// Invocation timestamp.
    pub ts: chrono::DateTime<chrono::Utc>,
    /// Event-specific data. Its documented shape is part of the public hook
    /// contract even though it stays JSON so executors can be language-neutral.
    pub data: serde_json::Value,
}

impl HookPayload {
    /// Build a payload for a particular hook.
    pub fn new(
        event: HookEvent,
        hook_id: impl Into<String>,
        context: HookContext,
        data: serde_json::Value,
    ) -> Self {
        Self {
            event,
            hook_id: hook_id.into(),
            context,
            ts: chrono::Utc::now(),
            data,
        }
    }
}

/// What a hook asks the engine to do.
///
/// Event-specific interpretation keeps the contract honest: prompt hooks may
/// replace `message`, pre-tool hooks may patch `arguments`, and post-tool
/// hooks may patch the result. Mutations and blocks returned at advisory
/// session/turn-end events are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookOutcome {
    /// Continue without changing anything.
    Allow,
    /// Apply an event-specific JSON patch, then continue the chain.
    Mutate {
        /// Event-specific fields to replace or merge.
        patch: serde_json::Value,
        /// Optional diagnostic reason for audit surfaces.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Stop a blockable action. On advisory events this is ignored.
    Block {
        /// Diagnostic reason visible to the model or host.
        reason: String,
        /// Optional safer message for a user-facing surface.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_message: Option<String>,
    },
    /// The executor itself failed.
    Error {
        /// Parse, timeout, sandbox, transport, or callback failure.
        message: String,
    },
}

/// Policy applied when a hook returns [`HookOutcome::Error`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookErrorPolicy {
    /// Treat the error as a block at a blockable event.
    Block,
    /// Continue silently.
    Allow,
    /// Continue and emit a durable `hook.warning` custom event.
    #[default]
    Warn,
}

/// Restricted matcher for tool hook events.
///
/// This intentionally matches Everruns' useful subset rather than embedding
/// a full JSONPath/glob language: exact or alternation/prefix tool names, a
/// `$.field.subfield` argument path, and a Rust regular expression.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HookMatcher {
    /// Exact tool name.
    pub tool_name: Option<String>,
    /// `a|b|prefix*` alternation.
    pub tool_name_glob: Option<String>,
    /// Dot-only JSON path into tool arguments.
    pub args_jsonpath: Option<String>,
    /// Regex that must match the extracted value.
    pub match_regex: Option<String>,
    /// Alternate spelling for a security-oriented matching rule. Like
    /// `match_regex`, the hook fires when it matches.
    pub deny_regex: Option<String>,
}

impl HookMatcher {
    /// Whether the matcher accepts a tool call.
    pub fn matches(&self, tool_name: &str, arguments: &serde_json::Value) -> bool {
        if let Some(exact) = &self.tool_name
            && exact != tool_name
        {
            return false;
        }
        if let Some(pattern) = &self.tool_name_glob
            && !pattern.split('|').any(|alternative| {
                let alternative = alternative.trim();
                alternative
                    .strip_suffix('*')
                    .map_or(alternative == tool_name, |prefix| {
                        tool_name.starts_with(prefix)
                    })
            })
        {
            return false;
        }

        let extracted = self
            .args_jsonpath
            .as_deref()
            .map(|path| extract_json_path(arguments, path));
        match (
            self.match_regex.as_deref(),
            self.deny_regex.as_deref(),
            extracted,
        ) {
            (None, None, None) => true,
            (None, None, Some(value)) => !value.is_empty(),
            (Some(pattern), None, Some(value)) | (None, Some(pattern), Some(value)) => {
                regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(&value))
            }
            _ => false,
        }
    }

    /// Reject contradictory or incomplete matcher shapes before an agent runs.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.match_regex.is_some() && self.deny_regex.is_some() {
            return Err("match_regex and deny_regex are mutually exclusive".into());
        }
        if (self.match_regex.is_some() || self.deny_regex.is_some()) && self.args_jsonpath.is_none()
        {
            return Err("a regex matcher requires args_jsonpath".into());
        }
        for pattern in [&self.match_regex, &self.deny_regex].into_iter().flatten() {
            regex::Regex::new(pattern)
                .map_err(|error| format!("invalid hook matcher regex: {error}"))?;
        }
        Ok(())
    }
}

fn extract_json_path(value: &serde_json::Value, path: &str) -> String {
    let mut current = value;
    for segment in path.strip_prefix("$.").unwrap_or(path).split('.') {
        if segment.is_empty() {
            continue;
        }
        let Some(next) = current.get(segment) else {
            return String::new();
        };
        current = next;
    }
    match current {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Null => String::new(),
        value => value.to_string(),
    }
}

/// One user hook attached by value to an agent.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Stable diagnostic id, also delivered in [`HookPayload::hook_id`].
    fn id(&self) -> &str;

    /// Lifecycle point this hook receives.
    fn event(&self) -> HookEvent;

    /// Optional tool matcher. Ignored for non-tool events.
    fn matcher(&self) -> Option<&HookMatcher> {
        None
    }

    /// What the engine should do if execution returns an error.
    fn on_error(&self) -> HookErrorPolicy {
        HookErrorPolicy::Warn
    }

    /// Execute the hook against a structured payload.
    async fn run(&self, payload: &HookPayload) -> HookOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matcher_combines_name_path_and_regex() {
        let matcher = HookMatcher {
            tool_name_glob: Some("bash|edit_*".into()),
            args_jsonpath: Some("$.command".into()),
            match_regex: Some(r"^rm\s+-rf".into()),
            ..HookMatcher::default()
        };
        assert!(matcher.matches("bash", &json!({"command": "rm -rf build"})));
        assert!(!matcher.matches("read_file", &json!({"command": "rm -rf build"})));
        assert!(!matcher.matches("bash", &json!({"command": "cargo test"})));
    }

    #[test]
    fn matcher_validation_rejects_ambiguous_regexes() {
        let matcher = HookMatcher {
            args_jsonpath: Some("$.command".into()),
            match_regex: Some("x".into()),
            deny_regex: Some("y".into()),
            ..HookMatcher::default()
        };
        assert!(matcher.validate().is_err());
    }
}
