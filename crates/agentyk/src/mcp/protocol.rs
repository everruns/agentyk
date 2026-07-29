//! MCP protocol-era selection and request metadata.
//!
//! Servers up to `2025-11-25` use an `initialize` handshake and may bind
//! later requests to an `Mcp-Session-Id`. `2026-07-28` is stateless:
//! protocol and client metadata travel with every operation instead.

use std::time::Duration;

use serde_json::{Map, Value, json};

/// Oldest MCP protocol version still offered during a handshake.
pub const MCP_PROTOCOL_VERSION_LEGACY: &str = "2025-03-26";
/// Newest stateful MCP protocol version — the handshake fallback. Servers
/// speaking an older revision negotiate down from it in their `initialize`
/// result, so the intermediate versions need no mode of their own.
pub const MCP_PROTOCOL_VERSION_STATEFUL: &str = "2025-11-25";
/// Newest MCP protocol version, and the first stateless one.
pub const MCP_PROTOCOL_VERSION_LATEST: &str = "2026-07-28";

/// Every stateful version we can speak, newest first. Wider than the set
/// [`McpProtocolMode`] can pin, because a server naming one of these in a
/// `supported` list is telling us it will accept it.
pub(super) const KNOWN_STATEFUL_VERSIONS: [&str; 3] = [
    MCP_PROTOCOL_VERSION_STATEFUL,
    "2025-06-18",
    MCP_PROTOCOL_VERSION_LEGACY,
];

/// `HeaderMismatchError` — routing headers disagree with the JSON body.
pub const ERROR_HEADER_MISMATCH: i64 = -32020;
/// `MissingRequiredClientCapabilityError` — the server needs a capability we
/// did not advertise.
pub const ERROR_MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
/// `UnsupportedProtocolVersionError` — carries the versions the server does
/// support in `data.supported`.
pub const ERROR_UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// The `server/discover` RPC every `2026-07-28` server must implement.
pub(super) const METHOD_DISCOVER: &str = "server/discover";

#[cfg(feature = "http")]
pub(super) const HEADER_METHOD: &str = "Mcp-Method";
#[cfg(feature = "http")]
pub(super) const HEADER_NAME: &str = "Mcp-Name";
#[cfg(feature = "http")]
pub(super) const HEADER_PROTOCOL_VERSION: &str = "MCP-Protocol-Version";
#[cfg(feature = "http")]
pub(super) const HEADER_SESSION_ID: &str = "Mcp-Session-Id";

const CLIENT_INFO_META_KEY: &str = "io.modelcontextprotocol/clientInfo";
const PROTOCOL_VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CLIENT_CAPABILITIES_META_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
pub(super) const SERVER_INFO_META_KEY: &str = "io.modelcontextprotocol/serverInfo";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Negotiated {
    pub version: String,
    pub stateful: bool,
    pub session_id: Option<String>,
}

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

/// A JSON-RPC error the spec allocates to MCP itself.
///
/// Seeing one identifies a *modern* server however the request failed: only
/// a `2026-07-28` implementation emits these codes, so the right response is
/// to retry on its terms rather than fall back to a handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpecError {
    pub code: i64,
    pub message: String,
    /// Versions offered by an [`ERROR_UNSUPPORTED_PROTOCOL_VERSION`].
    pub supported: Vec<String>,
}

/// Read a spec-allocated error out of a JSON-RPC envelope, if there is one.
pub(super) fn spec_error(envelope: &Value) -> Option<SpecError> {
    let error = envelope.get("error")?;
    let code = error.get("code").and_then(Value::as_i64)?;
    if !matches!(
        code,
        ERROR_HEADER_MISMATCH
            | ERROR_MISSING_REQUIRED_CLIENT_CAPABILITY
            | ERROR_UNSUPPORTED_PROTOCOL_VERSION
    ) {
        return None;
    }
    Some(SpecError {
        code,
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        supported: error["data"]["supported"]
            .as_array()
            .map(|versions| {
                versions
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Pick the best era we share with a server that listed what it supports.
///
/// Stateless wins when offered — it is the only version with no handshake to
/// pay for. Otherwise take the newest stateful version in common.
pub(super) fn negotiated_from_supported(supported: &[String]) -> Option<Negotiated> {
    let offers = |version: &str| supported.iter().any(|candidate| candidate == version);
    if offers(MCP_PROTOCOL_VERSION_LATEST) {
        return Some(Negotiated::stateless(MCP_PROTOCOL_VERSION_LATEST));
    }
    KNOWN_STATEFUL_VERSIONS
        .into_iter()
        .find(|version| offers(version))
        .map(Negotiated::stateful)
}

/// The freshness hint on a cacheable result, if the server gave a usable one.
pub(super) fn cache_ttl(result: &Value) -> Option<Duration> {
    let ttl_ms = result.get("ttlMs")?.as_u64()?;
    (ttl_ms > 0).then(|| Duration::from_millis(ttl_ms))
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

#[cfg(test)]
mod era_tests {
    use super::*;

    #[test]
    fn a_spec_allocated_code_identifies_a_modern_server() {
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": ERROR_UNSUPPORTED_PROTOCOL_VERSION,
                "message": "Unsupported protocol version",
                "data": {"supported": ["2026-07-28", "2025-11-25"], "requested": "1900-01-01"},
            },
        });
        let error = spec_error(&envelope).expect("a spec error");
        assert_eq!(error.code, ERROR_UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(error.supported, ["2026-07-28", "2025-11-25"]);
    }

    #[test]
    fn an_implementation_defined_code_is_not_a_spec_error() {
        // -32000..=-32019 stays with the implementation, and -32601 is plain
        // JSON-RPC. Neither says anything about the server's era.
        for code in [-32000, -32019, -32601, -32603] {
            let envelope = json!({"error": {"code": code, "message": "nope"}});
            assert!(spec_error(&envelope).is_none(), "code {code}");
        }
        assert!(spec_error(&json!({"result": {}})).is_none());
    }

    #[test]
    fn stateless_wins_when_a_server_offers_it() {
        let negotiated =
            negotiated_from_supported(&["2025-11-25".into(), "2026-07-28".into()]).unwrap();
        assert_eq!(negotiated.version, MCP_PROTOCOL_VERSION_LATEST);
        assert!(!negotiated.stateful);
    }

    #[test]
    fn the_newest_shared_stateful_version_is_taken_otherwise() {
        let negotiated =
            negotiated_from_supported(&["2025-03-26".into(), "2025-06-18".into()]).unwrap();
        assert_eq!(
            negotiated.version, "2025-06-18",
            "a version with no mode of its own is still one we can speak"
        );
        assert!(negotiated.stateful);
        assert_eq!(negotiated_from_supported(&["1900-01-01".into()]), None);
        assert_eq!(negotiated_from_supported(&[]), None);
    }

    #[test]
    fn a_cache_hint_is_read_only_when_it_is_usable() {
        assert_eq!(
            cache_ttl(&json!({"ttlMs": 3_600_000u64})),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(cache_ttl(&json!({"ttlMs": 0})), None, "expires immediately");
        assert_eq!(cache_ttl(&json!({"ttlMs": -5})), None, "not a duration");
        assert_eq!(cache_ttl(&json!({})), None, "an older server sends none");
    }
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
