# Contributing to Haarness Router

Thanks for helping. This file explains how to get a change merged.

By participating you agree to follow the Haarness
[Code of Conduct](https://github.com/haarness/.github/blob/main/CODE_OF_CONDUCT.md). Security problems go
to [the security policy](https://github.com/haarness/.github/blob/main/SECURITY.md), never to a public issue.

## Before you start

- **Bugs**: open an issue first, unless the fix is small and obvious.
- **Features, new providers and larger changes**: open an issue describing the problem before writing
  code, so we can agree on the approach. The router stays stateless and keeps no billing, accounts or
  prompt storage; changes that add them belong elsewhere.
- Small documentation fixes can go straight to a pull request.

## Development setup

You need a stable Rust toolchain: CI uses 1.94.0 and the workspace declares 1.88 as its minimum. The
[development guide](docs/development.md) covers build profiles, a credential-free mock setup and the
layout, and the [build and run skill](.skills/haarness-router-build-run/SKILL.md) describes the shortest
loop and how to keep the build cache warm.

```bash
export OPENAI_API_KEY='...' ANTHROPIC_API_KEY='...' GEMINI_API_KEY='...'   # any you have
cargo run --bin haarness-router                                            # no file: the keys above
cargo run --bin haarness-router -- --config config.example.toml           # or a configuration file
```

## Checks

Run these before pushing:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo deny check                     # advisories and dependency licences (deny.toml); needs cargo-deny
```

CI runs the first three. See [testing](docs/testing.md) for the full deterministic suite.

Live provider smoke tests are opt-in and capped; see [testing](docs/testing.md). A new provider or
adapter follows [provider development](docs/providers.md) and comes with black-box tests against a
local fake upstream.

## Pull requests

- Keep each pull request to one logical change, with tests for new behaviour and for the bug fixed.
- Write commit messages that say what changed and why.
- Update `README.md` and the pages in `docs/` when behaviour or configuration changes.
- Never commit secrets, provider keys, `.env` files or generated build output.

## Licensing of contributions

Haarness Router is licensed under the [MIT License](LICENSE). Unless you state otherwise, any
contribution you intentionally submit for inclusion is licensed under the same terms.
