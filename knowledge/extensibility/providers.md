---
type: Design
title: Providers and drivers
description: >-
  Separates the wire protocol a driver implements from the service that speaks
  it, and places credentials on the service.
tags:
  - providers
  - drivers
  - credentials
  - extensibility
  - design
---

# Providers and drivers

Two things were one thing. A `ChatDriver` used to be a wire protocol *and* a
vendor: `OpenAiDriver` carried the Chat Completions request shape, plus
`api.openai.com` as its default endpoint and bearer auth as its scheme, and it
was registered under the id `"openai"` that a `ModelSpec` routed to. The
credential rode on the `ModelSpec` alongside `base_url`, because that was the
only per-call value the driver could read.

That shape works exactly until a second service speaks the same protocol.

- OpenRouter, a corporate gateway, vLLM, and Ollama all speak OpenAI Chat
  Completions. Reaching them meant overriding `base_url` on every `ModelSpec`
  while the driver, the registry entry, and every error message still said
  `"openai"` — an operator reading `openai http 402` went to the wrong
  dashboard.
- Bedrock speaks Anthropic Messages with AWS credentials. It was unreachable
  without duplicating the entire Messages wire mapping, purely because auth
  and endpoint were welded to the driver.
- Adding a second OpenAI protocol (Responses alongside Chat Completions) had
  no place to be chosen once; every call site would decide for itself.

## The split

A **driver** is one wire protocol: build a body, read a response, fold a
stream, plus the endpoint *path* and any header the protocol itself mandates
(`anthropic-version`). It has no id, no hostname, and no credential — a driver
that knew a hostname could only ever serve one service.

A **provider** is a service that speaks one: an id, a base url, a
`ProviderAuth`, any headers the service demands, and the driver it holds. It
is the routing identity — `ProviderRegistry` keys by `ProviderId`, and
`ModelSpec.provider` names one.

So the same `OpenAiDriver` value serves OpenAI, OpenRouter, and a local
runtime as three providers; one service can switch protocols by swapping its
driver; and failures name the service that produced them.

## Credentials live on the service

`ModelSpec` carries **no credential**, and this is the load-bearing
consequence rather than a side effect. The spec is now ordinary configuration:
deserializable from a file, safe in a log line or an event, comparable in a
test. The hand-written redacting `Debug` and the standing rule that "a
`ModelSpec` must never reach an event" both went away with the field. Config
may choose a model; only wiring code chooses a credential.

Credentials are also no longer a value frozen at composition time.
`ProviderAuth::headers` is **asked once per request**, which is what an
expiring OAuth access token needs and what a `String` field could never
provide — the same shape, for the same reason, as `mcp::McpAuthProvider`. See
[`extensibility.md`](extensibility.md) for the general rule.

`Provider::endpoint()` resolves base url and headers immediately before each
completion and hands the result to the driver as `ChatRequest.endpoint`. That
value *is* sensitive — it holds the freshly minted credential — so it redacts
its header values in `Debug` and inherits the rule `ModelSpec` gave up.

## Where the vendors are known

`agentyk-core` names no vendor beyond three conventional ids
(`openai`, `anthropic`, `llmsim`) and cannot: it has no HTTP. The ready-made
assemblies live in the facade's `providers` module next to the driver each
one wires, which is why they are free functions (`providers::openai(key)`)
rather than constructors on `Provider` — an inherent impl must live in the
crate that defines the type.

A service without a ready-made assembly is not a special case; it is the
ordinary path:

```rust
Provider::new("openrouter", OpenAiDriver::new())
    .base_url("https://openrouter.ai/api/v1")
    .auth(BearerAuth::new(key))
```

## Which protocol OpenAI speaks

`providers::openai` speaks [OpenResponses](https://www.openresponses.org/) —
the vendor-neutral standard OpenAI's Responses API implements — rather than
Chat Completions. The split above is what made that a one-line decision
instead of a per-call-site one, and it is the exact case the split was
introduced for.

Responses is a different shape, not a rename. The conversation is a flat list
of typed items (`message`, `function_call`, `function_call_output`,
`reasoning`); the system prompt is `instructions`; a tool result quotes the
`call_id` the model issued rather than the item's own `id`; and content parts
are typed by direction (`input_text` on the way in, `output_text` on the way
back). Two consequences decide the default:

- **Reasoning is a first-class item.** A reasoning model's summary
  round-trips onto `Message::thinking`, the same place Anthropic's extended
  thinking lands. On Chat Completions it does not exist — only a token count
  does.
- **It is where OpenAI's stateful features live.** `previous_response_id`
  chaining, the gap [`everruns-adoption.md`](../roadmap/everruns-adoption.md)
  records, has no Chat Completions equivalent.

Chat Completions is not deprecated here and is not going away: it is what most
OpenAI-compatible vendors, gateways, and local runtimes actually speak, so
`OpenAiDriver` stays bundled and stays the one to pair with a provider of your
own. An OpenAI account or proxy that does not serve Responses swaps it back
with `providers::openai(key).with_driver(OpenAiDriver::new())` — the escape
hatch `with_driver` exists for.

Two driver-level defaults are worth stating because they differ from the
API's own:

- **`store` is off** (the API defaults it on). agentyk replays the whole
  transcript from its event log and sends it in full every turn, so server-side
  retention buys the agent nothing while leaving conversation data with the
  provider — not a default a library should pick for its host. `store(true)`
  for the features that need server state.
- **A refusal is answer text, not an error.** It is something the model said;
  a host that cannot show it has lost the only explanation there is. A
  `response.failed` or `error` *event*, by contrast, is a real failure and is
  raised as one — otherwise a truncated stream reads as a short answer.

The wire mapping was adopted from everruns' `openresponses_protocol`, which
had already found what a from-spec reading misses: gateways that close the
stream with Chat Completions' `[DONE]` sentinel, gateways that emit plaintext
reasoning as `response.reasoning_text.delta`, and `effort: "none"` needing the
reasoning block omitted entirely rather than sent.

## Known limits

- `ProviderAuth` produces headers. A service that signs the request *body*
  (AWS SigV4) needs a driver-level signing hook, which does not exist yet.
- An unregistered provider id fails at `build()`. Because the usual cause is a
  typo in configuration rather than missing wiring, the error lists the ids
  that *are* registered.
