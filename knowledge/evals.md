---
type: Process
title: Evaluation studies
description: Defines what agentyk measures with model-driven evals and how a
  finding becomes a fix.
tags: [evals, mira, testing, metrics, prompt-cache]
---

# Evaluation studies

## Status

Implemented — `evals/agentyk_basic`.

## Abstract

The test suite proves agentyk does what it says on inputs we choose. An eval
measures what it does when a real model drives it: which tools get called, what
a session costs, whether an approval gate holds, whether the prompt cache is
working. Both are needed, and neither substitutes for the other.

Studies are [Mira](https://github.com/everruns/mira) studies: the generic `mira`
host owns the matrix, selection, saved runs, and reporting; the study owns cases,
subject, and scoring.

## Design goals

- **The subject is the library, not a binary.** The eval harness composes an
  in-process `agentyk` session — filesystem capability, shell tool, approval
  middleware, event log, real driver — so a result is attributable to agentyk's
  own behaviour rather than to a downstream host's prompt.
- **Green without credentials.** Every study carries an offline eval on the
  scripted `SimDriver`, and every key-gated target *skips* rather than fails.
  CI grades the offline part on every change; paid presets are run by hand.
- **Cases are data.** A sample carries its own assertions, harness knobs, and
  scripted turns. Adding a case adds no code.
- **Assertions grade behaviour, not style.** A check that accepts exactly one
  correct implementation, or exactly one reasonable tool choice, measures taste
  and produces false failures. Prefer alternatives (`contains_any`,
  `required_any_tools`).
- **A finding is a fix.** When a study surfaces a defect, the fix lands in the
  crates with a unit test that fails without it; the eval remains the regression
  net.

## What is measured

Beyond pass/fail, every case reports tokens split into input / output /
cache-read / cache-write / reasoning, an estimated cost, wall-clock duration and
time-to-first-token, iterations, and the tool record (names, arguments,
observations, errors, denials) as an ATIF trajectory projected from the event
stream.

That split is why `driver::Usage` carries a breakdown rather than two totals: a
cache hit and a cache miss are the same number of tokens and a very different
price, so without it the prompt cache — which the Anthropic driver deliberately
sets up with `cache_control` breakpoints — is unobservable and its regressions
are silent. The first run of `session_efficiency` found exactly that: cached
tokens were dropped entirely on the streaming path, which is the path the engine
always takes.

## Constraints

- Studies live outside the Cargo workspace and depend on the crates **by path**:
  an eval that scored a published version would grade the last release.
- Provider minimums shape what a case can assert. Prompt caching does not engage
  below a model's minimum cacheable prompt (2048 tokens on Haiku, 1024
  elsewhere), and OpenAI's automatic cache is best-effort — a cache assertion
  needs a prompt comfortably past those thresholds, or it measures the provider's
  mood.
- Model-driven results vary between runs. A single failure is a signal to
  re-run with trials, not proof of a regression.

## See also

- [`evals/README.md`](../evals/README.md) — the studies and how to run them.
- [Shipping process](shipping.md) — the evidence bar a change must clear.
