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

Run `scripts/run-manual-ci.sh` to dispatch `manual-verify.yml`, start
`manual-release-build.yml` after workspace Clippy succeeds, and monitor both workflows. The script
uses long polls, retries pre-execution GitHub Actions infrastructure failures, and can attach to
existing runs with `--verify-run` and `--build-run`. Release monitoring reports Linux readiness
independently from the macOS primary, app-server, and packaging jobs, so successful Linux artifacts
remain visible while macOS is still building or has failed.

Manual checks build the pinned `third_party/voice` SDK and prepared link/runtime
trees through `setup-manual-voice`. Cargo probes GStreamer and GLib only in that
SDK; platform libraries such as ALSA use the runner's normal development packages.
Archive shards receive the same prepared libraries and standalone helpers and
restore them under the producer's Cargo target paths, with executable permissions.

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
`manual-write-generated.yml` workflow also retains these tracked SDK changes in
its repository-wide `generated.patch`.

Snapshot regeneration
uses `INSTA_UPDATE=always` and uploads complete `.snap` files (including embedded
multi-width metadata) only after the test step succeeds; `.snap.new` files remain
separate failure diagnostics, never accepted output.
