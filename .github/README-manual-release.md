# Manual fork releases

`manual-verify` remains the executable verification workflow. `manual-release-build`
produces the complete release with the existing Cargo `release` profile unchanged:
ThinLTO, four codegen units, and line-table debug information. It uses only standard
public GitHub runners. Do not compile this release on the developer's laptop.

The release inputs select Linux and/or Intel macOS. `reuse_successful_components`
defaults to true. A repeated dispatch of the **same commit** can reuse successful
macOS components even when another job made the previous overall run fail. This
does not reuse old binaries across commits: executables and native voice resources
are stamped with the source commit. Missing or expired artifacts are rebuilt.
Set the reuse input to false to rebuild every selected component.

## Compilation and caching

The macOS application job builds CLI, app-server, and responses-proxy libraries
together, then builds their executables with one Cargo job to avoid overlapping
final code-generation, ThinLTO, and linker memory peaks. Both phases select the
same packages, target, features, and release profile. The code-mode host, including
its V8 runtime, has a separate job. Native voice preparation remains separate.
Packaging requires every selected component to pass; a partial component is not
published as a complete release.

The shared sccache action uses GitHub Actions v2 runtime credentials and explicitly
exports `ACTIONS_CACHE_SERVICE_V2` through a JavaScript action. Both the endpoint
and protocol selection are required. Before building the workspace, it compiles a
tiny CI-only probe twice across a cache-daemon restart and requires a cache hit
with no write errors. It refuses silent fallback to an unpersisted local cache.
Runtime credentials are masked. The pinned cache daemon has idle shutdown
disabled, and warnings go to the diagnostics artifact. Source-download caches no
longer restore `~/.cargo/bin` over freshly installed rustup or build tools.

sccache reuses library compilation, not final executable links. The first build
can still be cold. Later runs must show actual cache hits; installing sccache or
seeing a Cargo source-cache hit does not establish compiled-artifact reuse.

## Evidence and recovery

`run-release-build.py` preserves build output, samples memory, swap, processes,
disk capacity, and sccache statistics every two minutes, and saves each completed
phase's Cargo timings. `/usr/bin/time` also records peak resource usage. Diagnostic
uploads run after failures, so a missing final Cargo HTML report no longer loses
all evidence. A new dispatch does not automatically cancel a running release.

Application artifacts are gated by `smoke-auth-file.py`, run only on CI-built
executables in a disposable home directory with fake credentials. It verifies
selected-file login, status, logout, absolute paths, invalid selectors, and the
app-server's public `account/read` behavior. An explicit missing file must not
fall back to the default profile. No real user credential is needed.

Each macOS binary component includes a manifest binding its source commit,
repository, target, version, release profile, toolchain file, lockfile, and binary
SHA-256 digests. Reused components are verified before execution, re-smoke-tested,
and verified again by the packaging job. The combined raw archive includes these
manifests and the credential-selector smoke output.

After a failed job, inspect the diagnostics before retrying. A retry can reuse
compiled libraries and successful same-commit components, but it cannot resume a
half-finished individual compiler invocation. Cross-commit component reuse and a
change-aware dependency planner require a separate provenance design; they are
deliberately not approximated with top-level directory filters.
