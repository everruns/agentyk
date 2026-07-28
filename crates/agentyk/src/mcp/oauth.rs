//! OAuth 2.1 authorization for remote MCP servers.
//!
//! This module adopts yolop's MCP authorization flow while keeping
//! application concerns outside the library. It discovers authorization
//! metadata, dynamically registers a public client when needed, prepares a
//! PKCE browser login, receives the loopback callback, exchanges the code,
//! and refreshes tokens. The caller decides how to open the authorization URL
//! and where to persist the resulting token set.
//!
//! ```no_run
//! use std::sync::Arc;
//! use agentyk::{
//!     McpCapability, McpOAuthTokenProvider, McpServer, Result,
//!     prepare_mcp_oauth_login,
//! };
//!
//! # async fn connect() -> Result<McpCapability> {
//! let login = prepare_mcp_oauth_login("https://mcp.example.com/mcp", None, None).await?;
//! println!("Open {}", login.authorize_url());
//! let tokens = login.complete().await?;
//! let provider = Arc::new(McpOAuthTokenProvider::new(tokens));
//! let capability = McpCapability::new(McpServer::http(
//!     "example",
//!     "https://mcp.example.com/mcp",
//! ))
//! .auth_arc(provider);
//! # Ok(capability)
//! # }
//! ```

use std::collections::HashMap;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use rand::RngCore as _;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use agentyk_core::error::{Error, Result};

use super::McpAuthProvider;

const CALLBACK_PATH: &str = "/callback";
const REFRESH_SKEW: Duration = Duration::seconds(60);

/// OAuth tokens and the metadata required to refresh them.
///
/// The value is serializable so a host can persist it in its own credential
/// store. `client_secret`, access tokens, and refresh tokens are secrets and
/// must not be written to logs or unencrypted storage.
#[derive(Clone, Serialize, Deserialize)]
pub struct McpOAuthTokenSet {
    /// Token sent to the MCP server.
    pub access_token: String,
    /// Token used to renew the access token, when the server issued one.
    pub refresh_token: Option<String>,
    /// Authorization scheme, normally `Bearer`.
    pub token_type: String,
    /// Absolute access-token expiry, when supplied by the server.
    pub expires_at: Option<DateTime<Utc>>,
    /// Granted OAuth scopes.
    pub scope: Option<String>,
    /// Token endpoint discovered during login.
    pub token_endpoint: String,
    /// OAuth client id used during login.
    pub client_id: String,
    /// Dynamic-registration client secret, when the server issued one.
    pub client_secret: Option<String>,
}

impl McpOAuthTokenSet {
    /// Whether the access token has expired at `now`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }
}

impl std::fmt::Debug for McpOAuthTokenSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthTokenSet")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("token_type", &self.token_type)
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// A refreshing [`McpAuthProvider`] backed by an OAuth token set.
///
/// Attach this to [`super::McpCapability::auth`]. It checks the token before
/// every request and serializes refreshes so concurrent MCP calls do not race.
pub struct McpOAuthTokenProvider {
    tokens: Mutex<McpOAuthTokenSet>,
}

impl McpOAuthTokenProvider {
    /// Create a provider from a completed login or a persisted token set.
    pub fn new(tokens: McpOAuthTokenSet) -> Self {
        Self {
            tokens: Mutex::new(tokens),
        }
    }

    /// Return the latest tokens, including any rotation performed by refresh.
    ///
    /// Hosts that persist credentials can call this after MCP activity.
    pub async fn tokens(&self) -> McpOAuthTokenSet {
        self.tokens.lock().await.clone()
    }
}

#[async_trait]
impl McpAuthProvider for McpOAuthTokenProvider {
    async fn authorization(&self, _server: &str) -> Result<Option<String>> {
        let mut tokens = self.tokens.lock().await;
        if needs_refresh(&tokens) {
            *tokens = refresh_mcp_oauth_tokens(&tokens).await?;
        }
        Ok(Some(format!(
            "{} {}",
            tokens.token_type, tokens.access_token
        )))
    }
}

#[derive(Debug, Clone)]
struct OAuthEndpoints {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct OAuthClient {
    client_id: String,
    client_secret: Option<String>,
}

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Deserialize)]
struct AuthorizationServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

/// A prepared browser login for an MCP server.
///
/// Display or open [`Self::authorize_url`], then call [`Self::complete`] to
/// wait for the loopback redirect and exchange its authorization code.
pub struct PreparedMcpOAuthLogin {
    authorize_url: String,
    listener: TcpListener,
    state: String,
    verifier: String,
    redirect_uri: String,
    endpoints: OAuthEndpoints,
    client: OAuthClient,
    scope: Option<String>,
}

impl PreparedMcpOAuthLogin {
    /// URL the user must visit to authorize the MCP client.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// Wait for the loopback callback and exchange the code for tokens.
    pub async fn complete(self) -> Result<McpOAuthTokenSet> {
        let code = wait_for_callback(self.listener, &self.state).await?;
        exchange_code(
            &http_client()?,
            &self.endpoints,
            &self.client,
            &code,
            &self.verifier,
            &self.redirect_uri,
            self.scope.as_deref(),
        )
        .await
    }
}

/// Discover an MCP server's OAuth endpoints and prepare a PKCE browser login.
///
/// When `client_id` is absent, the authorization server must advertise RFC
/// 7591 dynamic client registration. When `scope` is absent, all advertised
/// scopes are requested. This binds an ephemeral loopback callback before
/// returning so the exact redirect URI can be registered.
pub async fn prepare_mcp_oauth_login(
    mcp_url: &str,
    client_id: Option<&str>,
    scope: Option<&str>,
) -> Result<PreparedMcpOAuthLogin> {
    let http = http_client()?;
    let endpoints = discover_endpoints(&http, mcp_url).await?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| oauth_error(format!("bind loopback OAuth callback: {error}")))?;
    let port = listener
        .local_addr()
        .map_err(|error| oauth_error(format!("resolve OAuth callback port: {error}")))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let client = match client_id {
        Some(client_id) => OAuthClient {
            client_id: client_id.to_string(),
            client_secret: None,
        },
        None => register_client(&http, &endpoints, &redirect_uri).await?,
    };
    let verifier = random_pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = random_token(32);
    let scope = scope.map(str::to_string).or_else(|| {
        endpoints
            .scopes_supported
            .as_ref()
            .map(|scopes| scopes.join(" "))
    });
    let authorize_url = authorize_url(
        &endpoints,
        &client.client_id,
        &redirect_uri,
        &challenge,
        &state,
        scope.as_deref(),
    )?;
    Ok(PreparedMcpOAuthLogin {
        authorize_url: authorize_url.to_string(),
        listener,
        state,
        verifier,
        redirect_uri,
        endpoints,
        client,
        scope,
    })
}

/// Renew an OAuth access token, preserving refresh metadata omitted by the
/// token endpoint.
pub async fn refresh_mcp_oauth_tokens(tokens: &McpOAuthTokenSet) -> Result<McpOAuthTokenSet> {
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .ok_or_else(|| oauth_error("stored token has no refresh token"))?;
    require_secure(&tokens.token_endpoint, "token endpoint")?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", tokens.client_id.as_str()),
    ];
    if let Some(secret) = tokens.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let response = post_token(&http_client()?, &tokens.token_endpoint, &form).await?;
    Ok(token_set(
        response,
        &tokens.token_endpoint,
        &OAuthClient {
            client_id: tokens.client_id.clone(),
            client_secret: tokens.client_secret.clone(),
        },
        tokens.refresh_token.clone(),
        tokens.scope.as_deref(),
    ))
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| oauth_error(format!("build OAuth HTTP client: {error}")))
}

fn oauth_error(message: impl Into<String>) -> Error {
    Error::Mcp(format!("OAuth: {}", message.into()))
}

fn require_secure(url: &str, label: &str) -> Result<Url> {
    let parsed = Url::parse(url)
        .map_err(|error| oauth_error(format!("parse {label} URL `{url}`: {error}")))?;
    let loopback = parsed
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if loopback => Ok(parsed),
        scheme => Err(oauth_error(format!(
            "{label} must use https (got {scheme} for a non-loopback host): {url}"
        ))),
    }
}

async fn discover_endpoints(client: &reqwest::Client, mcp_url: &str) -> Result<OAuthEndpoints> {
    let server = require_secure(mcp_url, "MCP server")?;
    let origin = origin_of(&server);
    let issuer = fetch_json::<ProtectedResourceMetadata>(
        client,
        &well_known(&origin, "oauth-protected-resource"),
    )
    .await?
    .and_then(|metadata| metadata.authorization_servers.into_iter().next())
    .unwrap_or_else(|| origin.clone());
    let issuer = require_secure(&issuer, "authorization server")?;
    let issuer_origin = origin_of(&issuer);

    for suffix in ["oauth-authorization-server", "openid-configuration"] {
        if let Some(metadata) =
            fetch_json::<AuthorizationServerMetadata>(client, &well_known(&issuer_origin, suffix))
                .await?
        {
            require_secure(&metadata.authorization_endpoint, "authorization endpoint")?;
            require_secure(&metadata.token_endpoint, "token endpoint")?;
            if let Some(endpoint) = metadata.registration_endpoint.as_deref() {
                require_secure(endpoint, "registration endpoint")?;
            }
            return Ok(OAuthEndpoints {
                authorization_endpoint: metadata.authorization_endpoint,
                token_endpoint: metadata.token_endpoint,
                registration_endpoint: metadata.registration_endpoint,
                scopes_supported: metadata.scopes_supported,
            });
        }
    }
    Err(oauth_error(format!(
        "no authorization-server metadata found for {issuer_origin}"
    )))
}

async fn register_client(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    redirect_uri: &str,
) -> Result<OAuthClient> {
    let endpoint = endpoints.registration_endpoint.as_deref().ok_or_else(|| {
        oauth_error(
            "server does not support dynamic client registration; provide an OAuth client id",
        )
    })?;
    let response = client
        .post(endpoint)
        .json(&serde_json::json!({
            "client_name": "agentyk",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .send()
        .await
        .map_err(|error| oauth_error(format!("register client: {error}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(oauth_error(format!(
            "dynamic client registration returned {status}: {body}"
        )));
    }
    let registration = response
        .json::<RegistrationResponse>()
        .await
        .map_err(|error| oauth_error(format!("parse client registration: {error}")))?;
    Ok(OAuthClient {
        client_id: registration.client_id,
        client_secret: registration.client_secret,
    })
}

fn authorize_url(
    endpoints: &OAuthEndpoints,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    scope: Option<&str>,
) -> Result<Url> {
    let mut url = Url::parse(&endpoints.authorization_endpoint)
        .map_err(|error| oauth_error(format!("parse authorization endpoint: {error}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    if let Some(scope) = scope {
        url.query_pairs_mut().append_pair("scope", scope);
    }
    Ok(url)
}

async fn exchange_code(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    oauth_client: &OAuthClient,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<McpOAuthTokenSet> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", oauth_client.client_id.as_str()),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = oauth_client.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let response = post_token(client, &endpoints.token_endpoint, &form).await?;
    Ok(token_set(
        response,
        &endpoints.token_endpoint,
        oauth_client,
        None,
        scope,
    ))
}

async fn post_token(
    client: &reqwest::Client,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let response = client
        .post(endpoint)
        .form(form)
        .send()
        .await
        .map_err(|error| oauth_error(format!("call token endpoint: {error}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(oauth_error(format!(
            "token endpoint returned {status}: {body}"
        )));
    }
    response
        .json()
        .await
        .map_err(|error| oauth_error(format!("parse token response: {error}")))
}

fn token_set(
    response: TokenResponse,
    token_endpoint: &str,
    client: &OAuthClient,
    fallback_refresh: Option<String>,
    fallback_scope: Option<&str>,
) -> McpOAuthTokenSet {
    McpOAuthTokenSet {
        access_token: response.access_token,
        refresh_token: response.refresh_token.or(fallback_refresh),
        token_type: response.token_type.unwrap_or_else(|| "Bearer".to_string()),
        expires_at: response
            .expires_in
            .filter(|seconds| *seconds > 0)
            .map(|seconds| Utc::now() + Duration::seconds(seconds)),
        scope: response
            .scope
            .or_else(|| fallback_scope.map(str::to_string)),
        token_endpoint: token_endpoint.to_string(),
        client_id: client.client_id.clone(),
        client_secret: client.client_secret.clone(),
    }
}

fn needs_refresh(tokens: &McpOAuthTokenSet) -> bool {
    tokens.refresh_token.is_some() && tokens.is_expired(Utc::now() + REFRESH_SKEW)
}

fn origin_of(url: &Url) -> String {
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

fn well_known(origin: &str, suffix: &str) -> String {
    format!("{origin}/.well-known/{suffix}")
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<T>> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| oauth_error(format!("GET {url}: {error}")))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(oauth_error(format!(
            "GET {url} returned {}",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map(Some)
        .map_err(|error| oauth_error(format!("parse JSON from {url}: {error}")))
}

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|error| oauth_error(format!("accept OAuth callback: {error}")))?;
        let request = read_request_line(&mut socket).await?;
        let path = request
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| oauth_error("invalid OAuth callback request"))?;
        let parsed = Url::parse(&format!("http://127.0.0.1{path}"))
            .map_err(|error| oauth_error(format!("parse OAuth callback: {error}")))?;
        let params: HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        if parsed.path() != CALLBACK_PATH {
            write_response(&mut socket, "404 Not Found", "Unexpected callback path.").await?;
            continue;
        }
        if params.get("state").map(String::as_str) != Some(expected_state) {
            write_response(
                &mut socket,
                "400 Bad Request",
                "Login rejected: the state did not match.",
            )
            .await?;
            return Err(oauth_error("callback state mismatch"));
        }
        if let Some(error) = params.get("error") {
            write_response(
                &mut socket,
                "400 Bad Request",
                "The authorization server rejected the login.",
            )
            .await?;
            return Err(oauth_error(format!(
                "authorization server returned `{error}`"
            )));
        }
        if let Some(code) = params.get("code") {
            write_response(
                &mut socket,
                "200 OK",
                "MCP login complete. You can return to the application.",
            )
            .await?;
            return Ok(code.clone());
        }
        write_response(
            &mut socket,
            "400 Bad Request",
            "The callback had no authorization code.",
        )
        .await?;
        return Err(oauth_error("callback missing authorization code"));
    }
}

async fn read_request_line(socket: &mut TcpStream) -> Result<String> {
    let mut buffer = vec![0; 8192];
    let read = socket
        .read(&mut buffer)
        .await
        .map_err(|error| oauth_error(format!("read OAuth callback: {error}")))?;
    Ok(String::from_utf8_lossy(&buffer[..read])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string())
}

async fn write_response(socket: &mut TcpStream, status: &str, message: &str) -> Result<()> {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>MCP OAuth</title><h1>{}</h1><p>{}</p>",
        if status.starts_with("200") {
            "MCP login complete"
        } else {
            "MCP login failed"
        },
        html_escape(message)
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .map_err(|error| oauth_error(format!("write OAuth callback: {error}")))
}

fn random_pkce_verifier() -> String {
    random_token(48)
}

fn random_token(bytes: usize) -> String {
    let mut value = vec![0; bytes];
    rand::rng().fill_bytes(&mut value);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value)
}

fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockOAuthServer {
        base_url: String,
    }

    impl MockOAuthServer {
        async fn start() -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let task_base_url = base_url.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let base_url = task_base_url.clone();
                    tokio::spawn(async move {
                        let mut bytes = vec![0; 8192];
                        let read = socket.read(&mut bytes).await.unwrap();
                        let request = String::from_utf8_lossy(&bytes[..read]);
                        let first_line = request.lines().next().unwrap_or_default();
                        let mut parts = first_line.split_whitespace();
                        let method = parts.next().unwrap_or_default();
                        let path = parts.next().unwrap_or_default();
                        let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
                        let response_body = match (method, path) {
                            ("GET", "/.well-known/oauth-protected-resource") => {
                                format!(r#"{{"authorization_servers":["{base_url}"]}}"#)
                            }
                            ("GET", "/.well-known/oauth-authorization-server") => format!(
                                r#"{{"authorization_endpoint":"{base_url}/authorize","token_endpoint":"{base_url}/token","registration_endpoint":"{base_url}/register","scopes_supported":["read"]}}"#
                            ),
                            ("POST", "/register") => r#"{"client_id":"registered-client"}"#.into(),
                            ("POST", "/token")
                                if body.contains("grant_type=refresh_token") =>
                            {
                                r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600}"#.into()
                            }
                            ("POST", "/token") => {
                                r#"{"access_token":"access-1","refresh_token":"refresh-1","token_type":"Bearer","expires_in":3600,"scope":"read"}"#.into()
                            }
                            _ => r#"{"error":"not_found"}"#.into(),
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                            response_body.len()
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                    });
                }
            });
            Self { base_url }
        }
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn oauth_tokens_do_not_leak_through_debug() {
        let tokens = McpOAuthTokenSet {
            access_token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            token_type: "Bearer".into(),
            expires_at: None,
            scope: None,
            token_endpoint: "https://auth.example/token".into(),
            client_id: "client".into(),
            client_secret: Some("client-secret".into()),
        };
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
        assert!(!debug.contains("client-secret"));
    }

    #[test]
    fn rejects_plaintext_non_loopback_endpoints() {
        assert!(require_secure("https://mcp.example/mcp", "server").is_ok());
        assert!(require_secure("http://127.0.0.1:8080/mcp", "server").is_ok());
        assert!(require_secure("http://mcp.example/mcp", "server").is_err());
    }

    #[test]
    fn authorize_url_contains_pkce_state_and_scope() {
        let endpoints = OAuthEndpoints {
            authorization_endpoint: "https://auth.example/authorize".into(),
            token_endpoint: "https://auth.example/token".into(),
            registration_endpoint: None,
            scopes_supported: None,
        };
        let url = authorize_url(
            &endpoints,
            "client",
            "http://127.0.0.1:9/callback",
            "challenge",
            "state",
            Some("read write"),
        )
        .unwrap();
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["state"], "state");
        assert_eq!(pairs["scope"], "read write");
    }

    #[test]
    fn refreshes_only_when_expiring_and_refreshable() {
        let mut tokens = McpOAuthTokenSet {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            token_type: "Bearer".into(),
            expires_at: Some(Utc::now() + Duration::seconds(10)),
            scope: None,
            token_endpoint: "https://auth.example/token".into(),
            client_id: "client".into(),
            client_secret: None,
        };
        assert!(needs_refresh(&tokens));
        tokens.refresh_token = None;
        assert!(!needs_refresh(&tokens));
    }

    #[tokio::test]
    async fn login_discovers_registers_receives_callback_and_exchanges() {
        let server = MockOAuthServer::start().await;
        let prepared = prepare_mcp_oauth_login(&format!("{}/mcp", server.base_url), None, None)
            .await
            .unwrap();
        let authorize_url = Url::parse(prepared.authorize_url()).unwrap();
        let parameters: HashMap<_, _> = authorize_url.query_pairs().into_owned().collect();
        assert_eq!(parameters["client_id"], "registered-client");
        assert_eq!(parameters["code_challenge_method"], "S256");
        assert_eq!(parameters["scope"], "read");

        let redirect_uri = parameters["redirect_uri"].clone();
        let state = parameters["state"].clone();
        let completing = tokio::spawn(prepared.complete());
        reqwest::get(format!("{redirect_uri}?code=the-code&state={state}"))
            .await
            .unwrap();
        let tokens = completing.await.unwrap().unwrap();
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(tokens.client_id, "registered-client");
    }

    #[tokio::test]
    async fn token_provider_refreshes_before_returning_authorization() {
        let server = MockOAuthServer::start().await;
        let provider = McpOAuthTokenProvider::new(McpOAuthTokenSet {
            access_token: "expired".into(),
            refresh_token: Some("refresh-1".into()),
            token_type: "Bearer".into(),
            expires_at: Some(Utc::now() - Duration::seconds(1)),
            scope: Some("read".into()),
            token_endpoint: format!("{}/token", server.base_url),
            client_id: "registered-client".into(),
            client_secret: None,
        });
        assert_eq!(
            provider.authorization("demo").await.unwrap().as_deref(),
            Some("Bearer access-2")
        );
        assert_eq!(
            provider.tokens().await.refresh_token.as_deref(),
            Some("refresh-2")
        );
    }
}
