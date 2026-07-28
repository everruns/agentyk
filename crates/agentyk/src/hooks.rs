//! Trusted local shell-backed user hooks.
//!
//! The portable [`Hook`] contract lives in core and
//! is always available. This module is opt-in because [`ShellHook`] spawns a
//! host process. It is suitable for local applications that trust the hook
//! configuration; a sandboxed or durable host should implement `Hook`
//! against its own executor instead.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use agentyk_core::hook::{Hook, HookErrorPolicy, HookEvent, HookMatcher, HookOutcome, HookPayload};
use async_trait::async_trait;
use tokio::process::Command;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_ENV_VARS: usize = 16;

/// A user hook executed by the host's `/bin/sh`.
///
/// The hook receives its JSON [`HookPayload`] both on stdin and in
/// `AGENTYK_HOOK_PAYLOAD_JSON`. Convenience variables provide the event,
/// hook id, session id, optional turn id, and (for tool events) tool name and
/// call id. Stdout follows the Everruns decision contract:
///
/// - empty stdout plus exit 0 means allow;
/// - empty stdout plus non-zero exit means block, using stderr as the reason;
/// - otherwise stdout is a JSON [`HookOutcome`] such as
///   `{"decision":"mutate","patch":{"arguments":{"path":"safe"}}}`.
///
/// Output is capped at 64 KiB and execution at 30 seconds. Commands are
/// trusted code with the application's own OS permissions; this type does not
/// claim to be a sandbox.
pub struct ShellHook {
    id: String,
    event: HookEvent,
    command: String,
    env: BTreeMap<String, String>,
    matcher: Option<HookMatcher>,
    timeout: Duration,
    on_error: HookErrorPolicy,
}

impl ShellHook {
    /// Create a hook with a five-second timeout and warn-on-error policy.
    pub fn new(id: impl Into<String>, event: HookEvent, command: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            event,
            command: command.into(),
            env: BTreeMap::new(),
            matcher: None,
            timeout: DEFAULT_TIMEOUT,
            on_error: HookErrorPolicy::Warn,
        }
    }

    /// Restrict a pre/post-tool hook to matching calls.
    pub fn matcher(mut self, matcher: HookMatcher) -> Self {
        self.matcher = Some(matcher);
        self
    }

    /// Add one environment variable visible to the command.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Set the execution timeout. Values are clamped to 100 ms–30 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.clamp(MIN_TIMEOUT, MAX_TIMEOUT);
        self
    }

    /// Choose how executor failures affect the intercepted action.
    pub fn on_error(mut self, policy: HookErrorPolicy) -> Self {
        self.on_error = policy;
        self
    }
}

#[async_trait]
impl Hook for ShellHook {
    fn id(&self) -> &str {
        &self.id
    }

    fn event(&self) -> HookEvent {
        self.event
    }

    fn matcher(&self) -> Option<&HookMatcher> {
        self.matcher.as_ref()
    }

    fn on_error(&self) -> HookErrorPolicy {
        self.on_error
    }

    async fn run(&self, payload: &HookPayload) -> HookOutcome {
        if self.command.len() > MAX_COMMAND_BYTES {
            return HookOutcome::Error {
                message: "hook command exceeded 16 KiB".into(),
            };
        }
        if self.env.len() > MAX_ENV_VARS {
            return HookOutcome::Error {
                message: "hook environment exceeded 16 variables".into(),
            };
        }
        let encoded = match serde_json::to_string(payload) {
            Ok(encoded) => encoded,
            Err(error) => {
                return HookOutcome::Error {
                    message: format!("could not serialize hook payload: {error}"),
                };
            }
        };

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&self.command)
            .envs(&self.env)
            .env("AGENTYK_HOOK_PAYLOAD_JSON", &encoded)
            .env("AGENTYK_HOOK_EVENT", payload.event.as_str())
            .env("AGENTYK_HOOK_ID", &payload.hook_id)
            .env(
                "AGENTYK_HOOK_SESSION_ID",
                payload.context.session_id.to_string(),
            )
            .env(
                "AGENTYK_HOOK_TURN_ID",
                payload
                    .context
                    .turn_id
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
            )
            .env(
                "AGENTYK_HOOK_TOOL_NAME",
                payload
                    .data
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
            )
            .env(
                "AGENTYK_HOOK_TOOL_CALL_ID",
                payload
                    .data
                    .get("tool_call_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return HookOutcome::Error {
                    message: format!("could not spawn hook: {error}"),
                };
            }
        };
        let stdout = child.stdout.take().expect("stdout was configured as piped");
        let stderr = child.stderr.take().expect("stderr was configured as piped");
        let stdin = child.stdin.take();
        let execution = async {
            let write_input = async {
                if let Some(mut stdin) = stdin {
                    use tokio::io::AsyncWriteExt;
                    if let Err(error) = stdin.write_all(encoded.as_bytes()).await {
                        // Hooks may intentionally decide without reading stdin. On fast
                        // platforms the child can exit before this write completes.
                        if error.kind() != std::io::ErrorKind::BrokenPipe {
                            return Err(format!("could not write hook payload: {error}"));
                        }
                    }
                }
                Ok::<_, String>(())
            };
            let (input, stdout, stderr) =
                tokio::join!(write_input, read_bounded(stdout), read_bounded(stderr));
            input?;
            let stdout = stdout?;
            let stderr = stderr?;
            let status = child
                .wait()
                .await
                .map_err(|error| format!("hook process failed: {error}"))?;
            Ok::<_, String>((status, stdout, stderr))
        };
        let (status, stdout, stderr) = match tokio::time::timeout(self.timeout, execution).await {
            Ok(Ok(output)) => output,
            Ok(Err(message)) => {
                return HookOutcome::Error { message };
            }
            Err(_) => {
                return HookOutcome::Error {
                    message: format!("hook timed out after {} ms", self.timeout.as_millis()),
                };
            }
        };
        if stdout.len().saturating_add(stderr.len()) > MAX_OUTPUT_BYTES {
            return HookOutcome::Error {
                message: "hook output exceeded 64 KiB".into(),
            };
        }
        parse_output(
            status.success(),
            String::from_utf8_lossy(&stdout).trim(),
            String::from_utf8_lossy(&stderr).trim(),
        )
    }
}

async fn read_bounded(mut reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;

    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("could not read hook output: {error}"))?;
        if read == 0 {
            return Ok(kept);
        }
        let remaining = (MAX_OUTPUT_BYTES + 1).saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

fn parse_output(success: bool, stdout: &str, stderr: &str) -> HookOutcome {
    if stdout.is_empty() {
        return if success {
            HookOutcome::Allow
        } else {
            HookOutcome::Block {
                reason: if stderr.is_empty() {
                    "hook exited unsuccessfully".into()
                } else {
                    stderr.into()
                },
                user_message: None,
            }
        };
    }
    if !stdout.starts_with('{') {
        return HookOutcome::Error {
            message: "hook stdout is not a JSON decision".into(),
        };
    }
    serde_json::from_str(stdout).unwrap_or_else(|error| HookOutcome::Error {
        message: format!("hook stdout JSON is invalid: {error}"),
    })
}
