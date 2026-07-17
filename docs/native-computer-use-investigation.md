# Native computer-use adapter investigation

Status: TODO

## Question

Determine whether this fork needs a first-class computer-use adapter or whether
the existing shell, image, browser, and plugin surfaces already provide the
same useful behavior with less runtime and protocol complexity.

The immediate local use case is X11 automation. `exec_command` can invoke tools
such as `xdotool`, `xprop`, `xwininfo`, ImageMagick, or a purpose-built script,
and `view_image` can return a captured screenshot to the model. That path is
explicit, inspectable, composable, and does not require Codex to own desktop
automation semantics.

This document records investigation work only. It does not select an
implementation.

## Existing baseline

Preserve these properties while evaluating alternatives:

- Shell commands remain the general local execution primitive.
- Desktop automation can use ordinary operating-system tools without adding a
  model-visible schema to every session.
- Screenshots can be inspected through the existing image tool rather than
  embedded in shell output.
- Browser automation can remain a skill or plugin concern when Playwright or
  another browser-specific provider is the better abstraction.
- Remote machines can be reached through normal SSH or Tailscale networking
  without making Codex a remote desktop transport.
- The user can inspect the exact command, script, environment, and artifacts
  used for each action.

## Potential value of a native adapter

A native adapter may still provide value if it establishes a useful contract
that shell composition cannot provide economically:

- One bounded observe/step schema shared across X11, Wayland, macOS, Windows,
  browsers, Android, or hosted providers.
- Direct model-image responses without a separate screenshot path and
  `view_image` call.
- Batched actions, structured element metadata, post-failure observations, and
  consistent recovery receipts.
- Capability advertisement and inheritance for subagents.
- Uniform approval, mutation classification, policy, app-server, rollout, and
  TUI lifecycle events.
- Provider isolation when the desktop or browser runtime is owned by another
  process, client, executor, or environment.

These are possible architectural benefits, not requirements. A generic native
tool is not justified merely because shell commands can be wrapped behind it.

## Risks and costs

- Duplicating `exec_command`, `view_image`, Playwright, plugins, or MCP tools.
- Injecting additional tool schemas and guidance into model context.
- Creating a large cross-platform action vocabulary with inconsistent provider
  behavior.
- Adding protocol and persisted-history compatibility obligations for transient
  UI actions.
- Weakening auditability by replacing exact commands with opaque provider
  operations.
- Expanding approval and sandbox policy into desktop input, screenshots,
  credentials, and private window contents.
- Coupling local X11 automation to remote-control or hosted-runtime designs
  that are unnecessary for this fork.

## Required investigation

Before implementation:

1. Exercise a realistic X11 task using `exec_command`, screenshot capture, and
   `view_image`; record tool calls, latency, context cost, and failure recovery.
2. Compare that trace with a first-class observe/step trace from an existing
   computer-use provider.
3. Separate local desktop automation from browser automation, Android
   automation, and remote-machine transport rather than assuming one adapter
   must own all four.
4. Inspect the bundled `computer-use` plugin and current feature/requirements
   gates before adding a parallel runtime path.
5. Determine whether direct image return materially reduces turns or tokens
   relative to the explicit screenshot path.
6. Identify which structured outputs are actually useful to models and cannot
   be supplied by a small script or skill.
7. Define approval and privacy boundaries for observing the screen and issuing
   keyboard, pointer, clipboard, and application-control actions.
8. Verify behavior across local TUI, exec, app-server, remote executors,
   subagents, resume, fork, and compaction only if those surfaces genuinely need
   the capability.

## Decision criteria

Prefer the existing shell-and-image path unless evidence shows that a native
adapter:

- completes representative tasks more reliably or with materially fewer model
  turns;
- provides cross-provider behavior the fork will actually use;
- preserves or improves command/action auditability;
- has a bounded model-visible contract;
- does not turn Codex into an unnecessary remote-access transport; and
- has clear ownership for approvals, screenshots, provider lifecycle, and
  transient history.

If the only demonstrated use case is local X11 automation, document a small
skill or helper script instead of changing Codex core or protocol surfaces.

## Acceptance criteria for a future implementation

- The implementation addresses a measured limitation of the shell-and-image
  baseline.
- Tool context and image/token costs are measured and bounded.
- Every mutating action has explicit approval and audit semantics.
- Screenshots and structured observations have defined privacy and retention
  behavior.
- Provider failures return enough current state for recovery.
- Existing shell, image, browser, plugin, and MCP paths remain available and
  are not silently duplicated.
- Cross-platform or remote-provider claims are backed by executable coverage,
  not only a generic schema.
