# Haarness attach and current models

The connection chain is:

```text
Haarness attach client → Haarness server → Haarness Router → provider
```

The router exposes a model API. Attach to the Haarness server, which owns the
workspace, tools, provider configuration and conversation history.

## Configure the server

Start the router with `./scripts/run-stackforward`. Self-hosted connections need
no router access key by default.

In Desktop, open **Settings → Providers → Connect Haarness Router**. In the TUI,
use **/providers → Connect Haarness Router**. Enter the router address and choose
**Connect**. Haarness asks for a key only if the router requires one. **Advanced**
lets you name the connection and choose its project scope. Models are discovered
automatically; multiple router connections are supported.

For CLI setup on the Haarness server:

```sh
haarness provider connect-router http://127.0.0.1:8080
# Optional name and project scope:
haarness --trust-project provider connect-router https://router.example \
  --name "Team router" --project
```

Use `--key-stdin` when a protected router requires an access key. The key goes
into Haarness's credential store, never its provider configuration or command
arguments. To enable router authentication with the prepared launcher, run
`./scripts/run-stackforward --require-key`; it prepares a key in the private
`.env`. The underlying binary reads `HAARNESS_ROUTER_KEY` for that flag.

Connections made through Desktop or an attached TUI are checked and saved on
the selected Haarness server. `localhost` refers to that server. Provider keys
stay on the router, and the attach client only needs the Haarness server's own
authentication, if configured. Configuration refreshes automatically when active
work is idle.

The equivalent manual no-key configuration is:

```toml
[providers.router]
name = "Haarness Router"
protocol = "openai-chat-completions"
base_url = "http://127.0.0.1:8080/v1"
requires_auth = false
```

```sh
# On the server:
haarness --workspace /path/to/server-project --trust-project serve --port 4096

# On the client:
haarness attach http://server.example:4096
# In the TUI, use /models to select a router model.

haarness run --attach http://server.example:4096 \
  --model router/openai/gpt-6-astra "Hello"

haarness run --attach http://server.example:4096 \
  --model router/anthropic/claude-fable-5-1 "Hello"
```

Use the full name `router/openai/gpt-6-astra`. `openai/gpt-6-astra` selects
Haarness's direct OpenAI provider, even alongside `--provider router`.

## New models

`GET /v1/models` (authenticated when configured) discovers account-visible chat models from each
configured provider's model-list endpoint. OpenAI-compatible endpoints,
Anthropic and Gemini are supported. Discovery follows pagination, caches for
five minutes, combines simultaneous refreshes, and keeps the last complete
catalog after an error. Each provider refresh is bounded to ten seconds, ten
pages and four MiB per page. Router authentication and caller-supplied upstream
keys are never forwarded to model discovery: it uses server provider keys.

Configured models and aliases remain available as an offline fallback. Exact
`provider/model` requests do not require a catalog entry, so newly released
models using an existing supported protocol can be called immediately. Model
discovery is not a guarantee that an account can generate with every listed
model. Embeddings, realtime and media-generation-only models are excluded.

The prepared configuration includes GPT-6 Astra, GPT-5.6 Sol/Terra/Luna,
Claude Fable 5.1, Opus 5, Sonnet 5, and Gemini 3.8 Flash alongside the existing
low-cost models. Existing default/fast alias targets are unchanged.

For native OpenAI, GPT-6, GPT-5.6 and later numbered families, Codex and Pro
models use the Responses API internally. The public router API remains Chat
Completions. Text, images, function/custom tools, structured output, usage,
reasoning summaries and encrypted reasoning continuation are translated.
Responses uses `store: false`; continuation state travels in opaque
`reasoning_details` that clients must preserve. Unsupported Chat-only fields
return a named HTTP 400 instead of disappearing during translation. This does
not expose Responses-specific hosted tools, WebSocket steering or async tools
through the Chat Completions API.

Claude uses its native adaptive/manual thinking behavior. Fable 5.1 and Mythos
5.1 support `auto` and `none` tool selection; forced selection returns a named
HTTP 400 because those models do not support it. Gemini tool schemas
use `parametersJsonSchema` so nullable fields and schema constraints remain
intact. Gemini streamed calls retain distinct IDs, ordered arguments and thought
signatures across chunks and tool-result continuations.

## Verification

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --workspace --all-targets

# Opt-in: live provider requests, temporary server workspace and real tmux TUI.
HAARNESS_BIN=../haarness/target/release/haarness \
HAARNESS_ROUTER_BIN=target/release/haarness-router \
python3 scripts/test-haarness-attach.py
```

The live script saves credential-free evidence and exits nonzero if a selected
provider fails. Use `HAARNESS_LIVE_MODELS` to choose a comma-separated subset.
Set `HAARNESS_ROUTER_AUTH=required` to exercise the access-key challenge as well.
The script also connects through CLI and the actual tmux TUI wizard.
All test processes are stopped afterward.

On 2026-09-12, attached streaming, server-side read tools and continuation passed
for GPT-6 Astra, all three GPT-5.6 models, Claude Fable 5.1, Opus 5 and Sonnet 5.
The real attached TUI restored history, listed current models, completed a live
turn, and restored that turn after detach/reattach. Gemini's schema rejection
was repaired; the available Stackforward project then returned Google HTTP 403
`PERMISSION_DENIED`, preventing a successful live Gemini generation test.

Protocol references: [OpenAI model guidance](https://developers.openai.com/api/docs/guides/latest-model),
[stateless reasoning](https://developers.openai.com/api/docs/guides/reasoning),
[Claude tool selection](https://platform.claude.com/docs/en/claude_api_primer),
[Gemini function declarations](https://ai.google.dev/api/generate-content#FunctionDeclaration),
and [Gemini thought signatures](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures).
