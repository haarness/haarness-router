# Providers

## Built-in kinds

A provider's `kind` selects its wire protocol, default base URL and default capabilities:

| Kind | Protocol | Default base URL |
|---|---|---|
| `openai` | OpenAI Chat Completions, and the Responses API where needed | `https://api.openai.com/v1` |
| `anthropic` | Anthropic Messages | `https://api.anthropic.com/v1` |
| `gemini` | Gemini GenerateContent (Google AI Studio, or Vertex AI) | `https://generativelanguage.googleapis.com/v1beta` |
| `amazon-bedrock` | Bedrock ConverseStream | `https://bedrock-runtime.us-east-1.amazonaws.com` |
| `groq` | OpenAI-compatible | `https://api.groq.com/openai/v1` |
| `mistral` | OpenAI-compatible | `https://api.mistral.ai/v1` |
| `xai` | OpenAI-compatible | `https://api.x.ai/v1` |
| `deepseek` | OpenAI-compatible | `https://api.deepseek.com` |
| `together` | OpenAI-compatible | `https://api.together.ai/v1` |
| `fireworks` | OpenAI-compatible | `https://api.fireworks.ai/inference/v1` |
| `openai-compatible` | OpenAI-compatible | none: `base_url` is required |

OpenAI-compatible kinds share one transport and SSE implementation, but each keeps its own capabilities, so
an endpoint's claim of OpenAI compatibility is never read as support for every OpenAI field. Compatible
endpoints receive `Authorization: Bearer <key>` unless `auth_header` names another header (for example
`x-api-key`, sent with the raw key, or with an `auth_scheme` prefix). `X-Upstream-API-Key` is always consumed
by the router and never forwarded under that name.

### Vertex AI

Use `kind = "gemini"` with a Vertex AI publisher URL and `auth_header = "authorization"`, which sends the
credential as an OAuth Bearer token instead of a Gemini API key:

```toml
[providers.vertex]
kind = "gemini"
base_url = "https://aiplatform.googleapis.com/v1/projects/<project>/locations/<location>/publishers/google"
auth_header = "authorization"
api_key_env = "VERTEX_ACCESS_TOKEN"
```

Model discovery then lists Google's Model Garden chat models. The router reads the token once at startup,
so a short-lived access token needs a restart before it expires.

### Amazon Bedrock

`amazon-bedrock` calls ConverseStream with a Bedrock API key sent as a Bearer token. It never reads the
process's AWS credentials or signs requests with SigV4. Set `base_url` for a region other than `us-east-1`:

```toml
[providers.bedrock]
kind = "amazon-bedrock"
base_url = "https://bedrock-runtime.us-west-2.amazonaws.com"
api_key_env = "AWS_BEARER_TOKEN_BEDROCK"
```

Models are addressed by their Bedrock model or inference-profile ID, for example
`bedrock/<model-id>`. Discovery reads the region's text foundation models and inference profiles from the
Bedrock control plane with the same key. Bedrock starts with the conservative capability set below;
declare richer capabilities per model when you know them.

## Models and capabilities

A model ID splits at the first `/`: the provider name is the prefix and the rest is the opaque upstream ID.
`together/meta-llama/Llama-3` sends `meta-llama/Llama-3` upstream.

The router derives what each request needs and never sends it to a target that cannot preserve it.
Capability fields are:

`text`, `streaming`, `images`, `input_audio`, `pdf`, `pdf_text_extraction`, `video`, `function_tools`,
`custom_tools`, `parallel_tools`, `structured_outputs`, `reasoning`, `opaque_reasoning`, `logprobs`,
`seed`, `multiple_choices` and `audio_output`.

| Default set | Kinds | Capabilities |
| --- | --- | --- |
| OpenAI | `openai` | text, streaming, images, input audio, PDF text extraction, function, custom and parallel tools, structured outputs, reasoning, opaque reasoning, logprobs, seed, multiple choices, audio output |
| Anthropic | `anthropic` | text, streaming, images, PDF, PDF text extraction, function and parallel tools, structured outputs, reasoning, opaque reasoning |
| Gemini | `gemini` | text, streaming, images, input audio, PDF, PDF text extraction, video, function and parallel tools, structured outputs, reasoning, opaque reasoning |
| Conservative | every other kind | text, streaming, function tools, PDF text extraction |

A `capabilities` object on a provider or a model **replaces** the defaults rather than adding to them, so
list every capability the target supports:

```toml
[models."groq/llama-3.3-70b-versatile"]
capabilities = { text = true, streaming = true, function_tools = true, parallel_tools = true, structured_outputs = true }
```

Discovered models carry provider metadata — `supports_reasoning`, `reasoning_efforts`, tool and image
support and token limits — and explicit live metadata wins over family defaults. Anthropic's per-level
capability flags decide its effort choices. OpenAI's catalog has no effort metadata, so documented
model-family defaults live in `crates/router-providers/src/models.rs`. A provider that publishes its own
effort list needs no client changes.

## Reasoning

### OpenAI

For native OpenAI, GPT-6, GPT-5.6 and later numbered families, Codex and Pro models are called through the
Responses API, as are GPT-5 requests that combine tools with a reasoning effort other than `none` (Chat
Completions refuses that pair). The public API stays Chat Completions: text, images, function and custom
tools, structured output, usage, reasoning summaries and encrypted reasoning continuation are translated.
Requests use `store: false`, and continuation state travels in opaque `reasoning_details` that clients must
send back. A Chat-only field the Responses API cannot take returns a named 400; Responses-only features such
as hosted tools are not exposed.

### Anthropic

The adapter distinguishes manual extended-thinking models from adaptive ones. Claude Haiku and Sonnet 4.5
and earlier use `thinking.type = "enabled"` with a token budget; Claude 4.6 and later default to adaptive
thinking with native effort; Claude Opus 4.5 keeps manual thinking and can also carry native effort. Unknown
future Claude models default to adaptive.

For manual models, an effort maps to a budget: `minimal` and `low` 1,024, `medium` 4,096, `high` 8,192,
`xhigh` 16,384 and `max` 32,768 tokens. An explicit `reasoning.max_tokens` wins. A budget below Anthropic's
1,024-token minimum, or one that leaves no room under the request's output limit, is refused locally with a
named 400. This follows Anthropic's
[extended-thinking documentation](https://platform.claude.com/docs/en/build-with-claude/extended-thinking).

Claude Fable 5.1 and Mythos 5.1 accept `auto` and `none` tool choice; a forced tool choice returns a named
400 because those models do not support it.

Claude refuses a text block that is empty or only whitespace, so blank text parts are left out: a photo sent
with an empty text part beside it reaches Claude as the photo alone. A data URL may carry parameters before
`;base64` (`data:image/jpeg;name=photo.jpg;base64,…`); the media type is what comes before the first one.

### Gemini

Gemini 3 and later use `thinkingLevel`, with the levels each generation accepts: Gemini 4 Argon, 3.8 and 3.7
Flash and 3.1 Pro offer `low`, `medium` and `high`; Gemini 3 Pro `low` and `high`; other Gemini 3 models also
`minimal`. Gemini 2.5 uses `thinkingBudget` (`low` 2,048, `medium` 8,192, `high` 24,576; `none` maps to zero
only where thinking can be turned off). Thinking counts against the output limit, so with `max_tokens` or
`max_completion_tokens` set an effort's budget is at most half of it and a short answer keeps room; it stays on
unless half the limit is less than the model thinks at all, when 2.5 Pro thinks its minimum of 128 tokens and
Flash and Flash-Lite (512 at least) stop thinking. A `reasoning.max_tokens` budget is sent as given. An
unsupported level is refused before Google is called. See
Google's [thinking guide](https://ai.google.dev/gemini-api/docs/generate-content/thinking).

Gemini usage counts candidate and thought tokens as completion tokens, with reasoning as a subset and cached
input kept separately. Tool schemas are sent as `parametersJsonSchema`, so nullable fields and constraints
survive, and streamed calls keep distinct IDs, ordered arguments and thought signatures across chunks and
tool-result continuations.

## Adding a provider

First decide whether it is a new **profile** or a new **protocol**. Inside `router-core`, a provider kind
(OpenAI, Groq, Anthropic, …) is separate from its wire protocol; there are four protocols today: OpenAI Chat
Completions, Anthropic Messages, Gemini GenerateContent and Bedrock Converse. Each kind's descriptor in
`crates/router-core/src/config.rs` is the single source of its protocol, default URL and default
capabilities, and the adapter factory in `crates/router-providers/src/lib.rs` matches protocols exhaustively,
with no wildcard that could send a new native provider through the OpenAI adapter.

- An OpenAI-compatible provider needs no adapter: add its kind and descriptor (or use
  `kind = "openai-compatible"` with a `base_url`), then declare conservative capabilities.
- A genuinely new wire contract gets a new `ProviderProtocol` variant and adapter module. The exhaustive
  match fails to compile until that protocol has a constructor.

Native adapters use the shared per-provider transport in `transport.rs`: one pooled HTTP client per
configured provider, with connection timeouts, bounded upstream error inspection and the SSE idle policy in
one place. The routing layer bounds response headers and the first stream event with one deadline and
handles cancellation in both phases without retrying an ambiguous generation. Reuse `sse.rs` for bounded
framing and `attachment.rs` for attachment fetching and the PDF fallback. Request and stream-event
conversion stay inside the adapter module; they must not leak into `router-server` or the fallback loop.

Implement `ProviderAdapter` and emit only canonical `ProviderEvent` values:

1. Check the HTTP status before exposing a stream, and mark only explicit 408/429/5xx or safe connection
   failures retryable.
2. Parse arbitrarily fragmented SSE with the shared bounded parser.
3. Preserve choice indexes, incremental tool arguments, finish reasons, usage and opaque reasoning
   signatures. When a provider reports the prompt before the final usage (Anthropic `message_start`), emit
   `ProviderEvent::PromptUsage` so a cut stream can still be billed from the provider's own counts; clients
   only ever see the final `Usage`.
4. Refuse every accepted public field that cannot be represented upstream. Never ignore one.
5. Keep provider response bodies, prompts, tool arguments and credentials out of errors and traces.
6. Add request-conversion, stream-fixture, continuation-signature, malformed-input and black-box tests.

The dependency direction:

| Crate | Depends on |
| --- | --- |
| `router-core` | `router-protocol` |
| `router-providers` | `router-core`, `router-protocol` |
| `router-server` | `router-core`, `router-protocol` (`router-providers` only in its tests) |
| `haarness-router` | all four |

`router-core` owns routing policy and the adapter interface but never imports a concrete provider.
`router-providers` owns upstream wire knowledge but never authenticates the incoming request, orders
fallback, keeps circuit state or assembles public HTTP responses. Keeping those boundaries means a new
provider cannot change unrelated routing or server behaviour.
