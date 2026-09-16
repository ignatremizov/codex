# codex-app-server-daemon

> `codex-app-server-daemon` is experimental and its lifecycle contract may
> change while the remote-management flow is still being developed.

`codex-app-server-daemon` backs the machine-readable `codex app-server`
lifecycle commands used by remote clients such as the desktop and mobile apps.
It is intended for Codex instances launched over SSH, including fresh developer
machines that should expose app-server with `remote_control` enabled.

## Platform support

The current daemon implementation is Unix-only. It uses pidfile-backed
daemonization plus Unix process and file-locking primitives, and does not yet
support Windows lifecycle management.

## Commands

```sh
codex app-server daemon start
codex app-server daemon restart
codex app-server daemon enable-remote-control
codex app-server daemon disable-remote-control
codex app-server daemon stop
codex app-server daemon version
codex app-server daemon bootstrap --remote-control
```

On success, every command writes exactly one JSON object to stdout. Consumers
should parse that JSON rather than relying on human-readable text. Lifecycle
responses report the resolved backend, socket path, local CLI version, and
running app-server version when applicable.

## Bootstrap flow

For a new remote machine:

```sh
curl -fsSL https://chatgpt.com/codex/install.sh | sh
$HOME/.codex/packages/standalone/current/codex app-server daemon bootstrap --remote-control
```

`bootstrap` requires the standalone managed install. It records the daemon
settings under `CODEX_HOME/app-server-daemon/<profileOpaqueId>/`, starts app-server as a
pidfile-backed detached process, and launches a detached updater loop.

## Auth-profile scope

Each explicitly started server uses one captured credential selection.
`CODEX_AUTH_FILE=auth-office.json codex app-server daemon start`, for example,
starts the office profile while keeping the same `CODEX_HOME` for shared data.
Use the same selection for subsequent lifecycle and remote-control commands.
Default credential-storage policy participates in the profile identity; default
keyring storage does not match an explicitly selected file.

PID records, updater PID records, settings, lifecycle locks, and control sockets
are profile-scoped. Literal `--listen unix://` is resolved after startup config
loads; an explicit `--listen unix://PATH` remains exactly that endpoint.
Child servers and updater restarts retain the captured home, auth-file selection,
and backend policy. They do not reselect credentials from a later environment.

Executable installation remains shared under `CODEX_HOME/packages/standalone`,
including the installer's shared mutation lock. Thread ownership locks remain
scoped by home and thread ID, not auth profile.

Clients only reuse an existing, matching local server after checking home and
auth-profile metadata on the actual connection. Client startup does not start a
shared server automatically. A legacy server without profile metadata is not
eligible for automatic reuse; start a current profile-scoped server explicitly.
Legacy home-wide daemon PID files are not adopted by profile-scoped lifecycle
commands.

Stop, version, disable-remote-control, pairing, and default proxy lookup do not
initialize cloud authentication or fetch cloud configuration. They use local
configuration and inspect only the selected file's bounded set of backend
identities. A verified process-start fingerprint permits stopping a daemon even
when its socket is unresponsive; socket-only discovery requires matching
connection metadata. More than one live backend is an error unless
`-c cli_auth_credentials_store="keyring"` (or the intended other backend) selects
the captured runtime backend explicitly. These lookups never start a server or
search another auth-file selection.

The hidden updater receives its captured backend explicitly from bootstrap.
Optional local HTTP configuration is best-effort and cannot prevent resolving
that identity. Starting or explicitly restarting a daemon still loads effective
cloud configuration normally.

## Installation and update cases

The daemon assumes Codex is installed through `install.sh` and always launches
the standalone managed binary under `CODEX_HOME`.

| Situation | What starts | Does this daemon fetch new binaries? | Does a running app-server eventually move to a newer binary on its own? |
| --- | --- | --- | --- |
| `install.sh` has run, but only `start` is used | `start` uses `CODEX_HOME/packages/standalone/current/codex` | No | No. The managed path is used when starting or restarting, but no updater is installed. |
| `install.sh` has run, then `bootstrap` is used | The pidfile backend uses `CODEX_HOME/packages/standalone/current/codex` | Yes. Bootstrap launches a detached updater loop that runs `install.sh` hourly. | Yes, while that updater process is alive and app-server is already running. After a successful fetch, the updater restarts app-server with the refreshed binary and only then replaces its own process image. |
| Some other tool updates the managed binary path | The next fresh start or restart uses the updated file at that path | Only if `bootstrap` is active, because the updater still runs `install.sh` on its normal cadence. | Without `bootstrap`, no. With `bootstrap`, the next successful updater pass compares the managed binary contents after `install.sh` runs; if app-server is running and they differ from the updater's current image, it refreshes app-server first and then itself. |

### Standalone installs

For installs created by `install.sh`:

- lifecycle commands always use the standalone managed binary path
- `bootstrap` is supported
- `bootstrap` starts a detached pid-backed updater loop that fetches via
  `install.sh`
- after a successful refresh, if app-server is running and the managed binary
  contents changed, the updater restarts app-server with that binary first and
  only then replaces its own process image
- the updater loop is not reboot-persistent; it must be started again by
  rerunning `bootstrap` after a reboot

### Out-of-band updates

This daemon does not watch arbitrary executable files for replacement. If some
other tool updates the managed binary path:

- without `bootstrap`, a currently running app-server remains on the old
  executable image until an explicit `restart`
- with `bootstrap`, the detached updater loop notices the changed managed
  binary on its next successful scheduled pass after running `install.sh`; if
  app-server is running, it refreshes app-server first and then refreshes itself
  once that replacement starts successfully

## Lifecycle semantics

`start` is idempotent and returns after app-server is ready to answer the normal
JSON-RPC initialize handshake on the Unix control socket.

`restart` stops any managed daemon and starts it again.

`enable-remote-control` and `disable-remote-control` persist the launch setting
for future starts. If a managed app-server is already running, they restart it
so the new setting takes effect immediately.

Top-level `codex remote-control` bootstraps with `--remote-control` when the
updater loop is not running. Otherwise it enables remote control and starts the
daemon normally.

`stop` sends a graceful termination request first, then sends a second
termination signal after the grace window if the process is still alive.

All mutating lifecycle commands are serialized per home and auth profile, so a concurrent
`start`, `restart`, `enable-remote-control`, `disable-remote-control`, `stop`,
or `bootstrap` does not race another in-flight lifecycle operation for that profile.

## State

The daemon stores its local state under `CODEX_HOME/app-server-daemon/<profileOpaqueId>/`:

- `settings.json` for persisted launch settings
- `app-server.pid` for the app-server process record
- `app-server-updater.pid` for the pid-backed standalone updater loop
- `daemon.lock` for profile-local lifecycle serialization
