# Deleting a conversation and its saved side chats

The TUI's explicit `/delete` action includes its saved side chats. This implements the user-selected 0.169 integration policy; it does not expand deletion to arbitrary spawned agents, unrelated forks, or independent conversations.

## Discovery and authority

For local paginated history, the client first discovers the complete saved-side-chat graph, including archived children and nested saved side chats. Queries are not limited to the current account or provider: conversations share the same local home. Visibility does not change execution credentials or bypass server authorization.

Every candidate is checked through its own current `thread/read` metadata: its exact ID, persisted status, attached parent, fork origin, and `tui_side_chat` feature source must match. A cached tab, a list entry, an old first rollout header, or a generic spawn relationship is not enough. Cycles, conflicting parents, repeated pagination cursors, missing metadata, unsupported discovery, or a failed page abort before any delete request. Browsing may still treat an older store as having no discoverable sidebar; destructive discovery does not.

Each child is rechecked immediately before its delete request, and the selected parent's identity and attachment are checked again before its own request. Children are deleted before their parents. The backend's existing single-thread deletion, live-writer, attached-child, rollout-reference, and shared-evidence guards remain authoritative. A new attachment or a surviving reference can stop deletion rather than authorize collateral cleanup.

Backends without local saved-sidebar support retain their guarded single-thread delete path; the client does not invent a traversal from ordinary ancestry. For a local paginated backend, an unsupported attached-thread filter fails closed rather than falling back to a guessed empty cascade.

## Failure and retry

This sequence is not a multi-thread transaction. A successful child reply confirms that child's deletion. A later read or delete failure does not undo earlier successes, and a failed mutation response is not proof that the last request had no effect.

Errors report the exact confirmed child IDs and distinguish a parent deletion that was never requested from a parent deletion whose result was not confirmed. The client stops immediately; it does not automatically resend an uncertain mutation. Refresh the current conversation state before retrying.

Closing a tab, archiving, detaching, unsubscribing, and unloading are different operations and do not trigger this history-deletion cascade. Related histories and shared evidence outside the verified deletion set remain protected by the existing store checks.

## Coverage

`app_server_session/side_chat_delete_tests.rs` exercises the actual session facade over JSON-RPC. It covers complete discovery before the first mutation, nested and archived child order, cross-profile query fields, duplicate pages, cycles, unsupported and failed discovery, missing or unrelated metadata, changed attachment identities, server writer/reference rejection, and accurate partial-result presentation. Existing server/store deletion tests continue to own deletion-lock and shared-evidence guarantees. Tests and rendered snapshots require remote qualification; local formatting is not test execution.
