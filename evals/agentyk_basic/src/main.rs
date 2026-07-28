//! agentyk_basic — how well the library works when it is actually driven.
//!
//! A [Mira](https://github.com/everruns/mira) study, written on the `mira-eval`
//! Rust SDK. The subject is an in-process [`agentyk`] session composed the way
//! a coding agent composes it — filesystem capability, a shell tool, an
//! approval middleware, an event log — so what these evals grade is the
//! library's own behaviour (turn loop, tool surface, middleware, usage
//! accounting), not a downstream binary's prompt.
//!
//! Four evals:
//!
//! * `coding` — ordinary edits and lookups through the tool surface.
//! * `tool_policy` — tool *choice* and respect for a declined call.
//! * `session_efficiency` — what a multi-turn session costs: tokens, prompt
//!   cache reuse, wall clock.
//! * `offline_contract` — the wiring itself, on the scripted `sim` model, so
//!   CI grades it with no API key and no network.
//!
//! Run it with the `mira` host CLI from this directory (`mira list`,
//! `mira run --preset smoke`); see README.md.

mod cases;
mod harness;

use mira::scorer::{Scorer, latency_within, scorer, succeeded};
use mira::subject::subject_fn;
use mira::{Eval, Sample, Score, Target, Transcript, eval};
use serde_json::Value;

/// Models under test. Each gates on its provider key and is *skipped* — not
/// failed — when it is missing, so a keyless run is a no-op rather than a wall
/// of red.
fn live_targets() -> Vec<Target> {
    vec![
        Target::anthropic("claude-sonnet-5"),
        Target::anthropic("claude-haiku-4-5-20251001"),
        Target::openai("gpt-5.5"),
    ]
}

#[eval]
fn coding() -> Eval {
    Eval::new("coding")
        .describe("Ordinary coding work through the agentyk tool surface")
        .dataset(cases::coding())
        .subject(subject_fn(harness::run_case))
        .scorer(succeeded())
        .scorer(checks())
        .scorer(trajectory_is_joinable())
        .scorer(latency_within(300_000))
        .targets(live_targets())
        // The engine only ever calls `complete_streaming`; `buffered` forces
        // the driver's `complete` path instead. Same cases, same scorers — any
        // difference in pass rate is a difference between the two provider
        // code paths, which is the only way that gap shows up before a user
        // finds it.
        .axis("mode", ["stream", "buffered"])
        .max_turns(16)
        .build()
}

#[eval]
fn tool_policy() -> Eval {
    Eval::new("tool_policy")
        .describe("Tool choice, tool isolation, and respect for a declined call")
        .dataset(cases::tool_policy())
        .subject(subject_fn(harness::run_case))
        .scorer(succeeded())
        .scorer(checks())
        .scorer(tool_discipline())
        .targets(live_targets())
        .max_turns(12)
        .build()
}

#[eval]
fn session_efficiency() -> Eval {
    Eval::new("session_efficiency")
        .describe("Token, prompt-cache, and wall-clock cost of a multi-turn session")
        .dataset(cases::session_efficiency())
        .subject(subject_fn(harness::run_case))
        .scorer(succeeded())
        .scorer(checks())
        .scorer(budgets())
        .scorer(prompt_cache_is_reused())
        .targets(live_targets())
        .max_turns(12)
        .build()
}

#[eval]
fn offline_contract() -> Eval {
    Eval::new("offline_contract")
        .describe("Capability, middleware, and event/trajectory wiring on the scripted model")
        .dataset(cases::offline_contract())
        .subject(subject_fn(harness::run_case))
        .scorer(succeeded())
        .scorer(checks())
        .scorer(tool_discipline())
        .scorer(trajectory_is_joinable())
        .scorer(metric_expectations())
        .targets([Target::sim()])
        .max_turns(8)
        .build()
}

fn main() -> std::io::Result<()> {
    mira::Study::registered().serve_blocking()
}

// ============================================================================
// Scorers — all sample-driven, so the dataset stays the only place cases live
// ============================================================================

/// Grades the sample's own `checks`: file contents and response text.
fn checks() -> Box<dyn Scorer> {
    scorer("checks", |sample: &Sample, transcript: &Transcript| {
        let Some(checks) = sample.metadata.get("checks").and_then(Value::as_array) else {
            return Score::pass("checks", "no checks declared");
        };
        for check in checks {
            if let Some(path) = check.get("file").and_then(Value::as_str) {
                let Some(contents) = transcript.files.get(path) else {
                    return Score::fail("checks", format!("{path} is missing from the workspace"));
                };
                if let Some(missing) = missing_from(contents, check.get("contains")) {
                    return Score::fail("checks", format!("{path} lacks {missing:?}"));
                }
                if let Some(present) = present_in(contents, check.get("lacks")) {
                    return Score::fail("checks", format!("{path} still contains {present:?}"));
                }
                if let Some(alternatives) = check.get("contains_any")
                    && !contains_any(contents, alternatives)
                {
                    return Score::fail("checks", format!("{path} matches none of {alternatives}"));
                }
            }
            if let Some(missing) =
                missing_from(&transcript.final_response, check.get("response_contains"))
            {
                return Score::fail("checks", format!("the response lacks {missing:?}"));
            }
            if let Some(present) =
                present_in(&transcript.final_response, check.get("response_lacks"))
            {
                return Score::fail("checks", format!("the response contains {present:?}"));
            }
            if let Some(alternatives) = check.get("response_contains_any")
                && !contains_any(&transcript.final_response, alternatives)
            {
                return Score::fail(
                    "checks",
                    format!("the response matches none of {alternatives}"),
                );
            }
        }
        Score::pass("checks", "every declared check held")
    })
}

/// Grades tool *choice*: the tools a case requires, and the ones it forbids.
/// A forbidden tool is how "prefer the file tools" and "do not route around a
/// denial" stop being prose and start being a measurement.
fn tool_discipline() -> Box<dyn Scorer> {
    scorer(
        "tool_discipline",
        |sample: &Sample, transcript: &Transcript| {
            let used = |name: &str| transcript.tool_calls.iter().any(|call| call == name);
            for required in names(sample, "required_tools") {
                if !used(&required) {
                    return Score::fail(
                        "tool_discipline",
                        format!(
                            "never called `{required}` (used {:?})",
                            transcript.tool_calls
                        ),
                    );
                }
            }
            // `required_any_tools` is how a case says "any of these is a right
            // answer": which file tool answers a lookup is the model's call,
            // and pinning one would grade taste rather than behaviour.
            let alternatives = names(sample, "required_any_tools");
            if !alternatives.is_empty() && !alternatives.iter().any(|name| used(name)) {
                return Score::fail(
                    "tool_discipline",
                    format!(
                        "called none of {alternatives:?} (used {:?})",
                        transcript.tool_calls
                    ),
                );
            }
            for forbidden in names(sample, "forbidden_tools") {
                if used(&forbidden) {
                    return Score::fail(
                        "tool_discipline",
                        format!("called `{forbidden}`, which this case forbids"),
                    );
                }
            }
            Score::pass("tool_discipline", "used the intended tool surface")
        },
    )
}

/// The token and latency ceilings a case declares. Separate from the shared
/// `tokens_within` so each case can carry its own budget.
fn budgets() -> Box<dyn Scorer> {
    scorer("budgets", |sample: &Sample, transcript: &Transcript| {
        let Some(max) = sample.metadata.get("max_tokens").and_then(Value::as_u64) else {
            return Score::pass("budgets", "no budget declared");
        };
        let total = transcript.usage.total_tokens();
        match total <= max {
            true => Score::pass("budgets", format!("{total} tokens (budget {max})")),
            false => Score::fail(
                "budgets",
                format!("{total} tokens exceeds the {max} budget"),
            ),
        }
    })
}

/// The prompt-cache assertion: on a multi-turn session, a declared share of
/// the prompt tokens must come back from the provider's cache.
///
/// This is the eval that pays for itself — the driver places `cache_control`
/// breakpoints, and this is the only thing that notices when they stop
/// working, since a cache miss and a cache hit cost the same number of
/// *tokens* and differ only in price.
fn prompt_cache_is_reused() -> Box<dyn Scorer> {
    scorer(
        "prompt_cache",
        |sample: &Sample, transcript: &Transcript| {
            let Some(minimum) = sample
                .metadata
                .get("min_cache_hit_ratio")
                .and_then(Value::as_f64)
            else {
                return Score::pass("prompt_cache", "no cache expectation declared");
            };
            let ratio = transcript
                .metrics
                .get("cache_hit_ratio")
                .copied()
                .unwrap_or(0.0);
            let read = transcript.usage.cache_read_tokens;
            match ratio >= minimum {
                true => Score::pass(
                    "prompt_cache",
                    format!(
                        "{read} cached prompt tokens ({:.0}% of input)",
                        ratio * 100.0
                    ),
                ),
                false => Score::fail(
                    "prompt_cache",
                    format!(
                        "only {read} cached prompt tokens ({:.0}% of input, expected {:.0}%)",
                        ratio * 100.0,
                        minimum * 100.0
                    ),
                ),
            }
        },
    )
}

/// Exact metric expectations a case declares, e.g. `{"denied_tool_calls": 1}`.
fn metric_expectations() -> Box<dyn Scorer> {
    scorer(
        "metric_expectations",
        |sample: &Sample, transcript: &Transcript| {
            let Some(expected) = sample
                .metadata
                .get("expect_metrics")
                .and_then(Value::as_object)
            else {
                return Score::pass("metric_expectations", "none declared");
            };
            for (key, want) in expected {
                let want = want.as_f64().unwrap_or_default();
                let got = transcript.metrics.get(key).copied().unwrap_or(0.0);
                if (got - want).abs() > f64::EPSILON {
                    return Score::fail(
                        "metric_expectations",
                        format!("{key} was {got}, expected {want}"),
                    );
                }
            }
            Score::pass("metric_expectations", "every declared metric matched")
        },
    )
}

/// Every tool call in the trajectory must have an observation joined to it by
/// call id. That join is the library's own contract — `tool.started` and
/// `tool.completed` correlate on `call_id`, denials included — and a case that
/// loses it has lost the only structured record of what the agent did.
fn trajectory_is_joinable() -> Box<dyn Scorer> {
    scorer(
        "trajectory",
        |_: &Sample, transcript: &Transcript| match &transcript.trajectory {
            None => Score::fail("trajectory", "the subject reported no trajectory"),
            Some(trajectory) => {
                for step in &trajectory.steps {
                    let observed: Vec<&str> = step
                        .observation
                        .iter()
                        .flat_map(|observation| observation.results.iter())
                        .filter_map(|result| result.source_call_id.as_deref())
                        .collect();
                    for call in &step.tool_calls {
                        if !observed.contains(&call.tool_call_id.as_str()) {
                            return Score::fail(
                                "trajectory",
                                format!(
                                    "`{}` has no observation joined to call {}",
                                    call.function_name, call.tool_call_id
                                ),
                            );
                        }
                    }
                }
                Score::pass("trajectory", "every tool call carries its observation")
            }
        },
    )
}

fn names(sample: &Sample, key: &str) -> Vec<String> {
    sample
        .metadata
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The first needle absent from `haystack`, if any.
fn missing_from(haystack: &str, needles: Option<&Value>) -> Option<String> {
    strings(needles)
        .into_iter()
        .find(|needle| !haystack.contains(needle.as_str()))
}

/// The first needle present in `haystack`, if any.
fn present_in(haystack: &str, needles: Option<&Value>) -> Option<String> {
    strings(needles)
        .into_iter()
        .find(|needle| haystack.contains(needle.as_str()))
}

/// Whether `haystack` matches at least one alternative.
fn contains_any(haystack: &str, alternatives: &Value) -> bool {
    strings(Some(alternatives))
        .into_iter()
        .any(|needle| haystack.contains(needle.as_str()))
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mira::Runner;

    /// The offline eval is the study's own smoke test: it exercises the whole
    /// path — capability, middleware, events, trajectory, metrics — with no
    /// key and no network, so a broken harness fails here rather than in a
    /// paid run.
    #[tokio::test]
    async fn offline_contract_passes_with_no_credentials() {
        let report = Runner::new().add(offline_contract()).run().await;
        assert!(report.all_passed(), "{report:#?}");
    }
}
