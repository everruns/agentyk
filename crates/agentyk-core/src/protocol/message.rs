//! Chat messages exchanged with LLM drivers and reconstructed from the event
//! log. Mirrors the everruns message vocabulary (`Message`, `Role`,
//! `ToolCall`, `ContentPart`).

use serde::{Deserialize, Serialize};

/// Who a [`Message`] came from.
///
/// Deliberately exhaustive: a driver that failed to translate a new role
/// would silently drop part of the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Instructions framing the conversation. Most drivers send these
    /// out-of-band rather than as a message — see
    /// [`crate::driver::ChatRequest::system_prompt`].
    System,
    /// Input from whoever is talking to the agent.
    User,
    /// The model's own output: text, tool calls, or both.
    Assistant,
    /// A tool result addressed back to the model.
    Tool,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Driver-assigned call id, echoed back in the tool result.
    pub id: String,
    /// Which tool to run — matched against [`crate::tool::ToolDefinition`]
    /// names.
    pub name: String,
    /// Arguments as a JSON object. The model produces these, so treat them as
    /// untrusted: a tool validates its own arguments.
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Text content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextContentPart {
    /// The text itself.
    pub text: String,
}

/// Image content, as a URL or inline base64 data (mirrors everruns'
/// `ImageContentPart`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageContentPart {
    /// Where the image lives, for providers that fetch it themselves.
    /// Mutually exclusive with [`ImageContentPart::base64`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The image inline, base64-encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
    /// MIME type of the inline data, e.g. `"image/png"`. Required alongside
    /// `base64`; drivers assume PNG without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

impl ImageContentPart {
    /// An image the provider fetches for itself.
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            url: Some(url.into()),
            base64: None,
            media_type: None,
        }
    }

    /// An image sent inline, which needs no reachable URL.
    pub fn from_base64(base64: impl Into<String>, media_type: impl Into<String>) -> Self {
        Self {
            url: None,
            base64: Some(base64.into()),
            media_type: Some(media_type.into()),
        }
    }
}

/// A part of message content: text or an image. Tool calls and tool results
/// stay as dedicated [`Message`] fields rather than content-part variants —
/// a deliberate simplification over everruns' unified content-part model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Text.
    Text(TextContentPart),
    /// An image, by URL or inline.
    Image(ImageContentPart),
}

impl ContentPart {
    /// A text part.
    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text(TextContentPart { text: text.into() })
    }

    /// An image part the provider fetches by URL.
    pub fn image_url(url: impl Into<String>) -> Self {
        ContentPart::Image(ImageContentPart::from_url(url))
    }

    /// An image part sent inline.
    pub fn image_base64(base64: impl Into<String>, media_type: impl Into<String>) -> Self {
        ContentPart::Image(ImageContentPart::from_base64(base64, media_type))
    }

    /// The text of a text part, or `None` for anything else.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text(part) => Some(&part.text),
            ContentPart::Image(_) => None,
        }
    }
}

/// One message in a conversation.
///
/// This is both what drivers send to a provider and what replaying the event
/// log reconstructs, so anything that must survive a resume lives here rather
/// than beside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who produced it.
    pub role: Role,
    /// Content parts, in order. Text-only messages carry a single `Text`
    /// part (or none, for an empty message) — see [`Message::text`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ContentPart>,
    /// Tool calls requested by an assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For `Role::Tool` messages: the call this result answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Provider reasoning ("extended thinking") text, when the model emitted
    /// a reasoning block. First-class because it is **not** everruns-specific
    /// (OpenAI reasoning, Anthropic thinking) and it must round-trip back to
    /// the provider on subsequent turns for the exchange to stay valid —
    /// which an out-of-band store can't guarantee (the driver only ever sees
    /// `Message`). Empty for the common no-reasoning case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Opaque signature the provider requires to accept a replayed thinking
    /// block (e.g. Anthropic's `signature`). Paired with [`Message::thinking`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// Generic, serializable extension hatch — the message analogue of
    /// [`crate::event::EventData::Custom`]. Everruns-flavored richness a
    /// satellite crate owns (execution `phase`, narration hints, provider
    /// extras) rides here instead of growing typed core fields. `Null` for
    /// the common case.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl Message {
    fn text_content(text: impl Into<String>) -> Vec<ContentPart> {
        let text = text.into();
        if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(text)]
        }
    }

    fn new(role: Role, content: Vec<ContentPart>) -> Self {
        Self {
            role,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            thinking: None,
            thinking_signature: None,
            metadata: serde_json::Value::Null,
        }
    }

    /// A plain-text user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, Self::text_content(text))
    }

    /// A user message with arbitrary content parts (text, images).
    pub fn user_multimodal(content: Vec<ContentPart>) -> Self {
        Self::new(Role::User, content)
    }

    /// A plain-text assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::new(Role::Assistant, Self::text_content(text))
    }

    /// An assistant message requesting tools, with optional accompanying
    /// text. An empty `text` is normal: models often answer with calls alone.
    pub fn assistant_with_calls(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls,
            ..Self::new(Role::Assistant, Self::text_content(text))
        }
    }

    /// A tool's result, addressed back to the call it answers. The turn loop
    /// builds these from `tool.completed` events, so a resumed session
    /// reconstructs them identically.
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            tool_call_id: Some(call_id.into()),
            ..Self::new(Role::Tool, Self::text_content(text))
        }
    }

    /// Attach provider reasoning to this message — see [`Message::thinking`].
    pub fn with_thinking(mut self, thinking: impl Into<String>, signature: Option<String>) -> Self {
        self.thinking = Some(thinking.into());
        self.thinking_signature = signature;
        self
    }

    /// Attach extension metadata — see [`Message::metadata`].
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    /// The concatenation of every text part, in order. Empty if the message
    /// has no text content (e.g. an image-only message, or a tool-call-only
    /// assistant message).
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentPart::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    /// True if every content part is text (no images) — the common case,
    /// and the signal drivers use to decide whether they can send a plain
    /// string on the wire instead of a content-part array.
    pub fn is_text_only(&self) -> bool {
        self.content
            .iter()
            .all(|part| matches!(part, ContentPart::Text(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_constructors_round_trip_through_text() {
        assert_eq!(Message::user("hi").text(), "hi");
        assert_eq!(Message::assistant("").text(), "");
        assert!(Message::assistant("").content.is_empty());
    }

    #[test]
    fn multimodal_message_mixes_text_and_image() {
        let message = Message::user_multimodal(vec![
            ContentPart::text("what is this?"),
            ContentPart::image_url("https://example.com/cat.png"),
        ]);
        assert_eq!(message.text(), "what is this?");
        assert!(!message.is_text_only());
    }

    #[test]
    fn serde_roundtrip_preserves_content_parts() {
        let message = Message::user_multimodal(vec![
            ContentPart::text("a"),
            ContentPart::image_base64("Zm9v", "image/png"),
        ]);
        let json = serde_json::to_string(&message).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(message, back);
    }

    #[test]
    fn thinking_and_metadata_round_trip() {
        let message = Message::assistant("done")
            .with_thinking("let me consider…", Some("sig-abc".into()))
            .with_metadata(serde_json::json!({"phase": "final"}));
        let json = serde_json::to_string(&message).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(message, back);
        assert_eq!(back.thinking.as_deref(), Some("let me consider…"));
        assert_eq!(back.thinking_signature.as_deref(), Some("sig-abc"));
        assert_eq!(back.metadata["phase"], "final");
    }

    #[test]
    fn plain_messages_omit_the_new_fields_from_json() {
        // Back-compat: a message with no reasoning/metadata serializes exactly
        // as before, and pre-0.1.1 JSON (without these keys) still loads.
        let json = serde_json::to_string(&Message::user("hi")).unwrap();
        assert!(!json.contains("thinking"));
        assert!(!json.contains("metadata"));
        let back: Message =
            serde_json::from_str(r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#)
                .unwrap();
        assert_eq!(back, Message::user("hi"));
    }
}
