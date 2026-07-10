//! Chat messages exchanged with LLM drivers and reconstructed from the event
//! log. Mirrors the everruns message vocabulary (`Message`, `Role`,
//! `ToolCall`, `ContentPart`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// A tool result addressed back to the model.
    Tool,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Driver-assigned call id, echoed back in the tool result.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Text content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextContentPart {
    pub text: String,
}

/// Image content, as a URL or inline base64 data (mirrors everruns'
/// `ImageContentPart`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageContentPart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

impl ImageContentPart {
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            url: Some(url.into()),
            base64: None,
            media_type: None,
        }
    }

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
    Text(TextContentPart),
    Image(ImageContentPart),
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text(TextContentPart { text: text.into() })
    }

    pub fn image_url(url: impl Into<String>) -> Self {
        ContentPart::Image(ImageContentPart::from_url(url))
    }

    pub fn image_base64(base64: impl Into<String>, media_type: impl Into<String>) -> Self {
        ContentPart::Image(ImageContentPart::from_base64(base64, media_type))
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text(part) => Some(&part.text),
            ContentPart::Image(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
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

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Self::text_content(text),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// A user message with arbitrary content parts (text, images).
    pub fn user_multimodal(content: Vec<ContentPart>) -> Self {
        Self {
            role: Role::User,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Self::text_content(text),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant_with_calls(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: Self::text_content(text),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Self::text_content(text),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
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
}
