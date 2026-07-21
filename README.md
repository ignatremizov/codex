<p align="center"><strong>Codex CLI</strong> is a coding agent from OpenAI that runs locally on your computer.
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>
</br>
If you want Codex in your code editor (VS Code, Cursor, Windsurf), <a href="https://developers.openai.com/codex/ide">install in your IDE.</a>
</br>If you want the desktop app experience, run <code>codex app</code> or visit <a href="https://chatgpt.com/codex?app-landing-page=true">the Codex App page</a>.
</br>If you are looking for the <em>cloud-based agent</em> from OpenAI, <strong>Codex Web</strong>, go to <a href="https://chatgpt.com/codex">chatgpt.com/codex</a>.</p>

---

## OpenAI Build Week: Auditable Codex Multi-Agent V2

This fork contains **Auditable Codex Multi-Agent V2**, a Developer Tools entry
for OpenAI Build Week 2026. It restores a readable parent-to-child audit trail
for MultiAgent v2 without requiring a backend change.

The work was inspired by the auditability regression reported in
[openai/codex#28058](https://github.com/openai/codex/issues/28058). Upstream
MultiAgent v2 returns an encrypted `spawn_agent` task and forwards the same
ciphertext to the child. This fork adds a configurable plaintext delivery mode
that exposes the parent-generated task in both the parent and child transcripts
and preserves it in local rollout history.

### New work completed during Build Week

- Add configurable `plaintext` and `encrypted` MultiAgent v2 message delivery.
- Display the exact generated task beneath the parent `spawn_agent` activity.
- Materialize the same task as a sender-attributed `AgentMessage` in the child.
- Preserve follow-up and final-answer communication in thread transcripts and
  reconstructed rollout history.
- Allow direct input after switching to a v2 child, so the child can continue
  interactively instead of rejecting the follow-up.
- Retain compatibility with upstream encrypted sessions and shared state.

The recorded A/B uses the same GPT-5.6 model, authentication, working directory,
and prompt. Upstream hides the task and rejects direct child input; the fork
shows the task on both sides and accepts the same follow-up.

### How Codex and GPT-5.6 were used

The feature was built through Codex sessions running GPT-5.6. Codex implemented
the Rust tool-schema, orchestration, protocol, persistence, migration, and TUI
changes. Multi-agent GPT-5.6 reviewers independently examined transcript
identity, database compatibility, rollout reconstruction, sender metadata, and
test coverage; their findings were folded back into the implementation.

Key human decisions included keeping orchestration at the existing local client
boundary, supporting encrypted mode rather than replacing it, preserving
upstream session compatibility, and presenting sender identity without
duplicating payload metadata. Remote CI validated approximately 12,700 tests.

Primary Codex `/feedback` session:
`019f7b80-6e8a-7812-972b-77351da9838d`.

### Install and test

A verified Linux x86_64 package is available from the
[Build Week release workflow](https://github.com/ignatremizov/codex/actions/runs/29858823459)
as `codex-package-x86_64-unknown-linux-gnu-29858823459`. Extract the artifact,
make the Codex executable runnable, and authenticate normally.

Create `agents-v-two.config.toml` in `CODEX_HOME`:

```toml
model = "gpt-5.6-sol"
model_reasoning_effort = "low"

[features]
multi_agent = true

[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 4
default_fork_turns = "none"
message_delivery = "plaintext"
tool_namespace = "ma"
```

Launch the profile:

```shell
codex -p agents-v-two
```

Ask the parent to spawn a child with `fork_turns` set to `"none"` and provide a
short task. The parent should display the generated task. Switch to the child to
see the same sender-attributed input, then enter a direct follow-up and confirm
that the child continues. Set `message_delivery = "encrypted"` to compare the
upstream-compatible path.

The source retains Codex's existing Linux, macOS, and Windows support. General
source-build instructions are available in [Installing & building](./docs/install.md).

## Quickstart

### Installing and running Codex CLI

Run the following on Mac or Linux to install Codex CLI:

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

Run the following on Windows to install Codex CLI:

```shell
powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"
```

Codex CLI can also be installed via the following package managers:

```shell
# Install using npm
npm install -g @openai/codex
```

```shell
# Install using Homebrew
brew install --cask codex
```

Then simply run `codex` to get started.

<details>
<summary>You can also go to the <a href="https://github.com/openai/codex/releases/latest">latest GitHub Release</a> and download the appropriate binary for your platform.</summary>

Each GitHub Release contains many executables, but in practice, you likely want one of these:

- macOS
  - Apple Silicon/arm64: `codex-aarch64-apple-darwin.tar.gz`
  - x86_64 (older Mac hardware): `codex-x86_64-apple-darwin.tar.gz`
- Linux
  - x86_64: `codex-x86_64-unknown-linux-musl.tar.gz`
  - arm64: `codex-aarch64-unknown-linux-musl.tar.gz`

Each archive contains a single entry with the platform baked into the name (e.g., `codex-x86_64-unknown-linux-musl`), so you likely want to rename it to `codex` after extracting it.

</details>

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Business, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
