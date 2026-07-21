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

## Inter-agent transcripts

Incoming V2 messages also appear in the receiving thread's transcript, independently of experimental raw-response notifications. The app-server projects them as `agentMessage` items with `interAgentSource: { author, recipient }`. This field identifies communication content for presentation; it does not grant sender authority, request a wake, acknowledge delivery to a client, or identify a new assistant answer. Ordinary assistant messages use `null`, and older items may omit the field.

Plaintext messages retain their full content. Known `MESSAGE`, `NEW_TASK`, and `FINAL_ANSWER` wrappers are rendered as their message text only when the wrapper identities match the structured message. Unknown or mismatched wrappers remain literal text. Encrypted content, including mixed plaintext/encrypted payloads and the receiving side of encrypted-with-audit delivery, renders an opaque placeholder. The parent's readable audit prompt is a separate surface; it is not substituted into the child's transcript or model input.

Transcript projection preserves canonical item and turn identities across live delivery, cold replay, and paginated history. Historical id-less Legacy messages use deterministic replay identities. New public agent-message injection receives host-owned identity; idle injection records a completed history-only turn without starting inference, while active injection joins the receiving turn. Accepted canonical publication, live installation, and event enqueue remain ordered even if the caller stops waiting. An ambiguous persistence failure requires canonical reload rather than repeating the append.

Resuming an already running thread establishes a per-connection history/delivery boundary; it does not pause unrelated subscribers. The response contains the history through that boundary, followed by subsequent live items. If resume fails or is cancelled, the caller must resume again from canonical history before treating its notification stream as complete: cancellation releases suppression, but does not replay notifications suppressed during the unsuccessful snapshot. This is not a durable per-connection acknowledgement protocol.

The TUI renders communication as source-backed transcript content without ending an in-progress assistant answer or interpreting embedded question/branch directives. CLI transcript output likewise does not replace the assistant's final-answer output with communication content.

## Assignment metadata and logs

`list_agents` exposes each child's latest accepted readable assignment; the root displays
the constant `"Main thread"`. Opaque assignments clear
that text; rejected submissions and completion results do not change it. Assignment metadata
is volatile registry state, not a durable mailbox or a reconstructed rollout summary.
`send_message` remains non-waking and does not retain parent-turn linkage.

Ordinary tool argument logs redact communication payloads based on the resolved runtime,
including custom namespaces. The dedicated communication stream records readable content
and whether ciphertext is present, without printing ciphertext.
