# Codex CLI

[**Codex CLI Documentation**](https://developers.openai.com/codex/cli)

To fork a session from another local Codex home, use
`codex fork SESSION_ID --source-home CODEX_HOME`. To fork a specific rollout
file, use `codex fork --from-rollout PATH`. The source home and rollout are
read-only; the new fork is written to the active `CODEX_HOME`.
