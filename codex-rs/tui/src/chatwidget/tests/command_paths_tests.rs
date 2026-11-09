use super::*;
use codex_utils_path_uri::LegacyAppPathString;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;

fn execution(chat: &ChatWidget, id: &str, directory: &str) -> AppServerThreadItemPayload {
    AppServerThreadItemPayload::CommandExecution {
        model_context: None,
        sandbox_type: None,
        id: id.into(),
        command: "printf ok".into(),
        cwd: LegacyAppPathString::from_path(&chat.config.cwd.join(directory)),
        process_id: None,
        plugin_id: None,
        script_path: None,
        source: ExecCommandSource::Agent,
        user_shell_response_handling: None,
        status: AppServerCommandExecutionStatus::InProgress,
        command_actions: Vec::new(),
        aggregated_output: None,
        exit_code: None,
        duration_ms: None,
    }
}

#[tokio::test]
async fn live_and_completion_replay_show_only_directory_transitions() {
    let mut rendered = Vec::new();
    for replay in [false, true] {
        let (mut chat, mut events, _operations) =
            make_chatwidget_manual(/*model_override*/ None).await;
        let mut items = Vec::new();
        for (id, directory) in [
            ("a", "service-a"),
            ("b", "service-a"),
            ("c", "service-b"),
            ("d", ""),
        ] {
            let mut item = execution(&chat, id, directory);
            if !replay {
                handle_exec_begin(&mut chat, item.clone());
            }
            if let AppServerThreadItemPayload::CommandExecution {
                status,
                aggregated_output,
                exit_code,
                duration_ms,
                ..
            } = &mut item
            {
                *status = AppServerCommandExecutionStatus::Completed;
                *aggregated_output = Some("ok\n".into());
                *exit_code = Some(0);
                *duration_ms = Some(5);
            }
            if replay {
                chat.replay_thread_item(item.clone(), "turn".into(), ReplayKind::ThreadSnapshot);
            } else {
                handle_exec_end(&mut chat, item.clone());
            }
            items.push(item);
        }
        let cells = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => Some(cell),
                _ => None,
            })
            .collect::<Vec<_>>();
        let lines = cells
            .iter()
            .map(|cell| lines_to_single_string(&cell.display_lines(/*width*/ 80)))
            .collect::<String>();
        let cold = crate::thread_transcript::thread_items_to_transcript_cells(
            /*thread_id*/ None,
            &chat.config.cwd,
            items,
            crate::thread_transcript::RawReasoningVisibility::Hidden,
            /*config*/ None,
        );
        assert_eq!(
            cold.iter()
                .map(|cell| lines_to_single_string(&cell.display_lines(/*width*/ 80)))
                .collect::<String>(),
            lines
        );
        rendered.push(lines);
    }
    assert_eq!(rendered[0], rendered[1]);
    assert_snapshot!(rendered[0], @"
      ↳ cwd service-a
    • Ran printf ok
      └ ok
    • Ran printf ok
      └ ok
      ↳ cwd service-b
    • Ran printf ok
      └ ok
      ↳ cwd .
    • Ran printf ok
      └ ok
    ");
}

#[tokio::test]
async fn late_completion_does_not_rewind_the_next_commands_directory() {
    let (mut chat, _events, _operations) = make_chatwidget_manual(/*model_override*/ None).await;
    let older = execution(&chat, "older", "service-a");
    handle_exec_begin(&mut chat, older.clone());
    let newer = execution(&chat, "newer", "service-b");
    handle_exec_begin(&mut chat, newer);
    end_exec(&mut chat, older, "ok\n", "", /*exit_code*/ 0);
    assert!(active_blob(&chat).contains("↳ cwd service-b"));
    let next = execution(&chat, "next", "service-b");
    handle_exec_begin(&mut chat, next);
    assert_eq!(active_blob(&chat), "• Running printf ok\n");
}
