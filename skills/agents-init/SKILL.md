---
name: agents-init
description: Generate multi-agent supervisor prompts and runnable command.sh files that delegate work to multiple Codex agents, including naming, gating, review agent setup, and task decomposition. Use when the user asks to delegate work to N agents, split tasks by checklist or spec sections, or create agent prompts for implementation/plan/review workflows.
---

# Agents Init

Create a runnable multi-agent setup (usually a `command.sh` file) that uses `codex-supervise` (wrapper for `scripts/supervise_agents.py`) to spawn named agents with clear responsibilities, gating, and optional review.

## Workflow

1) Clarify inputs
- Task summary and expected outputs.
- Number of agents, desired roles, and whether a review agent is required.
- Any gating document or status file (e.g., a spec file with a `STATUS:` first line).
- Repo path (`--cwd`) and server cmd if non-default.

2) Decompose work
- Split by responsibility (spec, implementation, docs/UX, review).
- When a checklist exists, map one agent per section or per logical group.
- Keep prompts explicit, one agent per focused deliverable.
- Prefer multi-round refinement: draft prompt files, review, then finalize.
- If agents are likely to touch the same files, plan a git-worktree flow so each agent works in its own worktree.

3) Name agents
- Use `(name: <AgentName>)` inside each prompt so the supervisor can address them.
- Names should be short and role-oriented; examples (`SpecLead`, `CoreImplementer`, `DocsUI`, `Reviewer`) are inspiration only and MUST be customized per task.

4) Add gating when needed
- Prefer `WAIT_FOR_AGENT: <AgentName[,AgentName...]> || <prompt>` to defer an agent until named agents have completed.
- When multiple agents are listed, all of them must be done before the prompt runs.
- Use `WAIT_FOR_STATUS: <path> | <status> || <prompt>` only for external, human-edited gates (optional).

5) Skill invocation (when prompt uses `$<skill-name>`)
- The supervisor will auto-attach a `skill` input item when it sees `$<skill-name>` in a prompt.
- It resolves the skill path from `skills/<skill-name>/SKILL.md` under `--cwd` (if provided) or `~/.codex/skills/<skill-name>/SKILL.md`.
- If the skill file is missing, it will still send the text prompt without the extra skill input item.

6) Include review
- If review is required, add a reviewer agent with a prompt that references expected deliverables.
- Review should be explicit about correctness, missing steps, and fixes.

7) Worktree + commit discipline (when file overlap is likely)
- Use `git worktree add` so each agent has an isolated checkout.
- Include worktree paths in prompts so agents operate in the right directory.
- Require detailed commit messages with bodies (PR-summary style), explaining behavior changes and impact to ease merging.
- Rule of thumb: if the commit message is under ~5 lines it is likely too short; if it exceeds ~30 lines it is likely too long.
- In each agent prompt, instruct the agent to use the `git-commit-style` skill before writing commits.
- Remind downstream agents to `git merge <upstream-branch>` (or `git rebase`) to pick up completed work from other worktrees.

## Output format

Default output is:
- `agents/<AgentName>.md` prompt files for each agent.
- A `delegate.sh` that runs `codex-supervise` (or `scripts/supervise_agents.py` directly) and loads prompts via `$(cat agents/<AgentName>.md)`.
Always include a first instruction in each prompt file: “Read `AGENTS.md` and follow it.”
Use a here-doc to keep large prompts readable.
Always run `chmod +x` on the generated script.

Example:

```bash
#!/usr/bin/env bash
set -euo pipefail

codex-supervise \
  --cwd "/path/to/repo" \
  --review \
  --agent "(name: SpecLead) Write a spec in docs/feature.md. First line must be STATUS: ready-for-review. Include data model, edge cases, and acceptance criteria." \
  --agent "WAIT_FOR_AGENT: SpecLead || (name: CoreImplementer) Implement per docs/feature.md. Update tests and run relevant checks." \
  --agent "WAIT_FOR_AGENT: CoreImplementer || (name: DocsUI) Update docs/config.md with new keys and examples." \
  --agent "WAIT_FOR_AGENT: DocsUI || (name: Reviewer) Review outputs for correctness and missing steps; list concrete fixes."
```

## Prompt pattern

- Start with role and deliverable.
- List files to touch and constraints.
- Mention wait-gate behavior explicitly if a file status is required.
- Keep prompts actionable and scope-limited.

## Supervisor controls (curses UI)

The supervisor provides agent-aware shortcuts and per-agent queues:

- **Status strip:** shown above `cmd>`, e.g. `1.  2!  3+1`.
  - `.` running, `✓` done, `!` approval pending, `+N` queued prompts.
- **Inspect:** type `2` (or agent name) to inspect that agent; `esc` or `b` to return.
- **Send prompt:** `2 <prompt>` queues if running or runs immediately if idle.
- **Stop:** `2 stop <reason>` tries to cancel the current turn, or queues a stop message.
- **Approve:** `2 a` / `2 s` / `2 p` / `2 d` / `2 c` approve the latest request for agent 2.
  - `approve a` (oldest pending) or `approve 2 a` (agent-specific) also supported.
- **Review:** `review <agent|thread> [uncommitted|base <branch>|commit <sha> [title]|custom <instructions>] [--detached|--inline|delivery <mode>]`
- **Threads:** `threads` lists loaded thread ids; `threads list` lists stored threads (optional limit/cursor).
- **Review output files:** completed reviews are written to `~/.codex/supervisor_logs/review-*.md` and the path is logged in the agent history.

## When a manager agent is requested

If the user asks for a manager agent, create a manager prompt that:
- Reads any existing docs/checklists.
- Proposes agent split and names.
- Outputs a complete `command.sh` (or `agents.toml` if requested).
- Calls out gating rules and review requirements.

Include a short note on how to run the generated `command.sh`.

## Notes

- If `/skills` is available in the UI, mention relevant skills inside the manager prompt so it can incorporate them.
- Do not use MCP to read skills; load from `~/.codex/skills/` if you need to inspect them.
- Keep outputs self-contained so users can run them without additional context.
