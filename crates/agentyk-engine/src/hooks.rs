//! Canonical interpretation of the host-neutral user-hook contract.

use std::sync::Arc;

use agentyk_core::event::EventData;
use agentyk_core::hook::{Hook, HookContext, HookErrorPolicy, HookEvent, HookOutcome, HookPayload};
use agentyk_core::message::{ContentPart, Message, ToolCall};
use agentyk_core::tool::ToolOutput;
use serde_json::{Value, json};

pub(crate) enum PromptDecision {
    Continue(Box<Message>),
    Block {
        reason: String,
        user_message: Option<String>,
    },
}

pub(crate) enum ToolDecision {
    Continue(ToolCall),
    Block {
        call: ToolCall,
        reason: String,
        user_message: Option<String>,
    },
}

pub(crate) async fn run_prompt_hooks(
    hooks: &[Arc<dyn Hook>],
    context: HookContext,
    mut message: Message,
) -> (PromptDecision, Vec<EventData>) {
    let mut warnings = Vec::new();
    for hook in hooks_for(hooks, HookEvent::UserPromptSubmit, None) {
        let payload = HookPayload::new(
            HookEvent::UserPromptSubmit,
            hook.id(),
            context,
            json!({
                "message": message.text(),
                "attachments": message
                    .content
                    .iter()
                    .filter(|part| matches!(part, ContentPart::Image(_)))
                    .collect::<Vec<_>>(),
            }),
        );
        match hook.run(&payload).await {
            HookOutcome::Allow => {}
            HookOutcome::Mutate { patch, .. } => {
                if let Some(text) = patch.get("message").and_then(Value::as_str) {
                    replace_message_text(&mut message, text);
                }
            }
            HookOutcome::Block {
                reason,
                user_message,
            } => {
                return (
                    PromptDecision::Block {
                        reason,
                        user_message,
                    },
                    warnings,
                );
            }
            HookOutcome::Error { message: error } => match hook.on_error() {
                HookErrorPolicy::Block => {
                    return (
                        PromptDecision::Block {
                            reason: format!("hook `{}` failed: {error}", hook.id()),
                            user_message: None,
                        },
                        warnings,
                    );
                }
                HookErrorPolicy::Allow => {}
                HookErrorPolicy::Warn => warnings.push(warning(hook.as_ref(), error)),
            },
        }
    }
    (PromptDecision::Continue(Box::new(message)), warnings)
}

pub(crate) async fn run_pre_tool_hooks(
    hooks: &[Arc<dyn Hook>],
    context: HookContext,
    mut call: ToolCall,
) -> (ToolDecision, Vec<EventData>) {
    let mut warnings = Vec::new();
    for hook in hooks {
        if hook.event() != HookEvent::PreToolUse
            || hook
                .matcher()
                .is_some_and(|matcher| !matcher.matches(&call.name, &call.arguments))
        {
            continue;
        }
        let payload = HookPayload::new(
            HookEvent::PreToolUse,
            hook.id(),
            context,
            json!({
                "tool_name": call.name,
                "tool_call_id": call.id,
                "arguments": call.arguments,
            }),
        );
        match hook.run(&payload).await {
            HookOutcome::Allow => {}
            HookOutcome::Mutate { patch, .. } => {
                if let Some(arguments) = patch.get("arguments") {
                    merge_json(&mut call.arguments, arguments);
                }
            }
            HookOutcome::Block {
                reason,
                user_message,
            } => {
                return (
                    ToolDecision::Block {
                        call,
                        reason,
                        user_message,
                    },
                    warnings,
                );
            }
            HookOutcome::Error { message: error } => match hook.on_error() {
                HookErrorPolicy::Block => {
                    return (
                        ToolDecision::Block {
                            call,
                            reason: format!("hook `{}` failed: {error}", hook.id()),
                            user_message: None,
                        },
                        warnings,
                    );
                }
                HookErrorPolicy::Allow => {}
                HookErrorPolicy::Warn => warnings.push(warning(hook.as_ref(), error)),
            },
        }
    }
    (ToolDecision::Continue(call), warnings)
}

pub(crate) async fn run_post_tool_hooks(
    hooks: &[Arc<dyn Hook>],
    context: HookContext,
    call: &ToolCall,
    mut output: ToolOutput,
) -> (ToolOutput, Vec<EventData>) {
    let mut warnings = Vec::new();
    for hook in hooks_for(hooks, HookEvent::PostToolUse, Some(call)) {
        let payload = HookPayload::new(
            HookEvent::PostToolUse,
            hook.id(),
            context,
            json!({
                "tool_name": call.name,
                "tool_call_id": call.id,
                "arguments": call.arguments,
                "result": output.content,
                "error": output.is_error.then_some(&output.content),
                "success": !output.is_error,
            }),
        );
        match hook.run(&payload).await {
            HookOutcome::Allow => {}
            HookOutcome::Mutate { patch, .. } => apply_output_patch(&mut output, &patch),
            HookOutcome::Block { .. } => {
                // A completed side effect cannot be made un-happen. Everruns
                // treats post-tool block as advisory, so Agentyk does too.
            }
            HookOutcome::Error { message: error } => match hook.on_error() {
                // Post-tool use is not blockable. Block-on-error can only turn
                // the result into an error the model sees.
                HookErrorPolicy::Block => {
                    output = ToolOutput::error(format!("hook `{}` failed: {error}", hook.id()));
                }
                HookErrorPolicy::Allow => {}
                HookErrorPolicy::Warn => warnings.push(warning(hook.as_ref(), error)),
            },
        }
    }
    (output, warnings)
}

pub(crate) async fn run_advisory_hooks(
    hooks: &[Arc<dyn Hook>],
    event: HookEvent,
    context: HookContext,
    data: Value,
) -> Vec<EventData> {
    debug_assert!(matches!(
        event,
        HookEvent::SessionStart | HookEvent::SessionEnd | HookEvent::TurnEnd
    ));
    let mut warnings = Vec::new();
    for hook in hooks_for(hooks, event, None) {
        let payload = HookPayload::new(event, hook.id(), context, data.clone());
        if let HookOutcome::Error { message } = hook.run(&payload).await
            && hook.on_error() == HookErrorPolicy::Warn
        {
            warnings.push(warning(hook.as_ref(), message));
        }
    }
    warnings
}

fn hooks_for<'a>(
    hooks: &'a [Arc<dyn Hook>],
    event: HookEvent,
    call: Option<&ToolCall>,
) -> impl Iterator<Item = &'a Arc<dyn Hook>> {
    hooks.iter().filter(move |hook| {
        hook.event() == event
            && match (hook.matcher(), call) {
                (Some(matcher), Some(call)) => matcher.matches(&call.name, &call.arguments),
                (Some(_), None) => !event.supports_matcher(),
                _ => true,
            }
    })
}

fn warning(hook: &dyn Hook, message: String) -> EventData {
    EventData::custom(
        agentyk_core::event::event_types::HOOK_WARNING,
        json!({
            "hook_id": hook.id(),
            "event": hook.event().as_str(),
            "message": message,
        }),
    )
}

fn replace_message_text(message: &mut Message, replacement: &str) {
    let mut replaced = false;
    let mut content = Vec::with_capacity(message.content.len().max(1));
    for part in message.content.drain(..) {
        match part {
            ContentPart::Text(_) if !replaced => {
                if !replacement.is_empty() {
                    content.push(ContentPart::text(replacement));
                }
                replaced = true;
            }
            ContentPart::Text(_) => {}
            image @ ContentPart::Image(_) => content.push(image),
        }
    }
    if !replaced && !replacement.is_empty() {
        content.insert(0, ContentPart::text(replacement));
    }
    message.content = content;
}

fn merge_json(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                target.insert(key.clone(), value.clone());
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

fn apply_output_patch(output: &mut ToolOutput, patch: &Value) {
    if let Some(result) = patch.get("result") {
        output.content = result
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| result.to_string());
        output.is_error = false;
    }
    if let Some(error) = patch.get("error") {
        match error {
            Value::Null => output.is_error = false,
            Value::String(error) => {
                output.content = error.clone();
                output.is_error = true;
            }
            error => {
                output.content = error.to_string();
                output.is_error = true;
            }
        }
    }
    if let Some(context) = patch.get("additional_context").and_then(Value::as_str) {
        if !output.content.is_empty() {
            output.content.push('\n');
        }
        output.content.push_str(context);
    }
}
