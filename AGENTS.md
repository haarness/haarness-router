# Repository skills

Before building, running, testing, packaging, or cleaning build artifacts for Haarness Router or its embedded Web UI, read and follow [.skills/haarness-router-build-run/SKILL.md](.skills/haarness-router-build-run/SKILL.md).

Use the skill's fast local development path unless the task calls for release/performance validation. Keep incremental caches warm; do not add automatic cleanup to every build. Preserve existing source changes, credentials, configuration and unrelated running services.

The documentation index is [docs/README.md](docs/README.md). When behaviour, configuration, flags or endpoints change, update the matching page there and the README in the same change.

## Porting from other codebases

When a change is ported or adapted from another codebase, write it in this project's own words. Do not carry over the other codebase's name, product or feature names, command and flag names, UI text, prompt text, test fixtures and test assets, file names, issue or pull request numbers, commit hashes, or internal hostnames. The same applies to code comments, docs, records, commit messages and branch names: describe what changed and why, never where it came from. Commit as `Orex <orex@haarness.ai>`, set with `git config user.name Orex` and `git config user.email orex@haarness.ai` in every new clone, because the machine-wide identity may differ. Never credit an AI assistant: no `Co-Authored-By` trailer for Claude or any other model, no session links and no "Generated with" lines, in commit messages and pull request descriptions alike, even when a tool suggests them. `scripts/check-commercial-boundary.sh` must pass before every commit, and `scripts/check-commercial-boundary.sh --commits origin/main..HEAD` before every push.
