# Provider development

## Built-in profiles

The preset `kind` selects the default base URL and adapter:

| Kind | Protocol | Default base URL |
|---|---|---|
| `openai` | OpenAI Chat Completions | `https://api.openai.com/v1` |
| `anthropic` | Anthropic Messages | `https://api.anthropic.com/v1` |
| `gemini` | Gemini GenerateContent | `https://generativelanguage.googleapis.com/v1beta` |
| `groq` | OpenAI-compatible | `https://api.groq.com/openai/v1` |
| `mistral` | OpenAI-compatible | `https://api.mistral.ai/v1` |
| `xai` | OpenAI-compatible | `https://api.x.ai/v1` |
| `deepseek` | OpenAI-compatible | `https://api.deepseek.com` |
| `together` | OpenAI-compatible | `https://api.together.ai/v1` |
| `fireworks` | OpenAI-compatible | `https://api.fireworks.ai/inference/v1` |
| `openai-compatible` | Configured profile | Required `base_url` |

All compatible presets share transport/SSE code, while their configured capabilities remain independent. This prevents an endpoint's OpenAI compatibility claim from being interpreted as support for every OpenAI field.

Internally, a provider **profile** (OpenAI, Groq, Anthropic, and so on) is separate from its **wire protocol**. There are currently three protocols: OpenAI Chat Completions, Anthropic Messages, and Gemini GenerateContent. Profile metadata has one source of truth for protocol, default URL, and default capability class. Adapter construction then matches the protocol exhaustively; there is no wildcard fallback that could silently send a newly added native provider through the OpenAI adapter.

Compatible endpoints default to `Authorization: Bearer <key>`. A custom profile can set `auth_header = "x-api-key"` to send the raw configured or per-request key in that provider header, plus optional `auth_scheme = "Bearer"` (or another token) when the custom header requires a prefix. `X-Upstream-API-Key` is always consumed by the router and is never forwarded under that name.

## Models and capabilities

Exact model names split at the first `/`. The provider name is the prefix and everything after it is the opaque upstream model ID. For example, `together/meta-llama/Llama-3` sends `meta-llama/Llama-3` upstream.

Undeclared models inherit conservative provider defaults. Declare a model-specific full capability object when it differs. Capability fields are:

`text`, `streaming`, `images`, `input_audio`, `pdf`, `pdf_text_extraction`, `video`, `function_tools`, `custom_tools`, `parallel_tools`, `structured_outputs`, `reasoning`, `opaque_reasoning`, `logprobs`, `seed`, `multiple_choices`, and `audio_output`.

The model capability object replaces the provider default; include every supported capability. The router derives requirements from each request and never sends a candidate a request it cannot preserve.

### Anthropic reasoning modes

The Anthropic adapter distinguishes manual extended-thinking models from adaptive-thinking models. Claude Haiku/Sonnet 4.5 and earlier models use `thinking.type = "enabled"` with a token budget; Claude 4.6+ defaults to adaptive thinking with native effort. Claude Opus 4.5 keeps manual thinking and can also preserve native effort.

For manual models, OpenAI/OpenRouter-style effort is mapped to an explicit budget: `minimal`/`low` = 1,024, `medium` = 4,096, `high` = 8,192, `xhigh` = 16,384, and `max` = 32,768 tokens. An explicit `reasoning.max_tokens` remains authoritative. Manual budgets below Anthropic's 1,024-token minimum, or budgets that leave no room below the request's output-token limit, are rejected locally with a named OpenAI-style 400 instead of being sent upstream.

This model-mode split follows Anthropic's [extended-thinking compatibility table](https://platform.claude.com/docs/en/build-with-claude/extended-thinking). Unknown future Claude models default to adaptive mode; a future manual-only family needs one classification case plus request-conversion and live-contract coverage.

## Adding a native adapter

First decide whether the provider is a profile or a new protocol:

- An OpenAI-compatible provider normally needs no adapter. Add its typed profile and descriptor (or use `kind = "openai-compatible"` with a configured URL), then declare conservative provider/model capabilities.
- A genuinely new wire contract gets a new `ProviderProtocol` variant and adapter module. The exhaustive factory match intentionally fails compilation until that protocol has an explicit constructor.

Native adapters must use the shared per-provider transport in `transport.rs`. It creates exactly one pooled HTTP client per configured provider and centralizes connection timeouts, bounded upstream error inspection, and SSE idle policy. Reuse `sse.rs` for bounded framing and `attachment.rs` for attachment fetching/PDF fallback. Provider-specific request conversion and stream-event conversion stay inside the adapter module; they must not leak into `router-server` or the routing/fallback loop.

Implement `ProviderAdapter` and emit only canonical `ProviderEvent` values. Requirements:

1. Check the HTTP status before exposing a stream and mark only explicit 408/429/5xx or safe connection failures retryable.
2. Parse arbitrarily fragmented SSE with the shared bounded parser.
3. Preserve choice indexes, incremental tool arguments, finish reasons, usage, and opaque reasoning signatures.
4. Reject every accepted public field that cannot be represented upstream. Never ignore it.
5. Do not include provider response bodies, prompts, tool arguments, or credentials in errors or traces.
6. Add request-conversion, stream-fixture, continuation-signature, malformed-input, and black-box tests.

The intended dependency direction is:

```text
router-protocol <- router-core <- router-providers <- haarness-router
                         ^              ^
                         +-- router-server
```

`router-core` owns routing policy and the adapter interface, but never imports a concrete provider. `router-providers` owns upstream wire knowledge, but never owns authentication of the incoming router request, fallback ordering, circuit state, or public HTTP response assembly. Keeping those boundaries intact prevents provider additions from changing unrelated routing or server behavior.
