# Authentication

For information about Codex CLI authentication, see [this documentation](https://developers.openai.com/codex/auth).

## Selecting a credential file

`CODEX_AUTH_FILE` selects a credential file independently of `CODEX_HOME`. For example, these commands use the same configuration, skills, and session store, but separate saved credentials:

```sh
CODEX_AUTH_FILE=auth-office.json codex login
CODEX_AUTH_FILE=auth-office.json codex
CODEX_AUTH_FILE=auth-office.json codex logout
```

A relative value must be a single filename within `CODEX_HOME`; an absolute host-local file path is also accepted. Empty values, relative directory paths, directory targets, and credential-file symlinks are rejected.

When the variable is absent, existing credential-store settings apply unchanged. An explicit selection uses that literal file instead of a persistent keyring backend. Login, credential loading, refresh, and logout use the same selected file; a missing or invalid selected file never falls back to another file or a keyring entry. Explicit `auth.json` selection still requests file storage and is therefore distinct from leaving the selector unset.

The selection is captured at runtime startup and retained during credential and configuration reloads. Restart with another selector to change the saved credential store. Externally supplied ephemeral credentials retain their existing precedence, but their process-local store is shared only by runtimes using the same selection; explicitly ephemeral storage still does not persist credentials to disk.

This setting does not select a model provider or override provider authentication configured in `config.toml`. Existing API-key environment precedence, custom provider authentication, and externally managed authentication remain unchanged. The file selection applies consistently to saved credential payloads, including an explicitly requested API-key login.

## Model discovery with a selected file

Runtimes with an explicit `CODEX_AUTH_FILE` do not read, write, or renew the shared `models_cache.json` file. They retain their in-memory model catalog and any authoritative catalog configured for the model provider. Cache-aware online discovery may therefore make more requests: without the disk cache, each `OnlineIfUncached` refresh fetches the catalog when the provider supports remote discovery. Leaving the selector unset preserves existing disk-cache behavior.

To switch accounts, stop the runtime and restart with the desired selector. Changing accounts inside an already-running runtime does not yet invalidate all in-memory model-catalog and ETag state. Offline reads or failed discovery can retain the previous account's catalog, and a discovery request started before the account change can finish afterward. Disabling the shared disk cache does not resolve that separate live-account-switch limitation.
