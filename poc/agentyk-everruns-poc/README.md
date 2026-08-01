# agentyk-everruns-poc (prototype)

A proof that Everruns-flavored policy and metadata compose with Agentyk's
canonical engine. Not a shipped crate (`publish = false`); it exists to
validate the strategy in
[`knowledge/extensibility/extensibility.md`](../../knowledge/extensibility/extensibility.md).

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
- **The canonical engine prepares tool batches.** A durable or concurrent host
  can dispatch the batch without replacing middleware or turn semantics.
- **Everruns-flavored data rides the `metadata` hatch.** `ToolHints`
  (`readonly`/`destructive`/`open_world`) live in `ToolDefinition.metadata`
  under a `"hints"` key — core never learns the schema.
- **The transcript surface is a pure observer.** `NarrationListener` renders
  the event stream into readable lines — an `EventListener`, not a turn-loop
  concern. It surfaces pre-run **redaction** (`✎ … redacted before it ran`)
  from the first-class `tool.rewritten` event, plus
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
⚙ search(…)
⚙ delete_all(…)
⛔ delete_all denied — `delete_all` needs approval — blocked for the demo
✓ search
✗ delete_all
⚙ save_note(…)
✎ save_note — redacted before it ran
✓ save_note
‹ All done — one search ran, the delete was blocked, and the secret never reached the tool.
• turn completed
───────────────────────────────────────────
```

The `✎` redaction line comes from the durable `tool.rewritten` event emitted by
the engine; a real run against a thinking-capable driver would also show `💭`
lines.

## The adopter boundary

> Rebuilt everruns-core = a durable host and operation dispatchers +
> capabilities + drivers + metadata conventions. The shared engine owns turn
> semantics.

See [`knowledge/extensibility/extensibility.md`](../../knowledge/extensibility/extensibility.md) for the full rule:
first-class typed fields only for universal, correctness-load-bearing protocol
data; generic `metadata` hatches for everruns-flavored richness; behavior in
satellites.
