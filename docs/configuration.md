# Configuration

Haarness Router uses a layered configuration model common to production services:

1. A version-controlled or mounted TOML file supplies the readable baseline.
2. Repeatable `--set` arguments apply deployment-specific typed overrides.
3. Repeatable `--unset` arguments remove optional values or sections.
4. Dedicated `--host`, `--port`, or `--bind` flags apply the final listener override.
5. Secrets are resolved from environment-variable references after the effective configuration is assembled.

The merged result is validated as one configuration before any listener or upstream client is created. Unknown fields, invalid types, missing secret references, unsafe URLs, invalid routes, and inconsistent settings fail startup.

## Configuration file

```bash
haarness-router --config /etc/haarness/router.toml
```

The Stackforward launcher selects `config.stackforward.toml` automatically and accepts the same trailing router arguments, including an explicit configuration file:

```bash
./scripts/run-stackforward --port 9090 --set web_ui.max_runtime_keys=32
./scripts/run-stackforward --config config.local.toml
```

Use `HAARNESS_ROUTER_CONFIG` to select another file through the launcher:

```bash
HAARNESS_ROUTER_CONFIG="$PWD/config.local.toml" ./scripts/run-stackforward
```

For Unix pipelines or container secret/config injection, `--config -` reads the complete TOML document from standard input:

```bash
haarness-router --config - < /run/config/router.toml
```

## CLI overrides

`--set TOML_PATH=VALUE` accepts booleans, integers, arrays, inline tables, quoted TOML strings, and convenient bare strings:

```bash
haarness-router --config config.toml \
  --set server.max_request_bytes=16777216 \
  --set server.max_concurrent_requests=30000 \
  --set web_ui.enabled=false \
  --set providers.openai.connect_timeout_ms=3000 \
  --set 'auth.router_key_envs=["ROUTER_KEY_PRIMARY","ROUTER_KEY_SECONDARY"]'
```

Shell-quote any override containing spaces, brackets, braces, or quotes. TOML keys containing `/` or `.` must use quoted path segments:

```bash
haarness-router --config config.toml \
  --set 'models."openai/gpt-5-nano".capabilities.reasoning=true' \
  --set 'aliases.fast.targets=["openai/gpt-5-nano","gemini/gemini-3.5-flash-lite"]'
```

New provider/model sections can be assembled from multiple arguments:

```bash
export GROQ_API_KEY='...'

haarness-router --config config.toml \
  --set providers.groq.kind=groq \
  --set providers.groq.api_key_env=GROQ_API_KEY \
  --set 'models."groq/llama-3.3-70b-versatile"={}'
```

`--unset TOML_PATH` removes a field or whole section and fails if the path does not exist. Related routes must remain internally consistent:

```bash
haarness-router --config config.toml \
  --unset providers.anthropic \
  --unset 'models."anthropic/claude-haiku-4-5-20251001"' \
  --set 'aliases.default.targets=["openai/gpt-5-nano","gemini/gemini-3.5-flash-lite"]' \
  --set 'aliases.reasoning.targets=["openai/gpt-5-nano","gemini/gemini-3.5-flash-lite"]' \
  --set 'aliases.vision.targets=["openai/gpt-5-nano","gemini/gemini-3.5-flash-lite"]'
```

Every alias referencing a removed provider or model must be updated in the same effective configuration; otherwise startup validation fails.

## Listener convenience flags

```bash
haarness-router --config config.toml --port 9090
haarness-router --config config.toml --host 0.0.0.0 --port 9090
haarness-router --config config.toml --bind 127.0.0.1:9090
```

`--bind` conflicts with `--host` and `--port`. Equivalent environment overrides are `HAARNESS_ROUTER_BIND`, `HAARNESS_ROUTER_HOST`, and `HAARNESS_ROUTER_PORT`.

## Secrets

Command-line arguments can be visible to other processes and operational tooling. Do not put router keys, provider keys, or Basic-auth passwords into `--set` values. Pass only their environment-variable names:

```bash
export OPENAI_API_KEY='provider-secret'
haarness-router --config config.toml --set providers.openai.api_key_env=OPENAI_API_KEY
```

The router resolves the referenced variable once at startup and never returns it through the API or Web UI.
