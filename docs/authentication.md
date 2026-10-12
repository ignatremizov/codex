# Authentication

For information about Codex CLI authentication, see [this documentation](https://developers.openai.com/codex/auth).

## Account display

`/status` shows the connected app-server's account email and subscription plan when available. Account-update notifications omit email, so the TUI clears the old identity and retrieves the current email with a non-refreshing `account/read`. The read runs in the background; stale results after an account change or disconnect are ignored. If the account has no email or the read fails, the plan remains visible without borrowing an email from local credentials or a previous account.
