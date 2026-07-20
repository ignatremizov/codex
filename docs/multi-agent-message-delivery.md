# Multi-agent V2 message delivery

`features.multi_agent_v2.message_delivery` controls model-produced `spawn_agent`, `send_message`,
and `followup_task` messages:

- `encrypted_with_audit` (default) requires both opaque `message` and nonempty readable
  `task_message`. The sender's tool-call record retains the full audit text. The recipient's
  model input contains the original ciphertext, never the audit copy.
- `encrypted` accepts only `message` and has no readable assignment copy.
- `plaintext` accepts only `message`, delivered with one contextual agent-message wrapper.

Empty messages are rejected. The combined UTF-8 byte length of `message` and `task_message`
must not exceed 8192 bytes. Validation happens before recipient resolution, restoration, or
spawn; oversized content is rejected rather than truncated.

Trusted direct plaintext calls retain their plaintext provenance even under an encrypted
configuration. They do not become ciphertext and do not require a second audit field.
Resuming a conversation consumes the stored message representation, not the currently
configured delivery mode.

Parent `subAgentActivity` items retain the readable task in `prompt`: plaintext deliveries expose their text, encrypted-with-audit deliveries expose only the supplied audit copy, and fully encrypted deliveries have no readable prompt. Spawn and follow-up activity carries the complete text through live notifications and persisted history; the TUI applies `tui.agent_prompt_preview_lines` only when displaying it. Older activities without this field still load with no prompt. This parent activity metadata does not change the child's encrypted model input.

`list_agents` exposes each child's latest accepted readable assignment; the root displays
the constant `"Main thread"`. Opaque assignments clear
that text; rejected submissions and completion results do not change it. Assignment metadata
is volatile registry state, not a durable mailbox or a reconstructed rollout summary.
`send_message` remains non-waking and does not retain parent-turn linkage.

Ordinary tool argument logs redact communication payloads based on the resolved runtime,
including custom namespaces. The dedicated communication stream records readable content
and whether ciphertext is present, without printing ciphertext.
