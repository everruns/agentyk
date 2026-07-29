//! MCP protocol-era selection and request metadata.
//!
//! Servers up to `2025-11-25` use an `initialize` handshake and may bind
//! later requests to an `Mcp-Session-Id`. `2026-07-28` is stateless:
//! protocol and client metadata travel with every operation instead.

#[cfg(feature = "http")]
use serde_json::{Map, Value, json};

/// Oldest MCP protocol version still offered during a handshake.
pub const MCP_PROTOCOL_VERSION_LEGACY: &str = "2025-03-26";
/// Newest stateful MCP protocol version — the handshake fallback. Servers
/// speaking an older revision negotiate down from it in their `initialize`
/// result, so the intermediate versions need no mode of their own.
pub const MCP_PROTOCOL_VERSION_STATEFUL: &str = "2025-11-25";
/// Newest MCP protocol version, and the first stateless one.
pub const MCP_PROTOCOL_VERSION_LATEST: &str = "2026-07-28";

#[cfg(feature = "http")]
pub(super) const HEADER_METHOD: &str = "Mcp-Method";
#[cfg(feature = "http")]
pub(super) const HEADER_NAME: &str = "Mcp-Name";
#[cfg(feature = "http")]
pub(super) const HEADER_PROTOCOL_VERSION: &str = "MCP-Protocol-Version";
#[cfg(feature = "http")]
pub(super) const HEADER_SESSION_ID: &str = "Mcp-Session-Id";

#[cfg(feature = "http")]
const CLIENT_INFO_META_KEY: &str = "io.modelcontextprotocol/clientInfo";
#[cfg(feature = "http")]
const PROTOCOL_VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";
#[cfg(feature = "http")]
const CLIENT_CAPABILITIES_META_KEY: &str = "io.modelcontextprotocol/clientCapabilities";

/// Policy for selecting the MCP protocol era used by an HTTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpProtocolMode {
    /// Probe the stateless protocol, then fall back to a stateful handshake
    /// when the server explicitly requires it.
    #[default]
    Auto,
    /// Pin the `2025-03-26` handshake, for servers that reject any protocol
    /// version they do not recognise instead of negotiating down.
    Legacy,
    /// Pin the stateful handshake, offering [`MCP_PROTOCOL_VERSION_STATEFUL`].
    Stateful,
    /// Pin the stateless [`MCP_PROTOCOL_VERSION_LATEST`] protocol.
    Latest,
}

impl McpProtocolMode {
    #[cfg(feature = "http")]
    pub(super) fn initial(self) -> Option<Negotiated> {
        match self {
            Self::Auto => None,
            Self::Latest => Some(Negotiated::stateless(MCP_PROTOCOL_VERSION_LATEST)),
            Self::Stateful => Some(Negotiated::stateful(MCP_PROTOCOL_VERSION_STATEFUL)),
            Self::Legacy => Some(Negotiated::stateful(MCP_PROTOCOL_VERSION_LEGACY)),
        }
    }

    pub(super) fn handshake_version(self) -> Option<&'static str> {
        match self {
            Self::Stateful => Some(MCP_PROTOCOL_VERSION_STATEFUL),
            Self::Legacy => Some(MCP_PROTOCOL_VERSION_LEGACY),
            Self::Auto | Self::Latest => None,
        }
    }
}

#[cfg(feature = "http")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Negotiated {
    pub version: String,
    pub stateful: bool,
    pub session_id: Option<String>,
}

#[cfg(feature = "http")]
impl Negotiated {
    pub fn stateless(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            stateful: false,
            session_id: None,
        }
    }

    pub fn stateful(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            stateful: true,
            session_id: None,
        }
    }
}

/// The `_meta` every request carries.
///
/// `2026-07-28` dropped the handshake, so the protocol version and the
/// client's capabilities have to travel with each request instead. We
/// implement neither roots, sampling, nor elicitation, so the capabilities
/// are empty — that is what lets a server tell up front that it cannot ask
/// us for input mid-call.
#[cfg(feature = "http")]
pub(super) fn request_meta(version: &str) -> Value {
    let mut meta = Map::new();
    meta.insert(
        PROTOCOL_VERSION_META_KEY.to_string(),
        Value::String(version.to_string()),
    );
    meta.insert(CLIENT_CAPABILITIES_META_KEY.to_string(), json!({}));
    meta.insert(
        CLIENT_INFO_META_KEY.to_string(),
        json!({
            "name": "agentyk",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    );
    Value::Object(meta)
}

#[cfg(feature = "http")]
pub(super) fn with_request_meta(mut params: Value, version: &str) -> Value {
    let Value::Object(ref mut object) = params else {
        return params;
    };
    // Merge rather than replace: a caller-supplied `_meta` (trace context,
    // say) has no reason to lose to ours.
    match object.get_mut("_meta") {
        Some(Value::Object(existing)) => {
            if let Value::Object(ours) = request_meta(version) {
                for (key, value) in ours {
                    existing.entry(key).or_insert(value);
                }
            }
        }
        _ => {
            object.insert("_meta".to_string(), request_meta(version));
        }
    }
    params
}

#[cfg(feature = "http")]
pub(super) fn routable_headers(
    negotiated: &Negotiated,
    method: &str,
    tool_name: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        (HEADER_PROTOCOL_VERSION, negotiated.version.clone()),
        (HEADER_METHOD, method.to_string()),
    ];
    if let Some(tool_name) = tool_name {
        headers.push((HEADER_NAME, tool_name.to_string()));
    }
    if let Some(session_id) = &negotiated.session_id {
        headers.push((HEADER_SESSION_ID, session_id.clone()));
    }
    headers
}

#[cfg(feature = "http")]
pub(super) fn looks_like_handshake_required(status: u16, body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if matches!(status, 400 | 404 | 405 | 409 | 426)
        && (lower.contains("session")
            || lower.contains("initialize")
            || lower.contains("mcp-session-id"))
    {
        return true;
    }
    if status == 400
        && (lower.contains("protocol version")
            || lower.contains("protocol-version")
            || lower.contains("protocolversion"))
        && (lower.contains("unsupported")
            || lower.contains("not supported")
            || lower.contains("invalid"))
    {
        return true;
    }
    lower.contains("server not initialized")
        || lower.contains("session required")
        || lower.contains("missing session")
        || lower.contains("not initialized")
}

#[cfg(all(test, feature = "http"))]
mod tests {
    use super::*;

    #[test]
    fn request_metadata_carries_the_fields_a_stateless_server_needs() {
        let params = with_request_meta(json!({"name": "search"}), MCP_PROTOCOL_VERSION_LATEST);
        assert_eq!(
            params["_meta"][CLIENT_INFO_META_KEY]["name"],
            json!("agentyk")
        );
        assert_eq!(
            params["_meta"][PROTOCOL_VERSION_META_KEY],
            json!(MCP_PROTOCOL_VERSION_LATEST),
            "no handshake means the version rides on every request"
        );
        assert_eq!(
            params["_meta"][CLIENT_CAPABILITIES_META_KEY],
            json!({}),
            "we support no roots, sampling, or elicitation"
        );
        assert_eq!(params["name"], json!("search"), "arguments are untouched");
    }

    #[test]
    fn caller_supplied_metadata_survives() {
        let params = with_request_meta(
            json!({"_meta": {"traceparent": "00-abc-def-01"}}),
            MCP_PROTOCOL_VERSION_LATEST,
        );
        assert_eq!(params["_meta"]["traceparent"], json!("00-abc-def-01"));
        assert_eq!(
            params["_meta"][PROTOCOL_VERSION_META_KEY],
            json!(MCP_PROTOCOL_VERSION_LATEST)
        );
    }

    #[test]
    fn handshake_detection_is_conservative() {
        assert!(looks_like_handshake_required(
            400,
            "Mcp-Session-Id header required"
        ));
        assert!(looks_like_handshake_required(
            400,
            "Unsupported protocol version"
        ));
        assert!(looks_like_handshake_required(
            200,
            r#"{"error":{"message":"Server not initialized"}}"#
        ));
        assert!(!looks_like_handshake_required(400, "invalid arguments"));
        assert!(!looks_like_handshake_required(500, "internal server error"));
    }

    #[test]
    fn pinned_modes_select_their_protocol_era() {
        let latest = McpProtocolMode::Latest.initial().unwrap();
        assert_eq!(latest.version, MCP_PROTOCOL_VERSION_LATEST);
        assert!(!latest.stateful);
        assert_eq!(McpProtocolMode::Latest.handshake_version(), None);

        for (mode, version) in [
            (McpProtocolMode::Stateful, MCP_PROTOCOL_VERSION_STATEFUL),
            (McpProtocolMode::Legacy, MCP_PROTOCOL_VERSION_LEGACY),
        ] {
            let negotiated = mode.initial().unwrap();
            assert_eq!(negotiated.version, version);
            assert!(negotiated.stateful);
            assert_eq!(mode.handshake_version(), Some(version));
        }
        assert!(McpProtocolMode::Auto.initial().is_none());
    }
}
