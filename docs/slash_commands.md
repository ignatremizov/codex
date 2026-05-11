# Slash commands

For an overview of Codex CLI slash commands, see [this documentation](https://developers.openai.com/codex/cli/slash-commands).

## Permission settings in this fork

`/approvals` is an alias for `/permissions`. Both open the same permission settings
and accept the same inline profile selection, for example `/approvals :read-only`.
Profile availability, managed requirements, reviewer selection, and full-access
confirmation are resolved through the same server-owned discovery and validation
path; the alias does not bypass approvals or change sandbox permissions itself.
`/approve` remains a separate command for retrying a recent auto-review denial.
