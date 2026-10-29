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
credentials first. When router authentication is on, the playground and the activity figures need a router
key as well: **Create one for this tab** makes a runtime key and starts using it in one click, or enter a
configured key under **Settings**. The maintainers' launcher, `scripts/run-stackforward`, prepares both in
its private `.env` (see [development](development.md)).

The interface has six views:

- **Overview** — the endpoint with one-click copy and whether the router is ready; each provider with its
  key and circuit state; the model and alias counts; live activity (requests, active streams, error rate and
  average time to the first event, each with a line of what the page sampled in the last minutes); and the command
  that connects Haarness, the OpenAI SDK line and a curl request. With no provider yet it lists the provider
  key variables to set before restarting the router.
- **Playground** — a conversation through the router's own `/v1/chat/completions`. A searchable picker
  holds the aliases and every declared or reported model. Answers stream by default (**Stop** or Esc ends
  one), reasoning sits collapsed above the answer, and tokens, time to the first token and total time follow
  it. **Options** holds the system message, reasoning effort (the efforts the model reports, when it does),
  streaming, temperature, the output token limit, function tools (a tool call is answered inline with its
  result), structured output and a per-request provider key. The paperclip attaches images, PDFs, MP3 or WAV
  audio and video from a file, by pasting or dropping one, or from an HTTPS URL.
- **Models & routes** — each alias's ordered targets with their circuit state, and the models of each
  provider with their capabilities; **Try** opens one in the playground.
- **API keys** — process-local router keys: create one, copy it while it is shown, use it in the playground,
  see when each was last used, and revoke one with an inline confirmation.
- **Observability** — totals, traffic per route with its circuit state, upstream failures by cause, this
  tab's playground requests and the raw Prometheus exposition.
- **Settings** — this tab's router key, the access settings, the endpoints and the version, with no provider
  credentials or secret variable names.

The dashboard follows the system's light or dark appearance until one is picked at the foot of the sidebar.
That choice, the playground's model and its options are kept in the browser's `localStorage`; a provider
key typed into the playground never is. The page loads nothing from other origins, and its
Content-Security-Policy allows only its own script and stylesheet.

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

A runtime router key is displayed once after creation. The process immediately stores only its SHA-256 digest. **Use in playground** selects it for this tab's playground and activity; the browser keeps that selection in `sessionStorage`, not persistent local storage, and forgets it when the key is revoked. Runtime keys and their metadata disappear when the router restarts.

The playground's optional provider-key field sends `X-Upstream-API-Key` for that request only, so it works only when `auth.allow_upstream_key_header = true`. It is not saved, and the router accepts it only for exact `provider/model` targets. It is translated to the provider's configured authentication scheme and cannot fall back to a server-owned provider key.

The Web UI never supports reading or editing provider credentials. Change provider secret environment variables outside the process and restart the router to rotate them.
