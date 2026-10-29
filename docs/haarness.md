# Use with Haarness

Haarness connects to the router as one of its providers:

```text
Haarness client (TUI, Desktop, CLI) → Haarness server → Haarness Router → provider
```

The router only serves models. The Haarness server owns the workspace, tools, provider configuration and
chat history, so clients attach to the Haarness server, never to the router directly.

## Connect

Start the router (see the [quick start](../README.md#quick-start)). On a terminal it prints the command that
connects Haarness to it, with its real address. A self-hosted router needs no access key by default.

- **Desktop:** **Settings → Providers → Connect Haarness Router**.
- **Terminal UI:** **/providers → Connect Haarness Router**.

Enter the router's address and choose **Connect**. Haarness asks for a key only if the router requires one,
and **Advanced** names the connection and chooses its scope (this project or every project). Models are
discovered automatically, and several router connections can live side by side.

From the CLI on the Haarness server:

```sh
haarness provider connect-router http://127.0.0.1:8080
# A name, and this project only:
haarness --trust-project provider connect-router https://router.example \
  --name "Team router" --project
```

For a router that requires a key, add `--key-stdin` and pipe the key in: it goes into Haarness's
credential store, never into its provider configuration or the command line. (Start the router with
`--require-key` to require one; the binary then reads `HAARNESS_ROUTER_KEY`.)

Connections made from Desktop or an attached TUI are checked and saved on the selected Haarness server, so
`localhost` means that server's machine. Provider keys stay on the router; the client only needs the Haarness
server's own authentication, if any. Haarness picks up the new configuration once running work is idle.

The equivalent configuration for a router without a key:

```toml
[providers.router]
name = "Haarness Router"
protocol = "openai-chat-completions"
base_url = "http://127.0.0.1:8080/v1"
requires_auth = false
```

## Choose a router model

The first router connection gets the provider ID `router` (later ones `router_2`, `router_3`, …), so its
models are named `router/<provider>/<model>` and its aliases `router/<alias>` in Haarness:

```sh
# On the server:
haarness --workspace /path/to/server-project --trust-project serve --port 4096

# On a client:
haarness attach http://server.example:4096        # then /models to pick a router model

haarness run --attach http://server.example:4096 \
  --model router/openai/gpt-6-astra "Hello"
```

Use the full name: `openai/gpt-6-astra` selects Haarness's own direct OpenAI provider, even alongside
`--provider router`.

Haarness lists what the router's `GET /v1/models` returns — configured aliases and models plus each
provider's live catalog — so a newly released model shows up without changing either side. A router started
from provider keys alone declares no aliases; add one with `--set`, for example
`haarness-router --set 'aliases.fast.targets=["openai/gpt-5-nano","gemini/gemini-3.5-flash-lite"]'`, and it
appears as `router/fast`. See
[model discovery](api.md#model-discovery). The example configurations declare GPT-6 Astra, GPT-5.6
Sol/Terra/Luna, Claude Fable 5.1, Opus 5 and Sonnet 5, Gemini 4 Argon and Gemini 3.8 Flash beside the
low-cost models the `fast` alias uses; how each provider's reasoning and tools are translated is in
[providers](providers.md#reasoning).

## Verification

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --workspace --all-targets

# Opt-in: live provider requests, a temporary server workspace and a real tmux TUI.
HAARNESS_BIN=../haarness/target/release/haarness \
HAARNESS_ROUTER_BIN=target/release/haarness-router \
python3 scripts/test-haarness-attach.py
```

The live script needs tmux, both release binaries and the maintainers' credential YAML (its path in
`HAARNESS_STACKFORWARD_SECRETS`). It connects through the CLI and through the real TUI wizard, saves
credential-free evidence, stops every process it started, and exits non-zero if a selected provider fails.
`HAARNESS_LIVE_MODELS` picks a comma-separated subset, and `HAARNESS_ROUTER_AUTH=required` also exercises
the access-key challenge.

The last recorded run is [2026-09-12](records/2026-09-12-haarness-attach-live.md).
