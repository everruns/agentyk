# agentyk-everruns-poc (prototype)

A **proof that the extensibility boundary holds**: everruns-flavored behavior
built as a satellite over [`agentyk-core`](https://crates.io/crates/agentyk-core)'s
public seams, with **no change to core**. Not a shipped crate (`publish = false`);
it exists to validate the strategy in
[`specs/extensibility.md`](../../specs/extensibility.md).

The library depends on `agentyk-core` **only** — no framework, no tokio, no
HTTP. The framework (`agentyk`) appears only as a dev-dependency: the test and
example harness.

## What it demonstrates

- **Guardrails are core `TurnMiddleware`.** `ApprovalMiddleware` denies with a
  user-facing message; a redaction middleware **rewrites** a call before it
  runs; they **compose** (a rewrite feeds the next, first deny wins); and a
  capability can bundle a tool with the middleware governing it. None of it
  needs a custom executor — it did before core middleware could express a
  rewrite.
- **Strategy lives in a custom `TurnExecutor`.** `EverrunsExecutor` drives the
  same `atoms` + `TurnState` as the built-in executor and differs only in
  **concurrent tool dispatch** via `TurnState::pending_tool_actions`, closing
  agentyk's item-9 "concurrent dispatch is a deferred follow-up" note.
- **Everruns-flavored data rides the `metadata` hatch.** `ToolHints`
  (`readonly`/`destructive`/`open_world`) live in `ToolDefinition.metadata`
  under a `"hints"` key — core never learns the schema.
- **The transcript surface is a pure observer.** `NarrationListener` renders
  the event stream into readable lines — an `EventListener`, not a turn-loop
  concern. It also surfaces a tool's **risk hint** (`🔎 readonly` / `⚠ destructive`),
  which the executor emits as an `EventData::Custom` event, and pre-run
  **redaction** (`✎ … redacted before it ran`) from the first-class
  `tool.rewritten` event — plus
  provider **extended thinking** (`💭 …`, the typed `Message::thinking` field),
  all without core learning any new variant.

## Run the demo

```sh
cargo run -p agentyk-everruns-poc --example transcript
```

It scripts a small offline session (a search + delete batch, then a note with a
secret) and prints a transcript built entirely from events:

```text
── transcript ─────────────────────────────
• turn started
› search for cats, delete everything, and save a note
🔎 search — readonly
⚙ search(…)
⚠ delete_all — destructive
⚙ delete_all(…)
✓ search
⛔ delete_all denied — `delete_all` needs approval — blocked for the demo
✗ delete_all
⚙ save_note(…)
✎ save_note — redacted before it ran
✓ save_note
‹ All done — one search ran, the delete was blocked, and the secret never reached the tool.
• turn completed
───────────────────────────────────────────
```

The `🔎`/`⚠` hint lines and the `✎` redaction line come from `tool.hint` /
`tool.rewritten` `EventData::Custom` events the executor emits; a real run
against a thinking-capable driver would also show `💭` lines.

## The satellite boundary

> `agentyk-everruns-poc` (or a rebuilt everruns-core) = a custom `TurnExecutor`
> (act/hook/approval/parallel semantics) + capabilities + drivers + `metadata`
> conventions. `agentyk-core` stays frozen and lean.

See [`specs/extensibility.md`](../../specs/extensibility.md) for the full rule:
first-class typed fields only for universal, correctness-load-bearing protocol
data; generic `metadata` hatches for everruns-flavored richness; behavior in
satellites.
