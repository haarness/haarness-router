# API reference

Haarness Router serves the OpenAI Chat Completions API. The public request and response types follow the
[OpenAI Chat Completions reference](https://platform.openai.com/docs/api-reference/chat) and live in
`crates/router-protocol`.

## Endpoints

| Method and path | Authentication | Behaviour |
| --- | --- | --- |
| `POST /v1/chat/completions` | Router key when `auth` is on | Streaming (SSE) and non-streaming completions. |
| `GET /v1/models` | Router key when `auth` is on | Aliases, declared models and discovered provider catalogs. |
| `GET /healthz`, `GET /readyz` | None | `200 {"status":"ok"}` while the process serves. |
| `GET /metrics` | Router key or scrape token when `auth` is on | Prometheus text format; see [observability](observability.md). |
| `GET /` and `<web_ui.path>/…` | Web UI Basic auth when configured | `/` redirects to the dashboard; see [Web UI](web-ui.md). Absent when `web_ui.enabled = false`. |

With authentication on, send `Authorization: Bearer <router key>`. A missing, malformed or unknown key
gets `401` with code `invalid_api_key`; the scrape token is accepted only by `/metrics`.

## Choosing a model

The `model` field is either an **alias** from `[aliases]` or an exact **`provider/model`**. The provider
name is everything before the first `/`, and the rest is sent upstream unchanged:
`together/meta-llama/Llama-3` calls `meta-llama/Llama-3` on the provider named `together`.

- An exact `provider/model` does not have to be declared under `[models]`; an undeclared model inherits
  its provider's default capabilities. This is how newly released models work without a configuration
  change.
- An alias tries its targets in order. Targets that cannot serve the request (for example, images to a
  text-only model) are skipped before any upstream call; if none can, the request fails with `400` "no
  route can preserve the requested features", naming what each target lacks.
- Fallback to the next target happens only for a connection failure, `408`, `429` or `5xx` **before the
  first event** reached the client. A stream that has started is never retried.
- Each target has a circuit breaker: three failures open it for 30 seconds, then one probe request decides
  whether it closes. Exact models that are not declared share one breaker per provider.

## Request headers

| Header | Effect |
| --- | --- |
| `Authorization: Bearer …` | The router key (or, on `/metrics`, the scrape token). Never forwarded. |
| `X-Upstream-API-Key` | A provider credential for this request only, when `auth.allow_upstream_key_header = true`; otherwise `400`. Accepted only with an exact `provider/model`; aliases and fallback to server-owned keys are refused. |
| `X-Request-ID` | Reused when it is 1–128 characters of `[A-Za-z0-9._:-]`; otherwise the router generates `req_<UUIDv7>`. |
| `traceparent` | Recorded in the request log when it is strict W3C trace context. `tracestate` is ignored. |

No inbound header, request ID or trace context is sent to a provider.

## Response headers

Every response carries `X-Request-ID`. A routed completion also carries:

| Header | Value |
| --- | --- |
| `X-Haarness-Provider` | The configured provider that answered. |
| `X-Haarness-Model` | The upstream model it used. |
| `X-Haarness-Attempts` | How many targets were tried, the answering one included. |

## What a request may contain

- **Roles:** `developer`, `system`, `user`, `assistant`, `tool` and the legacy `function`. Native adapters
  refuse legacy shapes they cannot translate losslessly.
- **Tools:** function tools on every adapter; custom tools on targets that declare `custom_tools`.
  `tool_choice` is translated, and refused where a model cannot honour it.
- **Reasoning:** `reasoning_effort`, `reasoning`, `reasoning_content` and `reasoning_details`. Anthropic
  thinking signatures, Gemini thought signatures and OpenAI encrypted reasoning travel as opaque
  `reasoning_details` that clients must send back, and they pin a continuation to a compatible target.
  See [providers](providers.md#reasoning) for each provider's effort mapping.
- **Inputs:** text, images, PDF/file parts, audio and video, each only on targets that declare it.
- **PDFs:** sent natively to targets with `pdf`; otherwise, on targets with `pdf_text_extraction`, the
  router fetches the file (HTTPS only, with SSRF protections) or decodes its base64 and sends the
  extracted text. A PDF with no extractable text (a scan) returns `unsupported_file` instead of an empty
  document.
- **Structured output:** `response_format` passes through to compatible endpoints and is translated to
  Anthropic's and Gemini's native schema controls.
- **Prompt caching:** an OpenRouter-style `cache_control` (`type`, optional `ttl`) on a message or content
  part is passed to Anthropic as its cache breakpoint.
- **Streaming usage:** `stream_options.include_usage` adds the final usage chunk.
- **Unknown fields:** forwarded only by providers with `allow_unknown_fields = true`; native adapters
  refuse them.

A field that a target cannot represent is refused with a named `400` before the upstream call; nothing is
silently dropped.

## Limits

| Limit | Value |
| --- | --- |
| Request body | `server.max_request_bytes` (32 MiB by default); larger bodies get `413 request_too_large`. |
| Concurrent requests | `server.max_concurrent_requests` (20,000 by default); beyond it, `429 router_overloaded`. |
| First event | `server.first_byte_timeout_ms` (120 s by default) covers response headers and the first stream event. |
| Upstream connect and stream idle | `connect_timeout_ms` (5 s) and `stream_idle_timeout_ms` (60 s) per provider. |
| SSE frame from a provider | 1 MiB. |
| PDF download / extracted text | 20 MiB / 4 MiB, with at most three validated redirects. |

A client that disconnects cancels the upstream request and releases its concurrency slot.

## Errors

Errors use the OpenAI envelope:

```json
{"error":{"message":"model nope must be an alias or provider/model","type":"invalid_request_error","param":"model","code":"invalid_request"}}
```

An upstream HTTP error keeps its status and gets the code `upstream_<status>` (for example
`upstream_429`). Its message names at most the provider's machine-readable error type, such as
`upstream returned HTTP 429 (rate_limit_error)`: the provider's own message and body are never passed on,
because they can echo prompts or credentials. Transport failures are `502 upstream_transport_error` and
deadlines `504 upstream_timeout`. When every target of an alias failed, the last retryable error is
returned, or `503 no_route_available` when no target could be tried (for example, every circuit is open).

## Model discovery

`GET /v1/models` returns the configured aliases and models, plus every provider's own catalog of chat
models, discovered with the server's provider keys (never the caller's router key or upstream key):

- OpenAI-compatible endpoints, Anthropic, Gemini, Vertex AI and Bedrock catalogs are supported.
- Discovery runs when the endpoint is called, at most once every five minutes; simultaneous calls share
  one refresh, and a failed refresh keeps the last complete catalog.
- Each provider's refresh is bounded to ten seconds, ten pages and 4 MiB per page.
- Embeddings, realtime and media-generation-only models are left out. Entries carry provider metadata
  such as `supports_reasoning`, `reasoning_efforts`, tool and image support and token limits when known.

A listed model is not a promise that the account can generate with it, and an unlisted one can still be
called by its exact `provider/model` name.
