# Goal authority and compaction

Status: TODO

This investigation was recorded in the fork's 0.153-based series. Its runtime
observations describe that historical baseline, not verified behavior of the
current integration. Recheck the relevant goal, compaction, and fork owners
before using those observations to design or implement a change.

## Problem

Thread goals persist an objective, lifecycle state, accounting, and selected
skills. The runtime examined for this investigation could continue an active
goal across idle turns, restore it after resume, and reconstruct current goal
context at the compaction installation boundary.

The investigation identified an incomplete authority and delivery contract.
In that baseline, durable goal facts did not record whether an `Initial`,
`ObjectiveUpdated`, or `BudgetLimit` steering obligation was still waiting to
reach a model request. Steering role policy, terminal-error behavior, context
size, and end-to-end compaction coverage were also open review questions.

This document records investigation work only. It does not select an
implementation or establish that a proposed policy has been adopted.

## Historical baseline to recheck

The original investigation identified the following properties to preserve.
Verify their current ownership and coverage before evaluating changes:

- Goal facts and selected skills are structured durable state, not recovered
  from rendered prompt text.
- Ordinary user turns do not receive a full goal reminder merely because a
  goal is active.
- Automatic continuation is derived from the idle predicate and is suppressed
  when queued work, an active turn, or Plan mode should take precedence.
- Post-compaction goal context is reconstructed from the current persisted
  active goal while holding a lease through compacted-history installation.
- Goal mutations use revision checks and serialized runtime effects so stale
  updates cannot reactivate replaced or cleared state.
- Ordinary and subagent forks do not inherit goal ownership. Only the explicit
  safety-retry fork path copies the goal and its selected skills. The integrated
  path preserves the source status and accounting and persists a separate
  continuation deferral until the next explicit turn; it does not convert an
  active or budget-limited goal into a paused goal.
- Goal objectives are escaped as user-provided data before model injection.

## Deferred design questions

### Durable cadence

Determine whether goal state should persist pending steering intent separately
from goal facts:

```text
Initial
ObjectiveUpdated
BudgetLimit
```

The design must define:

- When each intent is created.
- Whether and how intents supersede one another.
- What evidence proves that an intent reached final model request input.
- When delivery is acknowledged and the pending intent is cleared.
- Behavior across process failure, request failure, retry, compaction, resume,
  rollback, and fork.
- Whether a goal revision, turn id, request id, or context-window id is the
  appropriate delivery watermark.

Continuation should remain derived from the idle lifecycle rather than becoming
a durable reminder emitted on every subsequent request.

### Steering authority

Evaluate user-role and developer-role delivery without assuming either is the
default.

The review must cover:

- Whether runtime-owned persistence requires developer-role authority.
- How a later ordinary user message can interrupt, pause, clear, or supersede
  developer-role goal steering.
- Whether role is global configuration, per-goal state, or fixed runtime
  policy.
- How role changes behave across resume and existing compacted histories.
- How to keep the raw objective untrusted even when its runtime wrapper uses
  developer role.
- Whether final request shaping should verify exactly one current goal item and
  repair missing, stale, duplicated, or wrong-role items.

Rendered goal markers must not become the source of goal facts, cadence, or
role policy.

### Terminal errors and blocking

Review the historical policy that marked an active goal `blocked` after one
non-usage-limit terminal turn error, and establish whether that policy still
applies in the runtime being changed.

Distinguish:

- A transient provider, compaction, tool, or runtime failure.
- Repeated identical failures that would otherwise create an automatic loop.
- A genuine external dependency or user decision that prevents progress.
- Usage exhaustion and token-budget exhaustion.
- Model-requested `blocked` after the configured repeated-blocker audit.

Any retry or repeated-failure policy must be bounded and durable enough to
avoid both infinite continuation and premature goal termination.

### Context size and cache behavior

Measure the complete rendered continuation item, including a maximal objective.
The historical objective was bounded, but the combined prompt could exceed the
1,000-token manual-review threshold used by the original investigation.
Recheck the current bounds and review guidance rather than treating this
observation as a measurement of the integrated runtime.

Investigate:

- A hard token or byte bound for each injected goal fragment.
- Separating stable policy from dynamic objective and accounting data.
- Avoiding repeated large prompt bodies and unnecessary cache churn.
- Preserving completion and blocked-audit semantics if the prompt is reduced.
- Rejecting configurable limits that make model-visible items unbounded.

The original investigation used a 10,000-token ceiling for an individual
injected goal item. Reconcile that proposed bound with current context policy
before implementation; this document neither establishes enforcement nor
changes explicitly configured available-skills catalog budgets.

### Compaction and reconstruction

Recheck durable-state reconstruction and the installation lease identified in
the historical baseline.
Determine whether reconstruction should always emit a continuation item after
compaction or only repair authority required by pending cadence.

The design must cover local compaction, remote compaction, pre-turn and
mid-turn compaction, repeated compaction, encrypted remote items, rollback, and
cold resume from a compacted rollout.

### Goal-bound skills

Preserve structured goal skill identity independently of rendered goal text.
Verify the historical behavior and ensure any cadence or authority redesign can:

- Promote an explicitly selected skill on the first goal turn.
- Restore selected skills after resume.
- Keep the promoted inventory available after compaction.
- Remove goal-owned skill authority from ordinary and subagent forks.
- Carry the selection only through the explicit safety-retry fork, preserving
  its source status and separate continuation deferral.

## Required investigation

Before implementation:

1. Trace final request-input construction for local, remote, and
   previous-response-id requests.
2. Identify the durable transaction boundary for goal mutation plus pending
   cadence.
3. Define crash points and expected recovery for every cadence kind.
4. Decide role and user-interruption semantics with explicit examples.
5. Measure prompt sizes and cache effects using realistic maximal goals.
6. Compare the proposed state machine against existing app-server, TUI, tool,
   resume, rollback, and fork APIs.
7. Review migration and backward-compatibility behavior for existing goal rows
   and rollout artifacts.

## Testing requirements for a future implementation

Add integration coverage for:

- Initial delivery followed by process interruption before and after request
  submission.
- Objective update during an active turn, while idle, and during compaction.
- Budget-limit transition when injection or request submission fails.
- Local and remote pre-turn and mid-turn compaction.
- Fresh objective exactly once after compaction, with no stale objective.
- Concurrent goal clear or replacement while compaction is installing.
- Cold resume and rollback from compacted histories.
- User-role and developer-role behavior if both remain supported.
- A later user interruption superseding developer-role steering.
- Repeated terminal failures without infinite automatic continuation.
- Goal-selected explicit skill availability before and after compaction.
- Ordinary, deferred, and subagent fork authority boundaries.

## Acceptance criteria for a future implementation

- Goal facts, cadence, rendered context, and delivery evidence have explicit,
  non-overlapping ownership.
- No runtime behavior recovers current goal authority from rendered marker
  text.
- Every pending cadence transition has defined ordering, acknowledgement,
  retry, and recovery behavior.
- Ordinary user turns do not become implicit continuation events.
- Compaction and resume preserve current goal behavior without installing stale
  state.
- User interruption semantics remain unambiguous under the selected steering
  role policy.
- Model-visible goal context is bounded and covered by size-focused tests.
- Goal-selected skills and fork isolation continue to work.
