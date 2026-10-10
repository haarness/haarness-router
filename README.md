# Haarness Router

Haarness Router is a stateless, high-throughput Rust service that exposes an OpenAI-compatible Chat Completions API and routes requests to OpenAI, Anthropic, Gemini, Groq, Mistral, xAI, DeepSeek, Together, Fireworks, or a custom OpenAI-compatible endpoint.

It deliberately contains no billing, credit accounting, user database, provider marketplace, or prompt storage.

## Credential boundary

The two API-key layers are independent:

```text
client -- Authorization: Bearer <router key> --> router
router -- provider-specific credential -------> upstream provider
```

The incoming router key is consumed locally and never forwarded. Provider credentials are loaded once from environment-variable references at startup and never returned or logged.

Self-hosted setups accept connections without a router key by default. To require
one, run `./scripts/run-stackforward --require-key`; the launcher prepares a
random key in its private `.env`. Use `--no-auth` to override an authenticated
configuration. The native binary accepts the same flags; `--require-key` reads
`HAARNESS_ROUTER_KEY`. Web UI login and upstream provider credentials are separate.

In TOML, omit `[auth]` for no authentication, or set `auth.enabled = false`.
For key protection, set `auth.enabled = true` and `auth.router_key_envs` to the
secret environment-variable names. Existing configurations that list keys and
omit `enabled` remain protected. Creating a runtime key does not change this mode.

When `allow_upstream_key_header` is enabled, `X-Upstream-API-Key` supplies a one-request provider credential. It is accepted only with an exact `provider/model`; aliases and fallback are rejected so a caller-owned key can never fall through to a server-owned credential.

## Run

With the existing Stackforward credential YAML, the prepared local configuration is one command:

```bash
cd /Users/alex/work/haarness/haarness-router
./scripts/run-stackforward
```

For rapid local iteration, use the incremental development build:

```bash
HAARNESS_ROUTER_PROFILE=dev ./scripts/run-stackforward
```

The default remains `release`; `debugger` enables full native-debugger information. Reuse the same profile while iterating. See the [build/run skill](.skills/haarness-router-build-run/SKILL.md) for focused tests and cache management.

Override the configured host or port directly from the CLI:

```bash
./scripts/run-stackforward --port 9090
./scripts/run-stackforward --host 0.0.0.0 --port 9090
./scripts/run-stackforward --bind 127.0.0.1:9090
```

The production binary accepts the same options. CLI values take precedence over `[server].bind`; `HAARNESS_ROUTER_HOST`, `HAARNESS_ROUTER_PORT`, and `HAARNESS_ROUTER_BIND` are equivalent environment overrides.

For general configuration, use the conventional layered model: a TOML file plus repeatable CLI overrides. Values are TOML-typed; bare strings are accepted for convenience.

```bash
./scripts/run-stackforward \
  --set server.max_concurrent_requests=30000 \
  --set web_ui.enabled=false \
  --set providers.openai.connect_timeout_ms=3000 \
  --set 'aliases.fast.targets=["openai/gpt-5-nano","gemini/gemini-3.5-flash-lite"]'
```

Add sections or remove optional values the same way:

```bash
./scripts/run-stackforward \
  --set providers.groq.kind=groq \
  --set providers.groq.api_key_env=GROQ_API_KEY \
  --set 'models."groq/llama-3.3-70b-versatile"={}' \
  --unset providers.openai.allow_unknown_fields
```

Removing a provider or model section is also supported, but every alias that references it must be updated in the same invocation or final validation will reject the inconsistent route graph.

The effective precedence is configuration file, then `--set`, then `--unset`, then the dedicated bind flags. `--config -` reads a complete TOML configuration from standard input. Configuration values name secret environment variables; secret values should not be placed in command-line arguments. See [configuration](docs/configuration.md) for the full model and quoting rules.

The launcher creates a random local router key and separate Web UI Basic-auth credentials in the ignored, mode-0600 `.env` file, reads provider keys directly from Stackforward without printing or copying them, builds the release binary, and starts on `127.0.0.1:8080`.

Open [http://127.0.0.1:8080/ui](http://127.0.0.1:8080/ui) and use the `HAARNESS_UI_USERNAME` and `HAARNESS_UI_PASSWORD` values from `.env` when the browser prompts. The dashboard, API, and metrics are served by the same Rust process; there is no separate frontend service. In another terminal, run `source .env` before the client examples.

For a manually managed configuration:

```bash
cd /Users/alex/work/haarness/haarness-router
cp config.example.toml config.toml
export HAARNESS_ROUTER_KEY='replace-with-a-long-random-router-key'
export OPENAI_API_KEY='...'
export ANTHROPIC_API_KEY='...'
export GEMINI_API_KEY='...'
cargo run --release -- --config config.toml
```

The prepared launcher accepts an explicit file too:

```bash
./scripts/run-stackforward --config config.local.toml
```

The Web UI is enabled at `/ui` by default. Configure its path, HTTP Basic protection, and runtime-key administration in `[web_ui]`, or set `enabled = false` for an API-only process. See [Web UI operations](docs/web-ui.md).

Call an exact model:

```bash
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $HAARNESS_ROUTER_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"openai/gpt-5-nano","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

Or request the configured `fast` alias. Aliases are tried in declared order after capability filtering.

## API behavior

- `POST /v1/chat/completions`: streaming and non-streaming OpenAI-shaped responses.
- `GET /v1/models`: configured aliases/models plus cached live provider catalogs, including newly released chat models.
- `/healthz`, `/readyz`: unauthenticated health checks.
- `/metrics`: Prometheus metrics, including upstream failures by route and class. With router authentication on it requires a router key or the scrape token from `auth.metrics_token_env`.
- `/ui`: optional embedded dashboard, model/route explorer, metrics view, OpenAI-compatible playground, and ephemeral router-key administration.
- Roles: developer, system, user, assistant, tool, and legacy function. Native adapters reject legacy shapes that cannot be translated losslessly.
- Tools: function tools across all native adapters; custom tools on compatible targets that declare support.
- Reasoning: `reasoning`, `reasoning_content`, and `reasoning_details`. Anthropic signatures and Gemini thought signatures are preserved as opaque details and constrain continuation routing.
- Inputs: text, images, PDF/file content, audio, and video. Unsupported targets return 400 before an upstream call.
- PDFs: passed natively where configured; otherwise fetched with SSRF protections or decoded from base64 and converted to embedded text. Scanned/no-text PDFs return `unsupported_file` rather than degrading silently.
- Structured output: passed through to compatible endpoints and translated to Anthropic/Gemini native schema controls.
- Unknown fields: passed only by profiles that explicitly enable `allow_unknown_fields`; native adapters reject them.

Requests use a fixed 32 MiB default body bound, 1 MiB SSE-frame bound, provider connection/idle deadlines, slow-client backpressure, client-disconnect cancellation, and a three-failure/30-second per-target circuit breaker. Fallback is limited to safe connection failures or explicit 408/429/5xx responses before the first semantic event.

See [configuration](docs/configuration.md), [Web UI operations](docs/web-ui.md), [provider development](docs/providers.md), [security](docs/security.md), [testing](docs/testing.md), and the [local benchmark result](docs/benchmark-results.md).

For Haarness server configuration, `attach`, automatic model discovery and the
current-model verification workflow, see [Haarness integration](docs/haarness.md).

Official client examples are provided for [Python](examples/openai_python.py) and [JavaScript](examples/openai_typescript.mjs); both point the standard OpenAI SDK at the router's `/v1` base URL.

## Workspace

- `router-protocol`: public OpenAI/OpenRouter-shaped DTOs.
- `router-core`: capabilities, auth, routing, fallback, and bounded circuit breakers (declared-model breakers plus one shared undeclared-model breaker per provider).
- `router-providers`: compatible, Anthropic, Gemini, SSE, and PDF adapters.
- `router-server`: Axum HTTP/SSE surface and metrics.
- `haarness-router`: production server and test/benchmark binaries.

The public DTO baseline follows the [official OpenAI Chat Completions documentation](https://developers.openai.com/api/reference/cli/resources/chat/subresources/completions).
