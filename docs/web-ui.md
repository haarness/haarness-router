# Embedded Web UI

The Web UI is compiled into `haarness-router` and served by the same Axum process as the OpenAI-compatible API. It adds no Node.js runtime, filesystem asset directory, database, or second listening port.

## Start the prepared local setup

```bash
cd /Users/alex/work/haarness/haarness-router
./scripts/run-stackforward
```

To use another address without editing TOML, pass `--port`, `--host` plus `--port`, or a complete `--bind` address:

```bash
./scripts/run-stackforward --port 9090
./scripts/run-stackforward --host 0.0.0.0 --port 9090
./scripts/run-stackforward --bind 127.0.0.1:9090
```

The embedded UI and OpenAI-compatible API always share the resulting listener.

Other UI settings may be overridden without editing TOML:

```bash
./scripts/run-stackforward \
  --set web_ui.path=/control \
  --set web_ui.allow_runtime_key_management=false \
  --set web_ui.max_runtime_keys=32
```

Open `http://127.0.0.1:8080/ui`. The browser's HTTP Basic prompt uses `HAARNESS_UI_USERNAME` and `HAARNESS_UI_PASSWORD` from the ignored, mode-0600 `.env` file. The OpenAI-compatible playground separately needs a router key; use `HAARNESS_ROUTER_KEY` from the same file or create a runtime key on the API keys screen.

The interface includes:

- An operational overview with providers, credential readiness, aliases, and circuit states.
- A playground for streaming and non-streaming chat, system messages, reasoning controls, function tools, structured output, images, PDFs/files, audio, video, and per-request upstream credentials.
- Declared model capabilities and ordered alias route maps.
- Process-local router-key creation, last-use visibility, and revocation.
- Parsed Prometheus metrics, route health, recent playground activity, and raw metrics.
- A safe settings summary that contains no provider credentials or secret environment-variable names.

## Configuration

```toml
[web_ui]
enabled = true
path = "/ui"
basic_auth_username_env = "HAARNESS_UI_USERNAME"
basic_auth_password_env = "HAARNESS_UI_PASSWORD"
allow_runtime_key_management = true
max_runtime_keys = 64
```

`enabled = false` removes the root redirect, UI assets, and UI administration APIs while leaving `/v1`, health, readiness, and metrics endpoints intact. `path` may be changed to another non-root absolute path without a trailing slash.

Both Basic-auth environment references must be configured together. Omitting both makes the UI accessible to anyone who can reach the listening socket, which is reasonable only for a protected loopback development environment. For a remote deployment, terminate TLS at a trusted reverse proxy and protect the UI there as well.

`allow_runtime_key_management = false` keeps the API key inventory read-only and rejects creation or revocation. `max_runtime_keys` bounds the in-memory authentication scan and inventory.

## Credential behavior

UI Basic credentials, router keys, and upstream provider credentials are separate:

```text
browser -- Basic credentials --> Web UI administration
client  -- Bearer router key --> OpenAI-compatible router API
router  -- provider key ------> upstream provider
```

Bootstrap router keys come from the configured environment variables and cannot be listed or revoked. The UI shows only their count.

A runtime router key is displayed once after creation. The process immediately stores only its SHA-256 digest. Copy it into a client or select “Use in playground”; the browser keeps that selection in `sessionStorage`, not persistent local storage. Runtime keys and their metadata disappear when the router restarts.

The optional playground upstream-key field sends `X-Upstream-API-Key` for that request only. It is not saved, and the router accepts it only for exact `provider/model` targets. It is translated to the provider's configured authentication scheme and cannot fall back to a server-owned provider key.

The Web UI never supports reading or editing provider credentials. Change provider secret environment variables outside the process and restart the router to rotate them.
