<p align="center">
  <a href="https://haarness.ai">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset=".github/assets/banner-dark.png">
      <img alt="Haarness Router — one endpoint, every model provider." src=".github/assets/banner-light.png" width="100%">
    </picture>
  </a>
</p>

<p align="center">
  <a href="#quick-start"><b>Quick start</b></a>&nbsp;&nbsp;·&nbsp;&nbsp;<a href="docs/configuration.md"><b>Configuration</b></a>&nbsp;&nbsp;·&nbsp;&nbsp;<a href="docs/security.md"><b>Security model</b></a>&nbsp;&nbsp;·&nbsp;&nbsp;<a href="docs/haarness.md"><b>Use with Haarness</b></a>&nbsp;&nbsp;·&nbsp;&nbsp;<a href="https://haarness.ai"><b>haarness.ai</b></a>
</p>

<p align="center">
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-dba273?style=flat-square&labelColor=232120"></a>
  <img alt="Written in Rust" src="https://img.shields.io/badge/Rust-1.94-eae5dd?style=flat-square&labelColor=232120&logo=rust&logoColor=eae5dd">
  <img alt="OpenAI-compatible API" src="https://img.shields.io/badge/API-OpenAI--compatible-eae5dd?style=flat-square&labelColor=232120">
</p>

**Haarness Router** is a stateless, high-throughput Rust service that exposes one OpenAI-compatible
Chat Completions API and routes each request to **OpenAI, Anthropic, Gemini, Groq, Mistral, xAI,
DeepSeek, Together, Fireworks** or any OpenAI-compatible endpoint you add.

Point any OpenAI SDK at it and you get every provider behind one address, with aliases and fallback,
streaming, tools, reasoning, images, PDFs, audio and video translated to each provider's native shape.
It deliberately contains **no billing, credit accounting, user database, provider marketplace or prompt
storage** — it routes, and that is all.

## Highlights

- **One API, native adapters.** Native Anthropic and Gemini adapters, not prompt-shaped shims: tool
  calls, structured output, reasoning (with Anthropic and Gemini thought signatures preserved) and
  multimodal input are translated losslessly or refused with a 400 before any upstream call.
- **Aliases and fallback.** `fast` can mean "this model, else that one". Targets are tried in order
  after capability filtering; fallback happens only on safe connection failures or 408/429/5xx before
  the first event, behind a per-target circuit breaker.
- **Keys stay on the server.** Clients present a router key that is never forwarded; provider
  credentials are read once from environment variables and never returned or logged.
- **Built for load.** Bounded request bodies and SSE frames, connection and idle deadlines,
  slow-client backpressure, cancellation on client disconnect, and Prometheus metrics.
- **A dashboard in the same binary.** `/ui` serves a model and route explorer, live metrics, a
  playground and ephemeral router-key administration — no separate front end to deploy.

## Quick start

You need the Rust toolchain in `rust-toolchain.toml` (rustup installs it on first use).

```bash
cp config.example.toml config.toml
export OPENAI_API_KEY='...' ANTHROPIC_API_KEY='...' GEMINI_API_KEY='...'
cargo run --release -- --config config.toml
```

The router listens on `127.0.0.1:8080` and opens its dashboard at
[http://127.0.0.1:8080/ui](http://127.0.0.1:8080/ui). Call an exact model:

```bash
curl http://127.0.0.1:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"openai/gpt-5-nano","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

…or ask for the configured `fast` alias. Any OpenAI client works unchanged — see the
[Python](examples/openai_python.py) and [JavaScript](examples/openai_typescript.mjs) examples:

```python
import os
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8080/v1", api_key=os.getenv("HAARNESS_ROUTER_KEY", "unused"))
client.chat.completions.create(model="fast", messages=[{"role": "user", "content": "Hi"}])
```

To use it from Haarness, connect it by address in Desktop, the TUI or the CLI; models refresh on
their own. See [Haarness integration](docs/haarness.md).

## Credential boundary

The two API-key layers are independent:

```text
client -- Authorization: Bearer <router key> --> router
router -- provider-specific credential -------> upstream provider
```

The incoming router key is consumed locally and never forwarded. Provider credentials are loaded once
from environment-variable references at startup and never returned or logged.

Self-hosted setups accept connections without a router key by default. In TOML, omit `[auth]` for no
authentication, or set `auth.enabled = false`. For key protection, set `auth.enabled = true` and
`auth.router_key_envs` to the secret environment-variable names. Existing configurations that list keys
and omit `enabled` remain protected. Creating a runtime key does not change this mode. The native
binary also accepts `--require-key`, which reads `HAARNESS_ROUTER_KEY`, and `--no-auth` to override an
authenticated configuration. Web UI login and upstream provider credentials are separate.

When `allow_upstream_key_header` is enabled, `X-Upstream-API-Key` supplies a one-request provider
credential. It is accepted only with an exact `provider/model`; aliases and fallback are rejected so a
caller-owned key can never fall through to a server-owned credential.

## Configuration

Configuration is a TOML file plus repeatable typed overrides on the command line:

```bash
haarness-router --config config.toml \
  --set server.max_concurrent_requests=30000 \
  --set web_ui.enabled=false \
  --set providers.openai.connect_timeout_ms=3000 \
  --set 'aliases.fast.targets=["openai/gpt-5-nano","gemini/gemini-3.5-flash-lite"]'
```

Add sections or remove optional values the same way:

```bash
haarness-router --config config.toml \
  --set providers.groq.kind=groq \
  --set providers.groq.api_key_env=GROQ_API_KEY \
  --set 'models."groq/llama-3.3-70b-versatile"={}' \
  --unset providers.openai.allow_unknown_fields
```

Removing a provider or model section is also supported, but every alias that references it must be
updated in the same invocation or final validation will reject the inconsistent route graph.

The effective precedence is configuration file, then `--set`, then `--unset`, then the dedicated
`--host`, `--port` and `--bind` flags (or `HAARNESS_ROUTER_HOST`, `HAARNESS_ROUTER_PORT` and
`HAARNESS_ROUTER_BIND`). `--config -` reads a complete TOML configuration from standard input.
Configuration values *name* secret environment variables; secret values never belong in command-line
arguments. The merged result is validated as one configuration before any listener or upstream client
starts. See [configuration](docs/configuration.md) for the full model and quoting rules,
`config.example.toml` for a starting point and `config.all-providers.example.toml` for every built-in
provider.

The Web UI is enabled at `/ui` by default. Configure its path, HTTP Basic protection and runtime-key
administration in `[web_ui]`, or set `enabled = false` for an API-only process. See
[Web UI operations](docs/web-ui.md).

## API behaviour

| Endpoint | Behaviour |
| --- | --- |
| `POST /v1/chat/completions` | Streaming and non-streaming OpenAI-shaped responses. |
| `GET /v1/models` | Configured aliases and models plus cached live provider catalogs, including newly released chat models. |
| `/healthz`, `/readyz` | Unauthenticated health checks. |
| `/metrics` | Prometheus metrics, including upstream failures by route and class. With authentication on it needs a router key or the scrape token from `auth.metrics_token_env`. |
| `/ui` | Optional embedded dashboard, model and route explorer, metrics, playground and ephemeral router-key administration. |

- **Roles:** developer, system, user, assistant, tool and legacy function. Native adapters reject legacy
  shapes that cannot be translated losslessly.
- **Tools:** function tools across all native adapters; custom tools on compatible targets that declare
  support.
- **Reasoning:** `reasoning`, `reasoning_content` and `reasoning_details`. Anthropic signatures and
  Gemini thought signatures are preserved as opaque details and constrain continuation routing.
- **Inputs:** text, images, PDF/file content, audio and video. Unsupported targets return 400 before an
  upstream call.
- **PDFs:** passed natively where configured; otherwise fetched with SSRF protections or decoded from
  base64 and converted to embedded text. Scanned PDFs with no text return `unsupported_file` rather than
  degrading silently.
- **Structured output:** passed through to compatible endpoints and translated to Anthropic and Gemini
  native schema controls.
- **Unknown fields:** passed only by profiles that explicitly enable `allow_unknown_fields`; native
  adapters reject them.

Requests use a fixed 32 MiB default body bound, a 1 MiB SSE-frame bound, provider connection and idle
deadlines, slow-client backpressure, client-disconnect cancellation, and a three-failure, 30-second
per-target circuit breaker. The public DTO baseline follows the
[official OpenAI Chat Completions documentation](https://developers.openai.com/api/reference/cli/resources/chat/subresources/completions).

## Development launcher

`scripts/run-stackforward` prepares a complete local setup in one command: it creates a random router
key and separate Web UI Basic-auth credentials in the ignored, mode-0600 `.env`, reads provider keys
from the maintainers' credential file without printing or copying them, builds the binary and starts on
`127.0.0.1:8080` with `config.stackforward.toml`.

```bash
./scripts/run-stackforward                        # release build
HAARNESS_ROUTER_PROFILE=dev ./scripts/run-stackforward   # incremental dev build
./scripts/run-stackforward --require-key          # require HAARNESS_ROUTER_KEY
./scripts/run-stackforward --port 9090            # or --host / --bind
./scripts/run-stackforward --config config.local.toml
```

Open the dashboard with the `HAARNESS_UI_USERNAME` and `HAARNESS_UI_PASSWORD` values from `.env`, and run
`source .env` before the client examples. `debugger` is a third profile with full native-debugger
information. See the [build and run skill](.skills/haarness-router-build-run/SKILL.md) for focused tests
and cache management.

## Workspace

| Crate | Role |
| --- | --- |
| `router-protocol` | Public OpenAI/OpenRouter-shaped DTOs. |
| `router-core` | Capabilities, auth, routing, fallback and bounded circuit breakers (declared-model breakers plus one shared undeclared-model breaker per provider). |
| `router-providers` | Compatible, Anthropic, Gemini, SSE and PDF adapters. |
| `router-server` | The Axum HTTP/SSE surface and metrics. |
| `haarness-router` | The production server and the test and benchmark binaries. |

[Haarness Cloud](https://github.com/haarness/haarness-cloud) reuses these crates for its managed,
metered router; nothing billing-related lives here.

## Documentation

- [Configuration](docs/configuration.md) — layered TOML, overrides, authentication, metrics, logs and secrets.
- [Security model](docs/security.md) — the credential boundary and what the router refuses.
- [Provider development](docs/providers.md) — built-in profiles, model capabilities, adding a native adapter.
- [Web UI operations](docs/web-ui.md) — the embedded dashboard and runtime keys.
- [Testing and benchmarks](docs/testing.md) and the [local benchmark result](docs/benchmark-results.md).
- [Haarness integration](docs/haarness.md) — server configuration, `attach`, model discovery and verification.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) for the checks every change runs. Everyone taking part follows
the Haarness [Code of Conduct](https://github.com/haarness/.github/blob/main/CODE_OF_CONDUCT.md); report
security problems privately as described in [SECURITY.md](https://github.com/haarness/.github/blob/main/SECURITY.md).

## License

Haarness Router is licensed under the [MIT License](LICENSE). The Haarness name and logo are trademarks
and are not covered by the license.
