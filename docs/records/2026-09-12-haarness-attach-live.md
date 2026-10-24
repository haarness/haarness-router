# Haarness attach through the router — live run, 2026-09-12

`scripts/test-haarness-attach.py` with release binaries of Haarness and Haarness Router, against the live
providers of the maintainers' Stackforward credentials. See [Use with Haarness](../haarness.md#verification)
for how to run it.

- Attached streaming, server-side read tools and continuation passed for GPT-6 Astra, all three GPT-5.6
  models, Claude Fable 5.1, Opus 5 and Sonnet 5.
- The real attached TUI restored history, listed current models, completed a live turn, and restored that
  turn after detach and reattach.
- Gemini's tool-schema rejection was repaired (schemas are now sent as `parametersJsonSchema`). The
  Stackforward Google project then returned HTTP 403 `PERMISSION_DENIED`, so no successful live Gemini
  generation was recorded in this run.

Protocol references used: [OpenAI model guidance](https://developers.openai.com/api/docs/guides/latest-model),
[stateless reasoning](https://developers.openai.com/api/docs/guides/reasoning),
[Claude tool selection](https://platform.claude.com/docs/en/claude_api_primer),
[Gemini function declarations](https://ai.google.dev/api/generate-content#FunctionDeclaration) and
[Gemini thought signatures](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures).
