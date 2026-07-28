//! MCP protocol-era selection and request metadata.
//!
//! HTTP servers in the `2025-03-26` and `2025-06-18` eras use an
//! `initialize` handshake and may bind later requests to an
//! `Mcp-Session-Id`. The `2026-07-28` release candidate is stateless:
//! protocol and client metadata travel with every operation instead.

#[cfg(feature = "http")]
use serde_json::{Map, Value, json};

/// Legacy stateful MCP protocol version.
pub const MCP_PROTOCOL_VERSION_LEGACY: &str = "2025-03-26";
/// Current stable stateful MCP protocol version.
pub const MCP_PROTOCOL_VERSION_STABLE: &str = "2025-06-18";
/// Stateless MCP release-candidate protocol version.
pub const MCP_PROTOCOL_VERSION_RC: &str = "2026-07-28";

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

/// Policy for selecting the MCP protocol era used by an HTTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpProtocolMode {
    /// Probe the stateless release candidate, then fall back to a stateful
    /// handshake when the server explicitly requires it.
    #[default]
    Auto,
    /// Pin the legacy `2025-03-26` stateful handshake.
    Legacy,
    /// Pin the stable `2025-06-18` stateful handshake.
    Stable,
    /// Pin the stateless `2026-07-28` release candidate.
    Rc,
}

impl McpProtocolMode {
    #[cfg(feature = "http")]
    pub(super) fn initial(self) -> Option<Negotiated> {
        match self {
            Self::Auto => None,
            Self::Rc => Some(Negotiated::stateless(MCP_PROTOCOL_VERSION_RC)),
            Self::Stable => Some(Negotiated::stateful(MCP_PROTOCOL_VERSION_STABLE)),
            Self::Legacy => Some(Negotiated::stateful(MCP_PROTOCOL_VERSION_LEGACY)),
        }
    }

    pub(super) fn handshake_version(self) -> Option<&'static str> {
        match self {
            Self::Stable => Some(MCP_PROTOCOL_VERSION_STABLE),
            Self::Legacy => Some(MCP_PROTOCOL_VERSION_LEGACY),
            Self::Auto | Self::Rc => None,
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

#[cfg(feature = "http")]
pub(super) fn request_meta() -> Value {
    let mut meta = Map::new();
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
pub(super) fn with_request_meta(mut params: Value) -> Value {
    let Value::Object(ref mut object) = params else {
        return params;
    };
    object.insert("_meta".to_string(), request_meta());
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
    fn request_metadata_carries_client_identity() {
        let params = with_request_meta(json!({"name": "search"}));
        assert_eq!(
            params["_meta"][CLIENT_INFO_META_KEY]["name"],
            json!("agentyk")
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
        let rc = McpProtocolMode::Rc.initial().unwrap();
        assert_eq!(rc.version, MCP_PROTOCOL_VERSION_RC);
        assert!(!rc.stateful);

        for (mode, version) in [
            (McpProtocolMode::Stable, MCP_PROTOCOL_VERSION_STABLE),
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
