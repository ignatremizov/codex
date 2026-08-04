---
name: code-review
description: Run a final code review on a pull request
---

By default, spawn exactly one rigorous `reviewer` subagent. Use `fork_context: false` and
explicitly tell the reviewer not to spawn or delegate to other agents. Give that reviewer the full
paths of the applicable `code-review-*` skills other than this orchestrator so it can apply the
relevant review lenses itself. Do not fan out merely because the change is large or crosses several
areas.

Only fan out when the user explicitly asks for multiple or parallel reviewers. In that case, use
non-overlapping review lenses and spawn `reviewer_check` subagents, which are configured to use the
cheaper `gpt-5.6-luna` model at medium reasoning. Use `fork_context: false` and explicitly prohibit
sub-delegation in every prompt. Do not use the change-size skill unless the user explicitly asks for
a size review.

Validate candidate findings against the code before reporting them. Return every validated issue
from every reviewer; there is no findings limit.
Use raw Markdown to report findings.
Number findings for ease of reference.
Each finding must include a specific file path and line number.

If the GitHub user running the review is the owner of the pull request add a `code-reviewed` label.
Do not leave GitHub comments unless explicitly asked.
