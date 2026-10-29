# Configuration

Haarness Router assembles its configuration in layers:

1. A TOML file supplies the readable baseline: the one named by `--config` or `HAARNESS_ROUTER_CONFIG`,
   else `config.toml` in the current folder. With neither, the router
   [starts from its built-in defaults](#starting-without-a-configuration-file) and the provider keys it
   finds in the environment.
2. Repeatable `--set TOML_PATH=VALUE` arguments apply typed overrides, in order.
3. `--require-key` or `--no-auth`, if given, switch authentication on or off.
4. Repeatable `--unset TOML_PATH` arguments remove values or whole sections.
5. `--host`, `--port` or `--bind` (or `HAARNESS_ROUTER_HOST`, `HAARNESS_ROUTER_PORT`, `HAARNESS_ROUTER_BIND`)
   override the listening address last.
6. Secrets are read from the environment variables the configuration names.

The merged result is validated as one configuration before any listener or upstream client starts.
Unknown fields, wrong types, unset secret variables, non-HTTPS provider URLs, aliases that point at unknown
providers and inconsistent settings all stop startup with a message naming the field.

Start from [`config.example.toml`](../config.example.toml) (OpenAI, Anthropic and Gemini with a `fast`
alias) or [`config.all-providers.example.toml`](../config.all-providers.example.toml) (every OpenAI-compatible
preset plus a local endpoint). [Providers](providers.md) shows Vertex AI and Bedrock.

## Starting without a configuration file

`haarness-router` with no `--config`, no `HAARNESS_ROUTER_CONFIG` and no `config.toml` in the current folder
starts from the defaults in the [reference](#reference) below and sets up one provider for each of these
variables that is set and not empty:

| Variable | Provider | Kind |
| --- | --- | --- |
| `OPENAI_API_KEY` | `openai` | `openai` (with `allow_unknown_fields`, as in the examples) |
| `ANTHROPIC_API_KEY` | `anthropic` | `anthropic` |
| `GEMINI_API_KEY`, else `GOOGLE_API_KEY` | `gemini` | `gemini` |
| `GROQ_API_KEY` | `groq` | `groq` |
| `MISTRAL_API_KEY` | `mistral` | `mistral` |
| `XAI_API_KEY` | `xai` | `xai` |
| `DEEPSEEK_API_KEY` | `deepseek` | `deepseek` |
| `TOGETHER_API_KEY` | `together` | `together` |
| `FIREWORKS_API_KEY` | `fireworks` | `fireworks` |
| `AWS_BEARER_TOKEN_BEDROCK` | `bedrock` | `amazon-bedrock`, in the region of `AWS_REGION` or `AWS_DEFAULT_REGION` (default `us-east-1`) |

No models or aliases are declared: `GET /v1/models` lists what each provider's live catalog offers, and any
model is called as `provider/model`. The router listens on `127.0.0.1:8080` with the dashboard on and no
router key, as a file with only `[providers]` would.

With no key at all it still starts, so the dashboard can show what to set, and a chat request answers
`400 no_providers_configured` with the list of variables. A request for a provider in the table whose
variable is not set answers `400 provider_not_configured`, naming it. Set the variable and restart.

`--set`, `--unset`, `--require-key`, `--no-auth` and the address flags apply on top, so a quick alias or a
local endpoint needs no file:

```bash
haarness-router --set 'aliases.fast.targets=["openai/gpt-5-nano","gemini/gemini-3.5-flash-lite"]'
haarness-router --require-key --host 0.0.0.0     # clients then need HAARNESS_ROUTER_KEY
```

A configuration named with `--config` or `HAARNESS_ROUTER_CONFIG` that cannot be read still stops startup.

## Configuration file

```bash
haarness-router --config /etc/haarness/router.toml
```

`--config -` reads the complete TOML document from standard input, for pipelines and container
config injection:

```bash
haarness-router --config - < /run/config/router.toml
```

## Command-line overrides

`--set TOML_PATH=VALUE` accepts booleans, integers, arrays, inline tables, quoted TOML strings and bare
strings:

```bash
haarness-router --config config.toml \
  --set server.max_request_bytes=16777216 \
  --set server.max_concurrent_requests=30000 \
  --set web_ui.enabled=false \
  --set providers.openai.connect_timeout_ms=3000 \
  --set 'auth.router_key_envs=["ROUTER_KEY_PRIMARY","ROUTER_KEY_SECONDARY"]'
```

Shell-quote any override with spaces, brackets, braces or quotes. Keys that contain `/` or `.` need quoted
path segments:

```bash
haarness-router --config config.toml \
  --set 'models."openai/gpt-5-nano".capabilities.reasoning=true' \
  --set 'aliases.fast.targets=["openai/gpt-5-nano","gemini/gemini-3.5-flash-lite"]'
```

New provider and model sections can be assembled from several arguments:

```bash
export GROQ_API_KEY='...'

haarness-router --config config.toml \
  --set providers.groq.kind=groq \
  --set providers.groq.api_key_env=GROQ_API_KEY \
  --set 'models."groq/llama-3.3-70b-versatile"={}'
```

`--unset TOML_PATH` removes a field or a whole section and fails if the path does not exist. Every model
and alias that refers to a removed provider must change in the same invocation, or validation rejects the
result. With `config.example.toml`, removing Gemini looks like this:

```bash
haarness-router --config config.toml \
  --unset providers.gemini \
  --unset 'models."gemini/gemini-4-argon"' \
  --unset 'models."gemini/gemini-3.8-flash"' \
  --unset 'models."gemini/gemini-3.5-flash-lite"' \
  --set 'aliases.fast.targets=["openai/gpt-5-nano","anthropic/claude-haiku-4-5-20251001"]'
```

## Listening address

```bash
haarness-router --config config.toml --port 9090
haarness-router --config config.toml --host 0.0.0.0 --port 9090
haarness-router --config config.toml --bind 127.0.0.1:9090
```

`--bind` conflicts with `--host` and `--port`. The default is `127.0.0.1:8080`, loopback only.

## Authentication and metrics

```toml
[auth]
enabled = true
router_key_envs = ["HAARNESS_ROUTER_KEY"]
# Optional: a token that can read /metrics but cannot call models.
metrics_token_env = "HAARNESS_METRICS_TOKEN"
```

- Omitting `[auth]`, or `enabled = false`, accepts model API requests without a key. An older
  configuration that lists `router_key_envs` without `enabled` stays protected.
- With authentication on, `/v1` and `/metrics` require `Authorization: Bearer <secret>`. `/metrics`
  accepts any router key or the scrape token; the scrape token is refused everywhere else. A missing or
  empty variable stops startup.
- With authentication off, `/metrics` is as open as `/v1` and `metrics_token_env` is ignored.
- `--require-key` is shorthand for `enabled = true` with `router_key_envs = ["HAARNESS_ROUTER_KEY"]`;
  `--no-auth` sets `enabled = false`.
- Keys created in the Web UI are extra, process-local keys; they do not switch authentication on.

See [observability](observability.md) for the metrics themselves and a Prometheus scrape configuration.

## Reference

Every key, with its default. Durations are milliseconds unless the name says otherwise.

### `[server]`

| Key | Default | Meaning |
| --- | --- | --- |
| `bind` | `"127.0.0.1:8080"` | Listening address. |
| `max_request_bytes` | `33554432` (32 MiB) | Largest accepted request body. |
| `max_concurrent_requests` | `20000` | Requests in flight before new ones get `429 router_overloaded`. |
| `first_byte_timeout_ms` | `120000` | Deadline for an upstream's response headers and first stream event. |
| `drain_seconds` | `30` | How long shutdown (SIGINT or SIGTERM) waits for open requests. |

### `[auth]`

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | unset (off, unless `router_key_envs` is set) | Require a router key on `/v1` and `/metrics`. |
| `router_key_envs` | `[]` | Environment variables that each hold a valid router key. |
| `metrics_token_env` | unset | Environment variable holding a token that may only read `/metrics`. |
| `allow_upstream_key_header` | `false` | Accept `X-Upstream-API-Key` for one request to an exact `provider/model`. |

### `[web_ui]`

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Serve the dashboard and its API, and redirect `/` to it. |
| `path` | `"/ui"` | Where it is served: absolute, not `/`, no trailing slash, letters, digits, `-`, `_` and `/`. |
| `basic_auth_username_env`, `basic_auth_password_env` | unset | Both or neither: environment variables holding HTTP Basic credentials. |
| `allow_runtime_key_management` | `true` | Allow creating and revoking router keys in the UI. |
| `max_runtime_keys` | `64` | Runtime keys allowed at once, 1 to 1024. |

### `[providers.<name>]`

`<name>` is yours (letters, digits, `-`, `_`) and becomes the prefix of every model ID: `[providers.work]`
serves `work/<model>`.

| Key | Default | Meaning |
| --- | --- | --- |
| `kind` | required | One of the [built-in kinds](providers.md#built-in-kinds). |
| `base_url` | the kind's default | Required for `openai-compatible`. Must be HTTPS unless `allow_insecure_http`. |
| `api_key_env` | unset | Environment variable holding the provider credential. |
| `auth_header` | `Authorization: Bearer` | A different header for the key, for example `x-api-key`; `authorization` switches Gemini to Bearer tokens (Vertex AI). |
| `auth_scheme` | unset | A prefix for a custom `auth_header`, for example `Bearer`. |
| `capabilities` | the kind's defaults | The provider-wide [capability object](providers.md#models-and-capabilities). |
| `allow_unknown_fields` | `false` | Accept request fields the router does not model for this provider and forward them; native adapters still refuse what they cannot translate. |
| `allow_insecure_http` | `false` | Allow an `http://` base URL; meant for loopback tests. |
| `connect_timeout_ms` | `5000` | TCP and TLS connection deadline. |
| `stream_idle_timeout_ms` | `60000` | Longest silence allowed inside a stream. |
| `max_error_body_bytes` | `4096` | How much of an upstream error body is read to find its error type. |

### `[models."<provider>/<model>"]`

Declaring a model lists it in `/v1/models` even when discovery fails, gives it its own circuit breaker and
metrics labels, and lets it override capabilities:

| Key | Default | Meaning |
| --- | --- | --- |
| `capabilities` | the provider's | A complete capability object for this model; it replaces the provider's. |

An empty table (`[models."openai/gpt-5-nano"]`) declares a model with its provider's capabilities.

### `[aliases.<name>]`

| Key | Meaning |
| --- | --- |
| `targets` | Non-empty ordered list of `provider/model` targets. Alias names cannot contain `/`. |

## Startup and logs

On a terminal the router starts with a short summary on standard error — its version, the API and dashboard
addresses, the providers it set up (or the variables to set), where its configuration came from and the
command that connects Haarness — and then logs readable text. When standard error is not a terminal
(a service manager, a container, a pipe) it writes JSON lines instead, starting with a
`haarness router listening` line. `--log-format text|json` or `HAARNESS_ROUTER_LOG_FORMAT` chooses either
explicitly; `NO_COLOR` turns colour off. `RUST_LOG` replaces the default `info,tower_http=warn` filter. See
[observability](observability.md#logs) for the fields and [security](security.md) for what is never logged.

A port that is already taken stops startup with `port 8080 on 127.0.0.1 is already in use` and a `--port`
to try instead.

## Secrets

Command-line arguments are visible to other processes and to operational tooling. Never put router keys,
provider keys, metrics tokens or Basic-auth passwords into `--set` values; pass only the names of the
variables that hold them:

```bash
export OPENAI_API_KEY='provider-secret'
haarness-router --config config.toml --set providers.openai.api_key_env=OPENAI_API_KEY
```

The router reads each variable once at startup and never returns it through the API or the Web UI. To
rotate a provider key, change the variable and restart the router.
