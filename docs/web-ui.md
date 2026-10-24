# Embedded Web UI

The Web UI is compiled into `haarness-router` and served by the same Axum process as the OpenAI-compatible API. It adds no Node.js runtime, filesystem asset directory, database, or second listening port.

## Open it

Start the router with any configuration that leaves `web_ui.enabled` on (the default) and open
`http://127.0.0.1:8080/ui`; `/` redirects there. The dashboard always shares the API's listener, so
`--port`, `--host` and `--bind` move both:

```bash
haarness-router --config config.toml --port 9090      # http://127.0.0.1:9090/ui
```

Other dashboard settings can be overridden without editing the file:

```bash
haarness-router --config config.toml \
  --set web_ui.path=/control \
  --set web_ui.allow_runtime_key_management=false \
  --set web_ui.max_runtime_keys=32
```

When `basic_auth_username_env` and `basic_auth_password_env` are set, the browser asks for those
credentials first. The playground and the metrics view also need a router key when router authentication is
on: paste a configured key or create a runtime key on **API keys**. The maintainers' launcher,
`scripts/run-stackforward`, prepares both in its private `.env` (see [development](development.md)).

The interface has six views:

- **Overview** — providers, credential readiness, aliases, circuit states and live request activity.
- **Playground** — streaming and non-streaming chat with system messages, reasoning controls, function
  tools, structured output, images, PDFs and files, audio, video and an optional per-request upstream key.
- **Models & routes** — declared model capabilities and each alias's ordered targets.
- **API keys** — process-local router-key creation, last use and revocation.
- **Observability** — parsed Prometheus metrics, route health, recent playground activity and the raw
  exposition.
- **Settings** — a safe configuration summary with no provider credentials or secret variable names.

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

A runtime router key is displayed once after creation. The process immediately stores only its SHA-256 digest. **Copy & use in playground** copies it and selects it for the playground; the browser keeps that selection in `sessionStorage`, not persistent local storage. Runtime keys and their metadata disappear when the router restarts.

The optional playground upstream-key field sends `X-Upstream-API-Key` for that request only, so it works only when `auth.allow_upstream_key_header = true`. It is not saved, and the router accepts it only for exact `provider/model` targets. It is translated to the provider's configured authentication scheme and cannot fall back to a server-owned provider key.

The Web UI never supports reading or editing provider credentials. Change provider secret environment variables outside the process and restart the router to rotate them.
