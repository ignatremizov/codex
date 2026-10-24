use super::*;
use crate::app::THREAD_EVENT_CHANNEL_CAPACITY;
use crate::app::Turn;
use crate::app::TurnStatus;
use crate::test_support::PathBufExt;
use crate::test_support::test_path_buf;
use codex_app_server_protocol::AgentMessageDeltaNotification;
use codex_app_server_protocol::CommandExecutionOutputDeltaNotification;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::RequestId as AppServerRequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::TerminalInteractionNotification;
use codex_app_server_protocol::TerminalWait;
use codex_app_server_protocol::TerminalWaitMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::TurnDiffUpdatedNotification;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartedNotification;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

fn turn_started_notification(thread_id: ThreadId, turn_id: &str) -> ServerNotification {
    ServerNotification::TurnStarted(TurnStartedNotification {
        thread_id: thread_id.to_string(),
        turn: Turn {
            id: turn_id.to_string(),
            root_turn_id: None,
            items_view: TurnItemsView::Full,
            items: Vec::new(),
            status: TurnStatus::InProgress,
            error: None,
            started_at: Some(0),
            completed_at: None,
            duration_ms: None,
        },
        agent_queue: None,
    })
}

fn exec_approval_request(
    thread_id: ThreadId,
    turn_id: &str,
    item_id: &str,
    approval_id: Option<&str>,
) -> ServerRequest {
    ServerRequest::CommandExecutionRequestApproval {
        request_id: AppServerRequestId::Integer(1),
        params: CommandExecutionRequestApprovalParams {
            kind: Default::default(),
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            item_id: item_id.to_string(),
            started_at_ms: Some(0),
            expires_at_ms: None,
            approval_id: approval_id.map(str::to_string),
            environment_id: None,
            reason: Some("needs approval".to_string()),
            network_approval_context: None,
            command: Some("echo hello".to_string()),
            cwd: Some(test_path_buf("/tmp/project").abs().into()),
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
        },
    }
}

fn snapshot_events(store: &ThreadEventStore) -> Vec<serde_json::Value> {
    store
        .snapshot()
        .events
        .into_iter()
        .map(|event| match event {
            ThreadBufferedEvent::Notification(notification) => {
                serde_json::to_value(notification).expect("notification should serialize")
            }
            ThreadBufferedEvent::Request(request) => {
                serde_json::to_value(request).expect("request should serialize")
            }
            other => panic!("unexpected buffered event: {other:?}"),
        })
        .collect()
}

fn command_output(thread_id: ThreadId, item_id: &str, delta: String) -> ServerNotification {
    ServerNotification::CommandExecutionOutputDelta(CommandExecutionOutputDeltaNotification {
        thread_id: thread_id.to_string(),
        turn_id: "turn".to_string(),
        item_id: item_id.to_string(),
        delta,
    })
}

#[test]
fn thread_event_store_coalesces_only_adjacent_matching_agent_message_deltas() {
    let thread_id = ThreadId::new();
    let mut store = ThreadEventStore::new(/*capacity*/ 6);
    let delta = |turn_id: &str, item_id: &str, text: &str| {
        ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            item_id: item_id.to_string(),
            delta: text.to_string(),
        })
    };
    let approval =
        exec_approval_request(thread_id, "turn-2", "approval-1", /*approval_id*/ None);
    let other_thread_delta = ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
        thread_id: ThreadId::new().to_string(),
        turn_id: "turn-2".to_string(),
        item_id: "item-2".to_string(),
        delta: "other thread".to_string(),
    });
    store.push_notification(delta("turn-1", "item-1", "hello"));
    store.push_notification_ref(&delta("turn-1", "item-1", " world"));
    store.push_notification(delta("turn-1", "item-2", "another"));
    store.push_notification(delta("turn-2", "item-2", "next"));
    store.push_notification(other_thread_delta.clone());
    store.push_request(approval.clone());
    store.push_notification(delta("turn-2", "item-2", "after approval"));

    assert_eq!(
        snapshot_events(&store),
        vec![
            serde_json::to_value(delta("turn-1", "item-1", "hello world"))
                .expect("delta should serialize"),
            serde_json::to_value(delta("turn-1", "item-2", "another"))
                .expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "next"))
                .expect("delta should serialize"),
            serde_json::to_value(other_thread_delta).expect("delta should serialize"),
            serde_json::to_value(approval).expect("request should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "after approval"))
                .expect("delta should serialize"),
        ]
    );
    assert!(store.has_pending_thread_approvals());

    let remaining = MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES - "after approval".len();
    let exact_limit = "é".repeat(remaining / "é".len());
    let full_chunk = "é".repeat(MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES / "é".len());
    store.push_notification(delta("turn-2", "item-2", &exact_limit));
    store.push_notification(delta("turn-2", "item-2", "🙂"));
    store.push_notification(delta("turn-2", "item-2", &full_chunk));
    store.push_notification(delta("turn-2", "item-2", "🙂"));
    store.push_notification(delta("turn-2", "item-2", &full_chunk));
    store.push_notification(delta("turn-2", "item-2", "🙂"));

    assert_eq!(
        snapshot_events(&store),
        vec![
            serde_json::to_value(delta(
                "turn-2",
                "item-2",
                &format!("after approval{exact_limit}"),
            ))
            .expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "🙂")).expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", &full_chunk))
                .expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "🙂")).expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", &full_chunk))
                .expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "🙂")).expect("delta should serialize"),
        ]
    );
    assert!(!store.has_pending_thread_approvals());

    let mut store = ThreadEventStore::new(/*capacity*/ THREAD_EVENT_CHANNEL_CAPACITY);
    store.push_request(exec_approval_request(
        thread_id,
        "turn-budget",
        "approval-budget",
        /*approval_id*/ None,
    ));
    for chunk in 0..65 {
        let fragment = format!("{chunk:08}");
        let notification = delta("turn-budget", "item-budget", &fragment);
        for _ in 0..MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES / fragment.len() {
            store.push_notification_ref(&notification);
        }
    }

    assert_eq!(
        snapshot_events(&store),
        (1..65)
            .map(|chunk| {
                serde_json::to_value(delta(
                    "turn-budget",
                    "item-budget",
                    &format!("{chunk:08}").repeat(MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES / 8),
                ))
                .expect("delta should serialize")
            })
            .collect::<Vec<_>>()
    );
    assert!(!store.has_pending_thread_approvals());

    let mut store = ThreadEventStore::new(/*capacity*/ 4);
    store.push_notification(turn_started_notification(thread_id, "turn-oversized"));
    store.push_request(exec_approval_request(
        thread_id,
        "turn-oversized",
        "approval-oversized",
        /*approval_id*/ None,
    ));
    store.push_notification(delta("turn-oversized", "item-oversized", "retained"));
    let expected_events = snapshot_events(&store);
    let oversized = "🙂".repeat(MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES / "🙂".len() + 1);

    store.push_notification(delta("turn-oversized", "item-oversized", &oversized));
    store.push_notification_ref(&delta("turn-oversized", "item-oversized", &oversized));

    assert_eq!(snapshot_events(&store), expected_events);
    assert!(store.has_pending_thread_approvals());
    assert_eq!(store.buffered_agent_message_delta_bytes, "retained".len());

    store.push_notification(delta("turn-oversized", "item-oversized", " text"));
    assert_eq!(
        snapshot_events(&store),
        vec![
            serde_json::to_value(turn_started_notification(thread_id, "turn-oversized"))
                .expect("turn notification should serialize"),
            serde_json::to_value(exec_approval_request(
                thread_id,
                "turn-oversized",
                "approval-oversized",
                /*approval_id*/ None,
            ))
            .expect("approval should serialize"),
            serde_json::to_value(delta("turn-oversized", "item-oversized", "retained text"))
                .expect("delta should serialize"),
        ]
    );
    assert!(store.has_pending_thread_approvals());
}

#[test]
fn cumulative_diff_updates_preserve_useful_replay_events() {
    let thread_id = ThreadId::new();
    let mut store = ThreadEventStore::new(/*capacity*/ 2);
    store.push_notification(turn_started_notification(thread_id, "turn"));
    store.push_request(exec_approval_request(
        thread_id, "turn", "approval", /*approval_id*/ None,
    ));
    let expected = snapshot_events(&store);
    let expected_bytes = store.buffered_notification_bytes;
    let notification = ServerNotification::TurnDiffUpdated(TurnDiffUpdatedNotification {
        thread_id: thread_id.to_string(),
        turn_id: "turn".to_string(),
        diff: "diff --git a/large.rs b/large.rs\n".repeat(32 * 1024),
    });

    for _ in 0..8 {
        store.push_notification_ref(&notification);
        store.push_notification(notification.clone());
    }

    assert_eq!(snapshot_events(&store), expected);
    assert_eq!(store.buffered_notification_bytes, expected_bytes);
    assert!(store.has_pending_thread_approvals());
    assert_eq!(store.active_turn_id(), Some("turn"));
}

#[test]
fn notification_byte_budget_preserves_requests_and_active_waits() {
    let thread_id = ThreadId::new();
    let mut store = ThreadEventStore::new(/*capacity*/ THREAD_EVENT_CHANNEL_CAPACITY);
    store.push_notification(turn_started_notification(thread_id, "turn"));
    let approval = exec_approval_request(thread_id, "turn", "approval", /*approval_id*/ None);
    store.push_request(approval.clone());
    let wait = ServerNotification::TerminalInteraction(TerminalInteractionNotification {
        thread_id: thread_id.to_string(),
        turn_id: "turn".to_string(),
        item_id: "command".to_string(),
        process_id: "process".to_string(),
        stdin: String::new(),
        deadline_at_ms: None,
        wait: Some(TerminalWait::Started {
            interaction_id: "wait".to_string(),
            started_at_ms: 0,
            mode: TerminalWaitMode::UntilExit,
        }),
    });
    store.push_notification_ref(&wait);
    store.turn_history_complete = true;
    let payload = "x".repeat(MAX_BUFFERED_NOTIFICATION_BYTES / 3);
    let middle = command_output(thread_id, "middle", payload.clone());
    let newest = command_output(thread_id, "newest", payload.clone());

    store.push_notification(command_output(thread_id, "oldest", payload));
    store.push_notification_ref(&middle);
    store.push_notification_ref(&newest);

    assert_eq!(
        snapshot_events(&store),
        vec![
            serde_json::to_value(approval).expect("approval should serialize"),
            serde_json::to_value(&middle).expect("command output should serialize"),
            serde_json::to_value(&newest).expect("command output should serialize"),
            serde_json::to_value(wait).expect("wait should serialize"),
        ]
    );
    assert_eq!(
        store.buffered_notification_bytes,
        serde_json::to_vec(&middle).expect("serialize output").len()
            + serde_json::to_vec(&newest).expect("serialize output").len()
    );
    assert!(store.buffered_notification_bytes <= MAX_BUFFERED_NOTIFICATION_BYTES);
    assert!(store.has_pending_thread_approvals());
    assert_eq!(store.active_turn_id(), Some("turn"));
    assert!(!store.turn_history_complete);
}

#[test]
fn oversized_completed_item_preserves_the_replay_window_and_invalidates_cached_history() {
    let thread_id = ThreadId::new();
    let mut store = ThreadEventStore::new(/*capacity*/ 4);
    store.push_notification(turn_started_notification(thread_id, "turn"));
    store.push_request(exec_approval_request(
        thread_id, "turn", "approval", /*approval_id*/ None,
    ));
    let expected = snapshot_events(&store);
    let expected_bytes = store.buffered_notification_bytes;
    store.turn_history_complete = true;
    let oversized = ServerNotification::ItemCompleted(ItemCompletedNotification {
        thread_id: thread_id.to_string(),
        turn_id: "turn".to_string(),
        completed_at_ms: 0,
        item: ThreadItem::Reasoning {
            id: "reasoning".to_string(),
            summary: vec!["x".repeat(MAX_BUFFERED_NOTIFICATION_BYTES)],
            content: Vec::new(),
        },
    });

    store.push_notification_ref(&oversized);
    store.push_notification(oversized);

    assert_eq!(snapshot_events(&store), expected);
    assert_eq!(store.buffered_notification_bytes, expected_bytes);
    assert!(store.has_pending_thread_approvals());
    assert!(!store.turn_history_complete);
}

#[test]
fn notification_accounting_tracks_escaped_coalesced_text_and_count_eviction() {
    let thread_id = ThreadId::new();
    let mut store = ThreadEventStore::new(/*capacity*/ 1);
    let delta = |text: &str| {
        ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: "turn".to_string(),
            item_id: "message".to_string(),
            delta: text.to_string(),
        })
    };
    store.push_notification(delta("é\n"));
    store.push_notification_ref(&delta("\"🙂"));
    let merged = delta("é\n\"🙂");
    assert_eq!(
        snapshot_events(&store),
        vec![serde_json::to_value(&merged).expect("delta should serialize")]
    );
    assert_eq!(
        store.buffered_notification_bytes,
        serde_json::to_vec(&merged).expect("serialize delta").len()
    );

    let output = command_output(thread_id, "command", "remaining".to_string());
    store.push_notification_ref(&output);
    assert_eq!(
        (
            store.buffered_agent_message_delta_bytes,
            store.buffered_notification_bytes
        ),
        (
            0,
            serde_json::to_vec(&output).expect("serialize output").len()
        )
    );
    store.rebase_buffer_after_session_refresh();
    assert_eq!(
        (
            snapshot_events(&store),
            store.buffered_agent_message_delta_bytes,
            store.buffered_notification_bytes
        ),
        (Vec::<serde_json::Value>::new(), 0, 0)
    );
}

#[test]
fn session_refresh_recounts_retained_questions_and_releases_hydrated_text() {
    let thread_id = ThreadId::new();
    let mut store = ThreadEventStore::new(/*capacity*/ 4);
    store.push_notification(command_output(
        thread_id,
        "command",
        "old output".to_string(),
    ));
    let mut completed = ItemCompletedNotification {
        thread_id: thread_id.to_string(),
        turn_id: "turn".to_string(),
        completed_at_ms: 0,
        item: ThreadItem::AgentMessage {
            id: "message".to_string(),
            text: "x".repeat(1024 * 1024),
            phase: None,
            inter_agent_source: None,
            memory_citation: None,
            attribution: None,
            input: None,
            delivery: None,
            questions: Some(Vec::new()),
        },
    };
    store.push_notification(ServerNotification::ItemCompleted(completed.clone()));

    store.rebase_buffer_after_session_refresh();

    let ThreadItem::AgentMessage { text, .. } = &mut completed.item else {
        panic!("expected agent message");
    };
    text.clear();
    let expected = ServerNotification::ItemCompleted(completed);
    assert_eq!(
        snapshot_events(&store),
        vec![serde_json::to_value(&expected).expect("item should serialize")]
    );
    assert_eq!(
        store.buffered_notification_bytes,
        serde_json::to_vec(&expected).expect("serialize item").len()
    );
    let Some(ThreadBufferedEvent::Notification(notification)) = store.buffer.front() else {
        panic!("expected retained notification");
    };
    let ServerNotification::ItemCompleted(ItemCompletedNotification {
        item: ThreadItem::AgentMessage { text, .. },
        ..
    }) = notification.as_ref()
    else {
        panic!("expected retained questions");
    };
    assert_eq!(text.capacity(), 0);
}
