use super::ThreadHistoryBuilder;
use super::ThreadHistoryChangeSet;
use super::ThreadHistoryItemChange;
use super::ThreadHistoryTurnMetadata;
use crate::protocol::v2::InterAgentMessageSource;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::Turn;
use crate::protocol::v2::TurnItemsView;
use crate::protocol::v2::TurnStatus;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;

fn message(id: Option<&str>, turn_id: Option<&str>, text: &str) -> ResponseItem {
    let mut item = ResponseItem::AgentMessage {
        id: id.map(|id| ResponseItemId::with_suffix("amsg", id)),
        author: "/root".into(),
        recipient: "/root/worker".into(),
        content: vec![AgentMessageInputContent::InputText { text: text.into() }],
        internal_chat_message_metadata_passthrough: None,
    };
    if let Some(turn_id) = turn_id {
        item.set_turn_id_if_missing(turn_id);
    }
    item
}

fn start(turn_id: &str) -> EventMsg {
    EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.into(),
        root_turn_id: Some("root-1".into()),
        started_at: Some(42),
        trace_id: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    })
}

fn transcript(id: &str, text: &str) -> ThreadItem {
    ThreadItem::AgentMessage {
        id: id.into(),
        text: format!("Agent message from `/root`:\n\n{text}"),
        inter_agent_source: Some(InterAgentMessageSource {
            author: "/root".into(),
            recipient: "/root/worker".into(),
        }),
        phase: Some(MessagePhase::Commentary),
        memory_citation: None,
        delivery: None,
        questions: None,
    }
}

fn history_turn(id: &str, items: Vec<ThreadItem>) -> Turn {
    Turn {
        id: id.into(),
        items,
        items_view: TurnItemsView::Full,
        status: TurnStatus::Completed,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

#[test]
fn pre_start_message_merges_with_real_turn_metadata() {
    let mut builder = ThreadHistoryBuilder::new();
    builder.handle_rollout_item(&RolloutItem::ResponseItem(
        message(Some("pre"), Some("turn-1"), "before").into(),
    ));
    let changes = builder.handle_event_with_changes(&start("turn-1"));
    let metadata = ThreadHistoryTurnMetadata {
        turn_id: "turn-1".into(),
        root_turn_id: Some("root-1".into()),
        status: TurnStatus::InProgress,
        error: None,
        started_at: Some(42),
        completed_at: None,
        duration_ms: None,
    };
    assert_eq!(
        builder.active_turn_metadata_snapshot(),
        Some(metadata.clone())
    );
    assert_eq!(
        changes,
        ThreadHistoryChangeSet {
            changed_turns: vec![metadata],
            ..Default::default()
        }
    );
    let mut expected = history_turn("turn-1", vec![transcript("amsg_pre", "before")]);
    expected.status = TurnStatus::InProgress;
    expected.started_at = Some(42);
    assert_eq!(builder.finish(), vec![expected]);
}

#[test]
fn legacy_communication_without_start_is_materialized() {
    let communication = codex_protocol::protocol::InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").expect("valid child"),
        Vec::new(),
        "legacy".into(),
        /*trigger_turn*/ false,
    );
    let mut builder = ThreadHistoryBuilder::new();
    let changes = builder
        .handle_rollout_item_with_changes(&RolloutItem::InterAgentCommunication(communication));
    assert_eq!(
        changes,
        ThreadHistoryChangeSet {
            changed_items: vec![ThreadHistoryItemChange {
                turn_id: "rollout-0".into(),
                item: transcript("amsg_legacy_0", "legacy"),
                started_at_ms: None,
                completed_at_ms: None,
            }],
            changed_turns: vec![ThreadHistoryTurnMetadata {
                turn_id: "rollout-0".into(),
                root_turn_id: None,
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }],
            removed_turn_ids: Vec::new(),
        }
    );
    assert_eq!(
        builder.finish(),
        vec![history_turn(
            "rollout-0",
            vec![transcript("amsg_legacy_0", "legacy")],
        )]
    );
}

#[test]
fn late_message_targets_finished_turn_while_other_turn_is_active() {
    let mut builder = ThreadHistoryBuilder::new();
    builder.handle_event(&start("finished"));
    builder.handle_event(&EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "finished".into(),
        last_agent_message: None,
        error: None,
        started_at: Some(42),
        completed_at: Some(43),
        duration_ms: Some(1000),
        time_to_first_token_ms: None,
    }));
    builder.handle_event(&start("active"));
    let active = builder.active_turn_metadata_snapshot();
    builder.handle_rollout_item(&RolloutItem::ResponseItem(
        message(Some("late"), Some("finished"), "late").into(),
    ));
    assert_eq!(builder.active_turn_metadata_snapshot(), active);
    let mut finished = history_turn("finished", vec![transcript("amsg_late", "late")]);
    finished.started_at = Some(42);
    finished.completed_at = Some(43);
    finished.duration_ms = Some(1000);
    let mut active = history_turn("active", Vec::new());
    active.status = TurnStatus::InProgress;
    active.started_at = Some(42);
    assert_eq!(builder.finish(), vec![finished, active]);
}

#[test]
fn unknown_turn_change_set_contains_metadata_and_item() {
    let mut builder = ThreadHistoryBuilder::new();
    builder.handle_event(&start("active"));
    let active = builder.active_turn_metadata_snapshot();
    let changes = builder.handle_rollout_item_with_changes(&RolloutItem::ResponseItem(
        message(Some("unknown"), Some("turn-unknown"), "message").into(),
    ));
    assert_eq!(builder.active_turn_metadata_snapshot(), active);
    assert_eq!(
        changes,
        ThreadHistoryChangeSet {
            changed_items: vec![ThreadHistoryItemChange {
                turn_id: "turn-unknown".into(),
                item: transcript("amsg_unknown", "message"),
                started_at_ms: None,
                completed_at_ms: None,
            }],
            changed_turns: vec![ThreadHistoryTurnMetadata {
                turn_id: "turn-unknown".into(),
                root_turn_id: None,
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }],
            removed_turn_ids: Vec::new(),
        }
    );
    assert_eq!(
        builder.turn_snapshot("turn-unknown"),
        Some(history_turn(
            "turn-unknown",
            vec![transcript("amsg_unknown", "message")],
        ))
    );
}

#[test]
fn canonical_and_raw_same_id_upsert_one_item() {
    let mut builder = ThreadHistoryBuilder::new();
    let item = message(Some("same"), Some("turn-1"), "message");
    builder.handle_rollout_item(&RolloutItem::ResponseItem(item.clone().into()));
    builder.handle_event(&EventMsg::RawResponseItem(
        codex_protocol::protocol::RawResponseItemEvent { item },
    ));
    assert_eq!(
        builder.finish(),
        vec![history_turn(
            "turn-1",
            vec![transcript("amsg_same", "message")],
        )]
    );
}

#[test]
fn idless_legacy_fallback_is_stable_and_terminal_placeholder_is_not_revived() {
    let mut builders = [ThreadHistoryBuilder::new(), ThreadHistoryBuilder::new()];
    for builder in &mut builders {
        builder.handle_rollout_item(&RolloutItem::ResponseItem(
            message(/*id*/ None, Some("turn-1"), "message").into(),
        ));
    }
    let histories = builders.map(ThreadHistoryBuilder::finish);
    let expected = vec![history_turn(
        "turn-1",
        vec![transcript("amsg_legacy_0", "message")],
    )];
    assert_eq!(histories, [expected.clone(), expected]);

    let mut builder = ThreadHistoryBuilder::new();
    builder.handle_rollout_item(&RolloutItem::ResponseItem(
        message(/*id*/ None, Some("turn-1"), "message").into(),
    ));
    builder.handle_event(&EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn-1".into(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: Some(2),
        duration_ms: None,
        time_to_first_token_ms: None,
    }));
    builder.handle_event(&start("turn-1"));
    let mut completed = history_turn("turn-1", vec![transcript("amsg_legacy_0", "message")]);
    completed.completed_at = Some(2);
    let mut restarted = history_turn("turn-1", Vec::new());
    restarted.status = TurnStatus::InProgress;
    restarted.started_at = Some(42);
    assert_eq!(builder.finish(), vec![completed, restarted]);
}

#[test]
fn interrupted_and_failed_history_turns_are_not_revived_as_placeholders() {
    for (terminal, expected_status) in [
        (
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("turn-1".into()),
                reason: TurnAbortReason::Interrupted,
                started_at: None,
                completed_at: Some(2),
                duration_ms: None,
            }),
            TurnStatus::Interrupted,
        ),
        (
            EventMsg::Error(ErrorEvent {
                message: "terminal failure".into(),
                codex_error_info: None,
                misalignment: None,
            }),
            TurnStatus::Failed,
        ),
    ] {
        let mut builder = ThreadHistoryBuilder::new();
        builder.handle_rollout_item(&RolloutItem::ResponseItem(
            message(Some("terminal"), Some("turn-1"), "retained").into(),
        ));
        builder.handle_event(&terminal);
        let terminal_turn = builder.turn_snapshot("turn-1").expect("terminal history");
        assert_eq!(terminal_turn.status, expected_status);
        builder.handle_event(&start("turn-1"));
        let mut restarted = history_turn("turn-1", Vec::new());
        restarted.status = TurnStatus::InProgress;
        restarted.started_at = Some(42);
        assert_eq!(builder.finish(), vec![terminal_turn, restarted]);
    }
}
