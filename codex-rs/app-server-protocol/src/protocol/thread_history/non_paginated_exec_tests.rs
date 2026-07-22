use super::*;
use crate::protocol::thread_history::ThreadHistoryBuilder;
use crate::protocol::thread_history::build_turns_from_rollout_items;
use crate::protocol::v2::CommandAction;
use codex_protocol::ThreadId;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;
use serde_json::json;

fn header(mode: ThreadHistoryMode, cwd: &str) -> RolloutItem {
    RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            history_mode: mode,
            cwd: cwd.into(),
            ..Default::default()
        },
        git: None,
    })
}

fn started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }))
}

fn call(turn_id: &str, name: &str, call_id: &str, args: serde_json::Value) -> RolloutItem {
    let mut item: ResponseItem = serde_json::from_value(json!({
        "type": "function_call", "name": name, "call_id": call_id,
        "arguments": args.to_string(),
    }))
    .expect("function call");
    item.set_turn_id_if_missing(turn_id);
    RolloutItem::ResponseItem(item.into())
}

fn output(turn_id: &str, call_id: &str, text: &str) -> RolloutItem {
    let mut item: ResponseItem = serde_json::from_value(json!({
        "type": "function_call_output", "call_id": call_id, "output": text,
    }))
    .expect("function output");
    item.set_turn_id_if_missing(turn_id);
    RolloutItem::ResponseItem(item.into())
}

fn initial_history() -> Vec<RolloutItem> {
    vec![
        header(ThreadHistoryMode::Legacy, "/tmp"),
        started("A"),
        call("A", "exec_command", "exec-1", json!({"cmd": "run"})),
        output(
            "A",
            "exec-1",
            "Chunk ID: 1\nWall time: 0.5 seconds\nProcess running with session ID 42\nOutput:\nfirst",
        ),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "A".to_string(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        })),
    ]
}

fn running_command() -> ThreadItem {
    ThreadItem::CommandExecution {
        model_context: None,
        id: "exec-1".to_string(),
        plugin_id: None,
        script_path: None,
        command: "run".to_string(),
        cwd: LegacyAppPathString::from_string("/tmp"),
        process_id: Some("42".to_string()),
        source: CommandExecutionSource::UnifiedExecStartup,
        status: CommandExecutionStatus::InProgress,
        command_actions: vec![CommandAction::Unknown {
            command: "run".to_string(),
        }],
        aggregated_output: Some("first".to_string()),
        exit_code: None,
        duration_ms: Some(500),
    }
}

#[test]
fn late_poll_updates_original_turn_and_rollback_restores_its_prior_snapshot() {
    let mut builder = ThreadHistoryBuilder::new();
    for item in initial_history() {
        builder.handle_rollout_item(&item);
    }
    let before = builder.turn_snapshot("A").expect("original turn");
    assert_eq!(before.items, vec![running_command()]);
    builder.handle_rollout_item(&started("B"));
    let start_index = builder.active_turn_start_index();
    builder.handle_rollout_item(&call(
        "B",
        "write_stdin",
        "poll-1",
        json!({"session_id":42}),
    ));
    let changes = builder.handle_rollout_item_with_changes(&output(
        "B",
        "poll-1",
        "Chunk ID: 2\nWall time: 0.5 seconds\nProcess exited with code 0\nOutput:\nsecond",
    ));
    let mut expected = running_command();
    if let ThreadItem::CommandExecution {
        status,
        aggregated_output,
        exit_code,
        duration_ms,
        ..
    } = &mut expected
    {
        *status = CommandExecutionStatus::Completed;
        *aggregated_output = Some("firstsecond".to_string());
        *exit_code = Some(0);
        *duration_ms = Some(1000);
    }
    assert_eq!(changes.changed_items.len(), 1);
    assert_eq!(
        (
            &changes.changed_items[0].turn_id,
            &changes.changed_items[0].item
        ),
        (&"A".to_string(), &expected)
    );
    assert_eq!(builder.active_turn_start_index(), start_index);
    let rollback = RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns: 1,
        materialized_turns: None,
        rollback_start_index: None,
    }));
    let changes = builder.handle_rollout_item_with_changes(&rollback);
    assert_eq!(changes.removed_turn_ids, vec!["B"]);
    assert_eq!(changes.changed_items.len(), 1);
    assert_eq!(changes.changed_items[0].item, running_command());
    assert_eq!(builder.turn_snapshot("A"), Some(before));
    // An output from the removed poll cannot reattach after rollback.
    builder.handle_rollout_item(&output(
        "B",
        "poll-1",
        "Chunk ID: 3\nProcess exited with code 0\nOutput:\nstale",
    ));
    assert_eq!(builder.finish()[0].items, vec![running_command()]);
}

#[test]
fn first_header_controls_mode_and_cwd_without_speculative_backfill() {
    for modes in [
        vec![],
        vec![ThreadHistoryMode::Paginated],
        vec![ThreadHistoryMode::Paginated, ThreadHistoryMode::Legacy],
        vec![ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated],
    ] {
        let mut items = vec![
            started("A"),
            call(
                "A",
                "exec_command",
                "before-header",
                json!({"cmd":"ignored","workdir":"/tmp"}),
            ),
        ];
        for (index, mode) in modes.iter().enumerate() {
            items.push(header(
                *mode,
                if index == 0 {
                    "/tmp"
                } else {
                    "/copied-ancestor"
                },
            ));
        }
        items.push(call("A", "exec_command", "exec-1", json!({"cmd":"run"})));
        items.push(output("A", "exec-1", "Chunk ID: 1\nWall time: 0.5 seconds\nProcess running with session ID 42\nOutput:\nfirst"));
        let turns = build_turns_from_rollout_items(&items);
        let expected = if modes.first() == Some(&ThreadHistoryMode::Legacy) {
            vec![running_command()]
        } else {
            vec![]
        };
        assert_eq!(turns[0].items, expected);
    }
    let mut builder = ThreadHistoryBuilder::new();
    for item in initial_history() {
        builder.handle_rollout_item(&item);
    }
    builder.reset();
    builder.handle_rollout_item(&started("A"));
    builder.handle_rollout_item(&call(
        "A",
        "exec_command",
        "exec-1",
        json!({"cmd":"run","workdir":"/tmp"}),
    ));
    assert!(builder.finish()[0].items.is_empty());

    let RolloutItem::SessionMeta(meta) = header(ThreadHistoryMode::Legacy, "/tmp") else {
        panic!("session header");
    };
    let mut historical_meta = serde_json::to_value(meta).expect("serialize header");
    historical_meta
        .as_object_mut()
        .expect("header object")
        .remove("history_mode");
    let mut items = initial_history();
    items[0] =
        RolloutItem::SessionMeta(serde_json::from_value(historical_meta).expect("old header"));
    assert_eq!(
        build_turns_from_rollout_items(&items)[0].items,
        vec![running_command()]
    );
}

#[test]
fn canonical_completion_keeps_authority_over_late_raw_outputs_and_rollback() {
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        let mut builder = ThreadHistoryBuilder::new();
        let mut items = initial_history();
        items[0] = header(mode, "/tmp");
        for item in items {
            builder.handle_rollout_item(&item);
        }
        let command: CommandExecutionItem = serde_json::from_value(json!({
            "id":"exec-1", "plugin_id":"plugin@example", "script_path":"scripts/run.sh",
            "command":["canonical"], "cwd":"file:///tmp", "parsed_cmd":[],
            "source":"unified_exec_startup", "status":"completed",
            "aggregated_output":"canonical\n", "exit_code":0,
        }))
        .expect("canonical command");
        let expected: ThreadItem = TurnItem::CommandExecution(command.clone()).into();
        builder.handle_rollout_item(&RolloutItem::EventMsg(EventMsg::ItemCompleted(
            ItemCompletedEvent {
                thread_id: ThreadId::new(),
                turn_id: "A".to_string(),
                item: TurnItem::CommandExecution(command),
                started_at_ms: Some(10),
                completed_at_ms: 20,
            },
        )));
        builder.handle_rollout_item(&started("B"));
        builder.handle_rollout_item(&call(
            "B",
            "write_stdin",
            "poll-1",
            json!({"session_id":42}),
        ));
        assert!(
            builder
                .handle_rollout_item_with_changes(&output(
                    "B",
                    "poll-1",
                    "Chunk ID: 2\nProcess exited with code 1\nOutput:\nraw"
                ))
                .changed_items
                .is_empty()
        );
        builder.handle_rollout_item(&RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )));
        assert_eq!(builder.finish()[0].items, vec![expected]);
    }
}

#[test]
fn reused_process_id_does_not_redirect_a_pending_old_poll() {
    let mut items = initial_history();
    items.extend([
        started("B"),
        call("B", "write_stdin", "old-poll", json!({"session_id":42})),
        call("B", "exec_command", "exec-2", json!({"cmd":"new"})),
        output(
            "B",
            "exec-2",
            "Chunk ID: 2\nProcess running with session ID 42\nOutput:\nnew",
        ),
        output(
            "B",
            "old-poll",
            "Chunk ID: 3\nProcess exited with code 0\nOutput:\nold",
        ),
        call("B", "write_stdin", "new-poll", json!({"session_id":42})),
        output(
            "B",
            "new-poll",
            "Chunk ID: 4\nProcess exited with code 0\nOutput:\nlast",
        ),
    ]);
    let turns = build_turns_from_rollout_items(&items);
    let outputs: Vec<_> = turns
        .iter()
        .flat_map(|turn| &turn.items)
        .map(|item| {
            let ThreadItem::CommandExecution {
                id,
                aggregated_output,
                ..
            } = item
            else {
                panic!("command")
            };
            (id.as_str(), aggregated_output.as_deref())
        })
        .collect();
    assert_eq!(
        outputs,
        vec![("exec-1", Some("firstold")), ("exec-2", Some("newlast"))]
    );
}

#[test]
fn foreign_and_relative_workdirs_use_path_uri_presentation() {
    for (base, workdir, expected) in [
        (r"C:\work", "child", r"C:\work\child"),
        ("/tmp", "file:///C:/work", r"C:\work"),
        ("/tmp", "/other", "/other"),
    ] {
        let items = [
            header(ThreadHistoryMode::Legacy, base),
            started("A"),
            call(
                "A",
                "exec_command",
                "exec-1",
                json!({"cmd":"run","workdir":workdir}),
            ),
        ];
        let turns = build_turns_from_rollout_items(&items);
        let ThreadItem::CommandExecution { cwd, .. } = &turns[0].items[0] else {
            panic!("command")
        };
        assert_eq!(cwd, &LegacyAppPathString::from_string(expected));
    }
}

#[test]
fn malformed_output_does_not_fabricate_success_and_compaction_context_is_not_projected() {
    for text in [
        "tool failed",
        "tool failed\nOutput:\nProcess exited with code 0",
        "Chunk ID: 1\nOutput:\n",
        "Chunk ID: 1\nProcess exited with code invalid\nOutput:\n",
    ] {
        let mut items = initial_history();
        items.push(call("A", "write_stdin", "poll-1", json!({"session_id":42})));
        items.push(output("A", "poll-1", text));
        let turns = build_turns_from_rollout_items(&items);
        let ThreadItem::CommandExecution {
            status, exit_code, ..
        } = &turns[0].items[0]
        else {
            panic!("command")
        };
        assert_eq!(
            (status, exit_code),
            (&CommandExecutionStatus::Failed, &None)
        );
    }
    let mut builder = ThreadHistoryBuilder::new();
    builder.handle_rollout_item(&header(ThreadHistoryMode::Legacy, "/tmp"));
    let compacted = serde_json::from_value(json!({
        "message":"summary",
        "replacement_history":[{"type":"function_call","name":"exec_command","call_id":"hidden","arguments":"{\"cmd\":\"hidden\"}"}],
    })).expect("compaction checkpoint");
    builder.handle_rollout_item(&RolloutItem::Compacted(compacted));
    assert!(builder.finish().iter().all(|turn| turn.items.is_empty()));
    for duration in ["NaN", "inf", "-1", "1e300"] {
        let parsed = parse_exec_output(&format!(
            "Chunk ID: 1\nWall time: {duration} seconds\nProcess exited with code 0\nOutput:\n"
        ));
        assert_eq!(parsed.duration_ms, None);
    }
}

#[test]
fn zero_and_overcount_rollback_preserve_existing_indices_and_reset_associations() {
    let mut builder = ThreadHistoryBuilder::new();
    for item in initial_history() {
        builder.handle_rollout_item(&item);
    }
    let before = builder.turn_snapshot("A");
    let changes = builder.handle_rollout_item_with_changes(&RolloutItem::EventMsg(
        EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 0,
            materialized_turns: None,
            rollback_start_index: None,
        }),
    ));
    assert!(changes.changed_items.is_empty());
    assert_eq!(builder.turn_snapshot("A"), before);
    builder.handle_rollout_item(&RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent {
            num_turns: u32::MAX,
            materialized_turns: None,
            rollback_start_index: None,
        },
    )));
    builder.handle_rollout_item(&started("C"));
    builder.handle_rollout_item(&call("C", "write_stdin", "stale", json!({"session_id":42})));
    builder.handle_rollout_item(&output(
        "C",
        "stale",
        "Chunk ID: 9\nProcess exited with code 0\nOutput:\nstale",
    ));
    let turns = builder.finish();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].id, "C");
    assert!(turns[0].items.is_empty());
}

#[test]
fn invalid_calls_and_unmatched_or_nontext_outputs_do_not_change_history() {
    let mut builder = ThreadHistoryBuilder::new();
    for item in initial_history() {
        builder.handle_rollout_item(&item);
    }
    let before = builder.turn_snapshot("A");
    for item in [
        call("A", "exec_command", "invalid", json!({"workdir":"/tmp"})),
        call(
            "A",
            "write_stdin",
            "invalid-poll",
            json!({"session_id":"bad"}),
        ),
        call(
            "A",
            "write_stdin",
            "unknown-poll",
            json!({"session_id":999}),
        ),
        output(
            "A",
            "unknown-poll",
            "Chunk ID: 1\nProcess exited with code 0\nOutput:\nunmatched",
        ),
    ] {
        assert!(
            builder
                .handle_rollout_item_with_changes(&item)
                .changed_items
                .is_empty()
        );
    }
    builder.handle_rollout_item(&call(
        "A",
        "write_stdin",
        "poll-1",
        json!({"session_id":42}),
    ));
    for call_id in [Some("poll-1"), None] {
        let item: ResponseItem = serde_json::from_value(json!({
            "type":"function_call_output", "call_id":call_id, "output":[],
        }))
        .expect("nontext output");
        assert!(
            builder
                .handle_rollout_item_with_changes(&RolloutItem::ResponseItem(item.into()))
                .changed_items
                .is_empty()
        );
    }
    assert_eq!(builder.turn_snapshot("A"), before);
}
