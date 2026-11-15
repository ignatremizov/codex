# Workflow Strategy

The workflows in this directory are split so that pull requests get fast, review-friendly signal while `main` still gets the full cross-platform verification pass.

## Pull Requests

- Required checks run against GitHub's synthetic merge commit, not the pull
  request head alone. This includes changes already on `main` and catches
  conflicts before they reach the branch.
- `bazel.yml` is the main pre-merge verification path for Rust code.
  It runs Bazel `test` and Bazel `clippy` on the supported Bazel targets,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

## Post-Merge On `main`

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path while still validating them after merge:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - Windows ARM64 nextest archives cross-compiled on Windows x64, then replayed on native Windows ARM64 shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel does not replace yet.

## Manual Verify and Build

The full Linux nextest archive job uses the same persistent, round-trip-checked
GitHub Actions v2 sccache backend as workspace Clippy. Compiler results are
stored during the build, not deferred to a local cache-save step after the job
ends. This allows a replacement hosted runner to reuse eligible compilation
work if the original runner is interrupted. The archive now starts after static
checks and both (possibly skipped) opt-in serializer capture jobs, rather than
competing with them for hosted-runner CPU, memory, and concurrent GitHub cache
writes. The archive bounds Cargo build concurrency to two rustc jobs to reduce
peak linking pressure; it can still run for up to six hours. Waiting for a
failed prerequisite does not skip the archive: `always()` retains diagnostic
coverage. If a serializer-capture job fails but the static checks and archive succeed,
all four test shards still run: their explicit `always()` prerequisite check
avoids inheriting an optional capture failure from the archive's transitive
job dependencies. The final result independently requires every requested
capture, so none of those failures is masked. The unfiltered archive,
runtime helpers, and all four test shards remain required. An infrastructure shutdown (exit 143) or a lost runner
heartbeat is not successful compilation or a reason to skip any tests.
Only explicitly filtered or package-scoped diagnostics allow empty individual hash
partitions (`cargo nextest --no-tests=pass`): otherwise a five-test selection can
put all cases in two of the four shards and incorrectly fail the other two as
`no tests to run`. The normal unfiltered archive and four-shard matrix do **not**
enable this flag. A targeted diagnostic is never full release qualification, and
its aggregate selected test count must be reviewed from the JUnit/artifact reports.


Run `scripts/run-manual-ci.sh` to dispatch `manual-verify.yml`, start
`manual-release-build.yml` after workspace Clippy succeeds, and monitor both workflows. The script
uses long polls, retries pre-execution GitHub Actions infrastructure failures, and can attach to
existing runs with `--verify-run` and `--build-run`. Release monitoring reports Linux readiness
independently from the macOS primary, app-server, voice-runtime, and packaging jobs.
Successful Linux artifacts remain visible while macOS is still building or has failed.

The macOS voice runtime and code-mode host build independently. CLI, app-server,
executor, and proxy libraries share an exact-source checkpoint; the CLI and services then
receive separate six-hour final-executable job budgets without weakening the
release profile. Native credential-file checks qualify the assembled application
component before packaging. Validated components upload immediately, so a later
packaging failure does not require recompilation. Packaging-only recovery checks
the original producer revision and component hashes; it never relabels an older
binary as a build of the packaging workflow's revision.

The fork's verification and release entrypoints are manual. Upstream Bazel jobs
and tag-release publication remain repository-gated, including independent
Windows and Clippy jobs that are not downstream of the main Bazel test job. The
upstream-removed `rust-release-prepare.yml` workflow stays removed.

Manual checks build the pinned `third_party/voice` SDK and prepared link/runtime
trees through `setup-manual-voice`. Cargo probes GStreamer and GLib only in that
SDK; platform libraries such as ALSA use the runner's normal development packages.
Archive shards receive the same prepared libraries and standalone helpers and
restore them under the producer's Cargo target paths, with executable permissions.
Package-scoped verification explicitly builds and transports the MCP test servers,
`codex-exec` and `codex-exec-server` alongside the CLI, Code Mode host,
sandbox, and voice helper. A dependency on a crate's library does not build its
standalone test helpers. Regeneration starts the remote executor from the already
built `ci-test` CLI and Code Mode host rather than rebuilding them in `debug`.

The 0.163 package path uses upstream's current shell backend. Manual packaging
does not fetch the retired patched-zsh manifest, and verification does not build
or transport the removed shell escalation wrapper.

Manual primary packages include a same-commit voice helper and the prepared
development runtime. Their package version is the workspace version plus the
build commit. These are private development artifacts, not signed or notarized
public releases: `--voice-runtime-dir` uses the assembler's development receipt
contract, while `--voice-release-dir` still requires a matching release version
and public-release receipt. Bare binary archives do not contain this runtime.

Schema uploads require their generating step to succeed. App-server generation
also updates the Python SDK. Both manual generation workflows upload
`python-sdk-generated-<run_id>` only after app-server generation succeeds; its
root contains `api.py` and `generated/{v2_all.py,notification_registry.py}`.
Restore those files under `sdk/python/src/openai_codex/`. The existing
`app-server-schema-<run_id>` artifact layout is unchanged. The standalone
`manual-write-generated.yml` workflow installs the pinned Python tooling and
retains SDK changes and newly generated schema files in its repository-wide
`generated.patch`.

`regenerate_tui_snapshots` uses `INSTA_UPDATE=always` for every selected package,
but its accepted-output artifact contains only TUI `.snap` files, including
embedded multi-width metadata, and uploads only after the test step succeeds.
Scope that producer to `codex-tui`. For core, app-server, and extension snapshots,
leave the TUI flag off: `INSTA_UPDATE=new` retains proposed changes in workspace-wide
`.snap.new` and hidden `.pending-snap` failure artifacts for review. Inline pending
records require corresponding source edits; they are not standalone snapshot files.
Generation and filtered runs do not replace full verification on the final revision.

`capture_upstream_agent_envelopes=true` also starts an independent fork source
serializer fixture job. It uses the exact integration commit, the existing
production test-only fixture, and persistent compiler caching, retaining the
complete JSON evidence, compile/test log, original source fixture, and a
source-tree/blob/sha256 receipt even when the full archive runner is interrupted.
The fork and pinned-upstream captures are both required when selected. Neither
can replace the unfiltered nextest archive or any of the four test shards.

`capture_upstream_agent_envelopes` adds a separate, opt-in measurement job to
Manual Verify. It checks out upstream code parent
`fa266e73c414c1798f6ada3f85a151044e07f7c4`, verifies the exact source and harness
revisions, and records a test-only overlay before invoking upstream production
factories and serializers. The artifact includes complete fixture values, UTF-8
byte accounting, preparation provenance, the overlay, and its execution log.
The requested measurement must succeed for the ordinary verification result to
pass; the normal static checks and all test shards remain unfiltered. This job
is a serializer baseline, not upstream runtime qualification. It deliberately
reports remaining unmeasured delivery cases and does not claim token savings.
