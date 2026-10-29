---
name: haarness-router-build-run
description: Build, run, prototype, and test Haarness Router and its embedded Web UI using fast local Rust builds with controlled cache growth. Use for local development, startup, validation, and build-space cleanup in this repository.
---

# Haarness Router build and run

Run from the Router repository containing `Cargo.toml` and `scripts/run-stackforward`. Router is one Rust service: its dashboard HTML/CSS/JS are embedded from `crates/router-server/assets` by `include_str!`. UI source changes require a Rust rebuild and process restart; there is no npm install or separate Vite service here.

## Rapid local development

Use the same toolchain, target directory and `dev` profile throughout an edit session. The default development profile retains incremental compilation and limits debug information to file/line backtraces. Do not use release builds for ordinary UI iteration or clean between edits.

```sh
cargo build --bin haarness-router
# When credentials referenced by the chosen configuration are already available:
target/debug/haarness-router --config config.example.toml --host 127.0.0.1 --port 8080
```

Select the intended configuration; the example is a schema/example, not a promise that its referenced provider credentials exist. Without `--config` (or `HAARNESS_ROUTER_CONFIG`) and without a `config.toml` in the current folder, the binary starts from built-in defaults with one provider per standard key variable it finds (see [configuration](../../docs/configuration.md#starting-without-a-configuration-file)); an explicit missing file still fails. On a terminal it prints a startup summary and text logs; otherwise JSON (`--log-format` overrides). Check existing processes and choose an unused port. Preserve `.env`, credential sources and user configuration. A foreground instance can be stopped with Ctrl+C; for background work record and stop only the process started for that task. Reloading the browser does not replace the running server binary.

For the existing Stackforward configuration and credentials, use the supported launcher with development compilation:

```sh
HAARNESS_ROUTER_PROFILE=dev ./scripts/run-stackforward --host 127.0.0.1 --port 8080
```

The launcher builds incrementally, reads provider credentials directly through its Ruby loader, maintains the private local `.env`, and forwards CLI arguments. Never print or copy the credential YAML into reports, command arguments or fixtures. The launcher defaults to `release` when `HAARNESS_ROUTER_PROFILE` is omitted; it also accepts `debugger`. Keep that default for an explicitly requested optimized service, but set `dev` for rapid prototyping.

The Web UI is normally `/ui`; it may require its separate configured Basic-auth credentials. Router-key authentication and upstream provider keys are distinct. Preserve the requested authentication mode; local run instructions do not authorize changing deployed access controls.

After startup, check the selected instance's `/healthz` and `/readyz`. Inspect `/ui` in a browser for UI changes, and exercise the relevant API for server changes. Health alone does not prove that provider calls succeed. See [configuration](../../docs/configuration.md), [Web UI](../../docs/web-ui.md), and [Haarness integration](../../docs/haarness.md) only as needed.

## No-credential local fixture

For deterministic local transport work, start the checked-in mock provider and Router in separate terminals, with unused loopback ports:

```sh
cargo run --bin mock-provider -- --bind 127.0.0.1:9090
```

```sh
HAARNESS_ROUTER_KEY=local-router-key MOCK_PROVIDER_KEY=unused-local-provider-key \
  cargo run --bin haarness-router -- --config tests/config.mock.toml --port 8080
```

These are fixture keys, not external credentials. The mock configuration disables the Web UI and uses `mock/test`; do not use it to claim production provider or Web UI coverage. Stop only these owned fixture processes after testing.

## Focused verification, then broader gates when needed

```sh
cargo check -p router-server
cargo test -p router-protocol
cargo test -p router-core
cargo test -p router-providers
cargo test -p router-server
```

For launcher changes, `python3 scripts/test-launcher-profile.py` checks profile selection and argument forwarding without credentials or compilation.

Choose affected packages/tests rather than running every line after every edit. Rust changes that affect the executable require `cargo build --bin haarness-router` before launch. Embedded asset edits also rebuild that executable. For cross-provider/runtime work, use the broader deterministic suite in [testing](../../docs/testing.md). Live tests are opt-in, bounded provider calls; load tests and `--release --workspace --all-targets` are special validation workflows, not the default loop.

For an optimized binary or explicit performance measurement:

```sh
cargo build --release --bin haarness-router
```

Full native variable inspection uses `cargo build --profile debugger --bin haarness-router` and `target/debugger/haarness-router`. Each profile stores a separate artifact set; create only the profiles needed for the task.

## Space management without slowing every build

- Keep `target/debug` and dependency caches warm while iterating. Do not clean on each launch, test, or build. Do not reinstall toolchains or create fresh target directories for routine iterations.
- Check `df -h .` and `du -sh target` when space is constrained or growth is unexpected. Incremental state is disposable, but deleting it makes subsequent recompilation slower. Reduced debug information limits its footprint; it does not impose a disk quota.
- If space is unusually constrained, use `CARGO_INCREMENTAL=0` consistently for that session. Default prototyping keeps it enabled; avoid repeatedly toggling compiler settings.
- With both sibling checkouts in the cleanup scope, Haarness provides a guarded preview: `python3 ../haarness/scripts/clean-build-artifacts.py --router --incremental-only`. This includes **both** workspaces. Add `--apply` only after reviewing that scope; the helper locks Cargo profiles and refuses artifacts in use.
- In a standalone Router checkout, leave warm caches alone unless reclaiming them is actually necessary. For deliberate cold cleanup, first ensure no build/test or running Router needs the development artifacts, then `cargo clean --profile dev`. This removes the debug binary as well as intermediates, so rebuild before restarting it. `cargo clean --profile debugger` removes an unused full-debug profile. Avoid full `cargo clean` unless release artifacts are also intended to be removed.
- Never use `git clean -fdx` as cache management, delete credentials/configuration, or stop unrelated services for cleanup. A dirty checkout is normal; preserve its source work.

Report the binary/profile and instance actually tested, along with any cold-build cost from cleanup. Do not claim external-provider parity from a successful mock test.
