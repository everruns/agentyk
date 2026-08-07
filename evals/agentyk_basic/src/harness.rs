//! The subject under test: one real `agentyk` session per case.
//!
//! Every eval in this study shares this harness, so a case differs from
//! another only in its sample, its scorers, and the knobs in [`Harness`] —
//! never in how the agent was wired. The harness composes the library the way
//! a coding agent composes it (filesystem capability + a shell tool, an
//! approval middleware, an event log), runs the sample's turns **in one
//! session** so prompt caching and context growth are observable, and mines
//! the event stream for the metrics the scorers grade.
//!
//! What it reports, per case: tokens split into input / output / cache-read /
//! cache-write / reasoning, an estimated USD cost, wall-clock duration and
//! time-to-first-token, iterations, tool calls (names, arguments,
//! observations, errors, denials), and the workspace files after the run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentyk::providers;
use agentyk::{Provider, 
    Agent, AnthropicDriver, BearerAuth, ChatDriver, ChatRequest, ChatResponse, DeltaSink, Event,
    EventData, EventListener, FileSystemCapability, FnTool, InMemoryEventLog, ModelSpec,
    OpenAiDriver, RealDiskFileSystem, SimDriver, SimTurn, ToolCallDecision, ToolInvocation,
    ToolOutput, TurnMiddleware, Usage as AgentykUsage,
};
use async_trait::async_trait;
use mira::trajectory::{
    Agent as AtifAgent, Observation, ObservationResult, Step, StepMetrics, StepSource, ToolCall,
    Trajectory,
};
use mira::{RunCx, Sample, Transcript};
use serde_json::{Value, json};

/// Ceiling on how much of a command's output goes back to the model — the
/// same guard a real host applies, so cases can't be won or lost on how a
/// runaway `cat` filled the context window.
const MAX_TOOL_OUTPUT: usize = 8_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

const SYSTEM_PROMPT: &str = "\
You are a terse coding agent working in a single workspace directory.

Work by acting, not by narrating: read what you need, make the change, verify
it. Prefer `read_file`, `grep_files`, and `list_directory` over shell
equivalents — they are cheaper and always available. Use `run_command` for
everything else (builds, tests).

A tool call the operator declines is a decision, not an error: say what you
would have done and stop, do not route around it with another tool.

Keep replies to a few lines, and end with the answer the task asked for.";

/// The knobs a case may vary. Everything else about the agent is fixed, so a
/// difference between two cases is a difference in these values or in the
/// sample — never in unrelated wiring.
pub struct Harness {
    /// Offer the shell tool. Off isolates the filesystem capability.
    pub shell: bool,
    /// Tool names an approval middleware declines, with a reason.
    pub denied: Vec<String>,
    /// Take the driver's non-streaming path. The engine always calls
    /// `complete_streaming`, so this wraps the driver to answer from
    /// `complete` instead — an A/B of the two provider code paths.
    pub buffered: bool,
    /// Reason-step ceiling for a turn.
    pub max_iterations: usize,
}

impl Harness {
    /// Resolve the case's harness from its sample metadata and the `mode`
    /// axis, so a case is configured by data rather than by a code branch.
    pub fn resolve(sample: &Sample, cx: &RunCx) -> Self {
        let meta_bool = |key: &str, default: bool| {
            sample
                .metadata
                .get(key)
                .and_then(Value::as_bool)
                .unwrap_or(default)
        };
        Self {
            shell: meta_bool("shell", true),
            denied: sample
                .metadata
                .get("deny")
                .and_then(Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(|name| name.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            buffered: cx.param("mode") == Some("buffered"),
            max_iterations: sample
                .metadata
                .get("max_turns")
                .and_then(Value::as_u64)
                .map_or(cx.max_turns, |turns| turns as usize),
        }
    }
}

/// Run one case end to end and report it as a Mira transcript.
///
/// Never returns `Err`: a provider or wiring failure is reported as a failed
/// transcript so the report shows *which* case broke and why, instead of the
/// run aborting.
pub async fn run_case(sample: Sample, cx: RunCx) -> Transcript {
    let harness = Harness::resolve(&sample, &cx);
    let workspace = match seed_workspace(&sample) {
        Ok(workspace) => workspace,
        Err(error) => return infra_failure(format!("could not seed the workspace: {error}")),
    };

    let model = model_spec(&cx);
    let provider = match provider(&cx, &sample, harness.buffered) {
        Ok(provider) => provider,
        Err(error) => return infra_failure(error),
    };
    let recorder = Arc::new(Recorder::default());
    let agent = match build_agent(workspace.path(), model, provider, &harness, recorder.clone()) {
        Ok(agent) => agent,
        Err(error) => return infra_failure(format!("could not build the agent: {error}")),
    };

    let started = Instant::now();
    let mut session = agent.session_with_log(Arc::new(InMemoryEventLog::new()));
    let mut usage = AgentykUsage::default();
    let (mut iterations, mut tool_calls) = (0usize, 0usize);
    let mut response = String::new();
    let mut failure = None;

    // Every turn of the sample runs on the *same* session: that is what makes
    // prompt-cache reuse and context growth measurable at all.
    for turn in &sample.input {
        match session.run(turn.clone()).await {
            Ok(result) => {
                usage.add(result.usage);
                iterations += result.iterations;
                tool_calls += result.tool_calls;
                response = result.response;
            }
            Err(error) => {
                failure = Some(error.to_string());
                break;
            }
        }
    }
    let elapsed = started.elapsed();

    let collected = recorder.take();
    let mut transcript = Transcript::response(response);
    transcript.iterations = iterations;
    transcript.tool_calls_count = tool_calls;
    transcript.tool_calls = collected.tool_names();
    transcript.usage = mira_usage(&usage, &cx.target.model);
    transcript.timing.duration_ms = elapsed.as_millis() as u64;
    transcript.timing.time_to_first_token_ms = collected
        .first_delta
        .map(|at| at.duration_since(started).as_millis() as u64);
    transcript.files = read_workspace(workspace.path());
    transcript.metrics = metrics(&usage, &collected, sample.input.len(), elapsed);
    transcript.trajectory = Some(collected.trajectory(&cx.target.model));
    transcript.events = collected
        .events
        .iter()
        .filter_map(|event| serde_json::to_value(event).ok())
        .collect();
    if let Some(error) = failure {
        // A turn that failed is the agent under test failing, not infra — the
        // provider errors we can recognise are re-classified in `run_case`'s
        // caller-visible metrics, not here.
        transcript.error = Some(error);
    }
    transcript
}

fn infra_failure(message: String) -> Transcript {
    Transcript {
        error: Some(message),
        // Infra, not subject: a missing key or an unseedable workspace is the
        // study's problem, and grading the model down for it would be a lie.
        error_kind: mira::ErrorKind::Infra,
        ..Transcript::default()
    }
}

// ============================================================================
// Wiring
// ============================================================================

/// The matrix target's model, named against the service the target runs on.
///
/// A target may carry a `reasoning_effort` in its metadata, which rides onto
/// the [`ModelSpec`] — the `gpt-5.6` family needs `none` before it will accept
/// function tools on chat completions at all.
fn model_spec(cx: &RunCx) -> ModelSpec {
    let spec = match cx.target.provider.as_str() {
        "sim" => ModelSpec::llmsim(),
        other => ModelSpec::on(other, cx.target.model.clone()),
    };
    match cx
        .target
        .metadata
        .get("reasoning_effort")
        .and_then(Value::as_str)
    {
        Some(effort) => spec.reasoning_effort(effort),
        None => spec,
    }
}

/// The service that model runs on: endpoint, credential from the environment,
/// and the driver whose protocol it speaks.
///
/// `openrouter` is the case the split exists for — the same OpenAI Chat
/// Completions driver as `openai`, pointed somewhere else with a different
/// key, so the target proves the compatible path against a real gateway.
fn provider(cx: &RunCx, sample: &Sample, buffered: bool) -> Result<Provider, String> {
    let key = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| format!("{name} is not set"))
    };
    Ok(match cx.target.provider.as_str() {
        // The offline target: no key, no network, a scripted model. It exists
        // so the wiring this harness measures — capabilities, middleware,
        // events, trajectory projection — is graded in CI too, not only when
        // someone has provider credentials.
        "sim" => Provider::llmsim(SimDriver::new(script(sample))),
        // 16k: the 8k default truncates a multi-file edit mid-write.
        "anthropic" => providers::anthropic(key("ANTHROPIC_API_KEY")?)
            .with_driver_arc(wrap(AnthropicDriver::new().max_tokens(16_000), buffered)),
        "openai" => providers::openai(key("OPENAI_API_KEY")?)
            .with_driver_arc(wrap(OpenAiDriver::new(), buffered)),
        "openrouter" => Provider::from_driver("openrouter", wrap(OpenAiDriver::new(), buffered))
            .base_url("https://openrouter.ai/api/v1")
            .auth(BearerAuth::new(key("OPENROUTER_API_KEY")?)),
        other => return Err(format!("no agentyk provider for `{other}`")),
    })
}

fn build_agent(
    workspace: &Path,
    model: ModelSpec,
    provider: Provider,
    harness: &Harness,
    recorder: Arc<Recorder>,
) -> agentyk::Result<Agent> {
    let files = RealDiskFileSystem::new(workspace)?;
    let mut builder = Agent::builder()
        .name("agentyk-eval")
        .system_prompt(SYSTEM_PROMPT)
        .capability(FileSystemCapability::new(files))
        .middleware(DenyMiddleware(harness.denied.clone()))
        .listener_arc(recorder)
        .max_iterations(harness.max_iterations);

    builder = builder.model(model).provider(provider);

    if harness.shell {
        builder = builder.tool(run_command_tool(workspace.to_path_buf()));
    }
    builder.build()
}

/// Optionally hide a driver's incremental streaming, for the buffered preset.
fn wrap(driver: impl ChatDriver + 'static, buffered: bool) -> Arc<dyn ChatDriver> {
    match buffered {
        true => Arc::new(Buffered(driver)),
        false => Arc::new(driver),
    }
}

/// The scripted completions a `sim` case plays, from its sample's `script`
/// metadata: `[{"tool": "read_file", "args": {…}}, {"text": "…"}]`. A case
/// with no script gets a single text answer, which is enough to prove the turn
/// loop ran.
fn script(sample: &Sample) -> Vec<SimTurn> {
    let Some(steps) = sample.metadata.get("script").and_then(Value::as_array) else {
        return vec![SimTurn::text("(no script)")];
    };
    steps
        .iter()
        .map(|step| match step.get("tool").and_then(Value::as_str) {
            Some(tool) => {
                SimTurn::tool_call(tool, step.get("args").cloned().unwrap_or_else(|| json!({})))
            }
            None => SimTurn::text(
                step.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("(no text)"),
            ),
        })
        .collect()
}

/// Forces the non-streaming provider path. The engine only ever calls
/// `complete_streaming`, so without this wrapper the `complete` path — the one
/// third-party drivers are most likely to implement first — is never exercised
/// end to end.
struct Buffered<D>(D);

#[async_trait]
impl<D: ChatDriver + 'static> ChatDriver for Buffered<D> {
    async fn complete(&self, request: ChatRequest) -> agentyk::Result<ChatResponse> {
        self.0.complete(request).await
    }

    async fn complete_streaming(
        &self,
        request: ChatRequest,
        sink: &mut dyn DeltaSink,
    ) -> agentyk::Result<ChatResponse> {
        let response = self.0.complete(request).await?;
        let text = response.message.text();
        if !text.is_empty() {
            sink.delta(&text, &text).await?;
        }
        Ok(response)
    }
}

/// Declines the named tools, the way a host's approval prompt does when the
/// operator says no. Cases use it to test that a denial is respected rather
/// than routed around.
struct DenyMiddleware(Vec<String>);

#[async_trait]
impl TurnMiddleware for DenyMiddleware {
    fn name(&self) -> &str {
        "eval-approval"
    }

    async fn before_tool(&self, invocation: &ToolInvocation<'_>) -> ToolCallDecision {
        match self.0.iter().any(|name| name == &invocation.call.name) {
            true => ToolCallDecision::Deny {
                reason: "the operator declined this call".to_string(),
            },
            false => ToolCallDecision::Proceed,
        }
    }
}

fn run_command_tool(workspace: PathBuf) -> FnTool {
    FnTool::new(
        "run_command",
        "Run a bash command in the workspace directory. Use for builds, tests, \
         and anything the file tools cannot do.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The bash command line to run."}
            },
            "required": ["command"]
        }),
        move |arguments| {
            let workspace = workspace.clone();
            async move {
                let Some(command) = arguments["command"].as_str() else {
                    return ToolOutput::error("`command` is required and must be a string");
                };
                run_command(&workspace, command).await
            }
        },
    )
}

async fn run_command(workspace: &Path, command: &str) -> ToolOutput {
    let spawned = tokio::process::Command::new("bash")
        .arg("-lc")
        .arg(command)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();

    let output = match tokio::time::timeout(COMMAND_TIMEOUT, spawned).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return ToolOutput::error(format!("failed to run command: {error}")),
        Err(_) => {
            return ToolOutput::error(format!(
                "command timed out after {}s",
                COMMAND_TIMEOUT.as_secs()
            ));
        }
    };

    let mut body = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&stderr);
    }
    if body.trim().is_empty() {
        body = "(no output)".to_string();
    }
    let body = truncate(body, MAX_TOOL_OUTPUT);

    match output.status.success() {
        true => ToolOutput::text(body),
        false => ToolOutput::error(format!("exit status {:?}\n{body}", output.status.code())),
    }
}

fn truncate(mut text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let cut = text
        .char_indices()
        .nth(limit)
        .map_or(text.len(), |(index, _)| index);
    text.truncate(cut);
    text.push_str("\n… output truncated");
    text
}

// ============================================================================
// Observation — the event stream is the only source of per-case metrics
// ============================================================================

#[derive(Default)]
struct Recorder {
    inner: Mutex<Collected>,
}

#[derive(Default)]
struct Collected {
    events: Vec<Event>,
    /// When the first streamed token arrived — the latency a user feels.
    first_delta: Option<Instant>,
}

#[async_trait]
impl EventListener for Recorder {
    async fn on_event(&self, event: &Event) {
        let mut inner = self.inner.lock().expect("recorder poisoned");
        if matches!(event.data, EventData::OutputMessageDelta { .. }) {
            inner.first_delta.get_or_insert_with(Instant::now);
            // Deltas are ephemeral: keeping them would bloat the report with a
            // character-by-character replay of every answer.
            return;
        }
        inner.events.push(event.clone());
    }

    fn name(&self) -> &'static str {
        "agentyk-eval-recorder"
    }
}

impl Recorder {
    fn take(&self) -> Collected {
        std::mem::take(&mut *self.inner.lock().expect("recorder poisoned"))
    }
}

impl Collected {
    fn tool_names(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match &event.data {
                EventData::ToolStarted { call, .. } => Some(call.name.clone()),
                _ => None,
            })
            .collect()
    }

    fn tool_errors(&self) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(&event.data, EventData::ToolCompleted { is_error, .. } if *is_error))
            .count()
    }

    fn denied(&self) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event.data, EventData::ToolDenied { .. }))
            .count()
    }

    /// Project the agentyk event stream into an ATIF trajectory — Mira's
    /// primary structured contract, and what makes tool *arguments* and tool
    /// *observations* scorable rather than just tool names.
    fn trajectory(&self, model: &str) -> Trajectory {
        let mut agent = AtifAgent::new("agentyk", env!("CARGO_PKG_VERSION"));
        agent.model_name = Some(model.to_string());
        let mut trajectory = Trajectory::new(agent);
        let mut step_id = 0u64;
        for event in &self.events {
            match &event.data {
                EventData::InputMessage { message } => {
                    step_id += 1;
                    trajectory
                        .steps
                        .push(Step::new(step_id, StepSource::User, message.text()));
                }
                EventData::OutputMessageCompleted { message, usage, .. } => {
                    step_id += 1;
                    let mut step = Step::new(step_id, StepSource::Agent, message.text());
                    step.model_name = Some(model.to_string());
                    step.tool_calls = message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            ToolCall::new(
                                call.id.clone(),
                                call.name.clone(),
                                call.arguments.clone(),
                            )
                        })
                        .collect();
                    step.metrics = Some(StepMetrics {
                        prompt_tokens: Some(usage.input_tokens),
                        completion_tokens: Some(usage.output_tokens),
                        cached_tokens: Some(usage.cache_read_input_tokens),
                        ..StepMetrics::default()
                    });
                    trajectory.steps.push(step);
                }
                EventData::ToolCompleted {
                    call_id,
                    output,
                    is_error,
                    ..
                } => {
                    // Results attach to the agent step that asked for them,
                    // which is what joins an argument to its observation.
                    let Some(step) = trajectory.steps.last_mut() else {
                        continue;
                    };
                    let content = match is_error {
                        true => format!("error: {output}"),
                        false => output.clone(),
                    };
                    step.observation
                        .get_or_insert_with(Observation::default)
                        .results
                        .push(ObservationResult {
                            source_call_id: Some(call_id.clone()),
                            content: Some(truncate(content, 2_000).into()),
                            ..ObservationResult::default()
                        });
                }
                _ => {}
            }
        }
        trajectory
    }
}

// ============================================================================
// Metrics
// ============================================================================

/// Open-vocabulary metrics every case reports, on top of the typed usage and
/// timing. These are what `metric_within` / `metric_at_least` grade and what
/// the reports compare across the matrix.
fn metrics(
    usage: &AgentykUsage,
    collected: &Collected,
    turns: usize,
    elapsed: Duration,
) -> BTreeMap<String, f64> {
    let input = usage.input_tokens as f64;
    let mut metrics = BTreeMap::new();
    metrics.insert("turns".into(), turns as f64);
    metrics.insert("input_tokens".into(), input);
    metrics.insert("output_tokens".into(), usage.output_tokens as f64);
    metrics.insert(
        "cache_read_tokens".into(),
        usage.cache_read_input_tokens as f64,
    );
    metrics.insert(
        "cache_write_tokens".into(),
        usage.cache_creation_input_tokens as f64,
    );
    metrics.insert("reasoning_tokens".into(), usage.reasoning_tokens as f64);
    // The headline prompt-cache number: the share of prompt tokens that were
    // served from cache rather than re-read. 0 means caching bought nothing.
    metrics.insert(
        "cache_hit_ratio".into(),
        match input > 0.0 {
            true => usage.cache_read_input_tokens as f64 / input,
            false => 0.0,
        },
    );
    metrics.insert("tool_errors".into(), collected.tool_errors() as f64);
    metrics.insert("denied_tool_calls".into(), collected.denied() as f64);
    metrics.insert("events".into(), collected.events.len() as f64);
    metrics.insert("duration_ms".into(), elapsed.as_millis() as f64);
    metrics
}

/// Per-million-token list prices, used only to make runs comparable in USD.
/// Approximate by construction — update the row rather than the arithmetic.
struct Price {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

fn price_for(model: &str) -> Price {
    match model {
        m if m.starts_with("claude-opus") => Price {
            input: 15.0,
            output: 75.0,
            cache_read: 1.5,
            cache_write: 18.75,
        },
        m if m.starts_with("claude-haiku") => Price {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write: 1.25,
        },
        m if m.starts_with("claude") => Price {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
        m if m.contains("mini") || m.contains("nano") => Price {
            input: 0.25,
            output: 2.0,
            cache_read: 0.025,
            cache_write: 0.25,
        },
        m if m.starts_with("gpt-5") => Price {
            input: 1.25,
            output: 10.0,
            cache_read: 0.125,
            cache_write: 1.25,
        },
        // An unpriced model reports zero cost rather than a fabricated one.
        _ => Price {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
    }
}

/// Convert agentyk usage to Mira usage, pricing the cache split at its own
/// rate — the whole reason the breakdown is worth carrying.
fn mira_usage(usage: &AgentykUsage, model: &str) -> mira::Usage {
    let price = price_for(model);
    let cached = usage.cache_read_input_tokens;
    let written = usage.cache_creation_input_tokens;
    let fresh = usage
        .input_tokens
        .saturating_sub(cached)
        .saturating_sub(written);
    let cost = (fresh as f64 * price.input
        + cached as f64 * price.cache_read
        + written as f64 * price.cache_write
        + usage.output_tokens as f64 * price.output)
        / 1_000_000.0;
    mira::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: cached,
        reasoning_tokens: usage.reasoning_tokens,
        cost_usd: cost,
    }
}

// ============================================================================
// Workspace
// ============================================================================

fn seed_workspace(sample: &Sample) -> std::io::Result<tempfile::TempDir> {
    let workspace = tempfile::tempdir()?;
    for (path, contents) in &sample.files {
        let target = workspace.path().join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, contents)?;
    }
    Ok(workspace)
}

/// Read the workspace back so file scorers grade what the agent actually
/// left on disk. Binary and oversized files are skipped: a transcript is a
/// report, not a backup.
fn read_workspace(root: &Path) -> BTreeMap<String, String> {
    const MAX_FILE: u64 = 64 * 1024;
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE {
                continue;
            }
            if let (Ok(contents), Ok(relative)) =
                (std::fs::read_to_string(&path), path.strip_prefix(root))
            {
                files.insert(relative.to_string_lossy().into_owned(), contents);
            }
        }
    }
    files
}
