use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn continue_consumes_only_its_control_token_and_keeps_remote_attachments() {
    let (mut chat, mut events, mut operations) =
        make_chatwidget_manual(/*model_override*/ None).await;
    chat.bottom_pane
        .set_composer_text("/continue".into(), Vec::new(), Vec::new());
    let images = vec!["https://example.com/draft.png".to_string()];
    chat.bottom_pane.set_remote_image_urls(images.clone());
    chat.handle_slash_command_dispatch(SlashCommand::Continue);

    assert_eq!(chat.bottom_pane.composer_text(), "");
    assert_eq!(chat.bottom_pane.remote_image_urls(), images);
    assert!(operations.try_recv().is_err());
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AppEvent::CodexOp(Op::Continue)))
    );
}

#[tokio::test]
async fn continue_dispatch_preserves_draft_and_queue_without_submitting_user_input() {
    let (mut chat, mut events, mut operations) =
        make_chatwidget_manual(/*model_override*/ None).await;
    chat.bottom_pane
        .set_composer_text("unfinished draft".into(), Vec::new(), Vec::new());
    chat.input_queue
        .queued_user_messages
        .push_back(UserMessage::from("queued follow-up").into());
    chat.handle_slash_command_dispatch(SlashCommand::Continue);

    assert_eq!(chat.bottom_pane.composer_text(), "unfinished draft");
    assert_eq!(chat.input_queue.queued_user_messages.len(), 1);
    assert!(!chat.input_queue.user_turn_pending_start);
    assert!(!chat.bottom_pane.is_task_running());
    assert!(operations.try_recv().is_err());
    let mut continued = false;
    while let Ok(event) = events.try_recv() {
        if let AppEvent::CodexOp(operation) = event {
            assert!(matches!(operation, Op::Continue));
            assert!(!continued);
            continued = true;
        }
    }
    assert!(continued);
}

#[tokio::test]
async fn continue_rejects_active_and_pending_turns_without_consuming_draft() {
    for running in [true, false] {
        let (mut chat, mut events, mut operations) =
            make_chatwidget_manual(/*model_override*/ None).await;
        chat.bottom_pane.set_task_running(running);
        chat.input_queue.user_turn_pending_start = !running;
        chat.bottom_pane
            .set_composer_text("/continue".into(), Vec::new(), Vec::new());
        chat.handle_slash_command_dispatch(SlashCommand::Continue);

        assert_eq!(chat.bottom_pane.composer_text(), "/continue");
        assert!(operations.try_recv().is_err());
        let mut rendered = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                AppEvent::CodexOp(_) => panic!("busy continuation must not submit an operation"),
                AppEvent::InsertHistoryCell(cell) => rendered.extend(
                    cell.display_lines(/*width*/ 120)
                        .iter()
                        .map(ToString::to_string),
                ),
                _ => {}
            }
        }
        insta::allow_duplicates! {
            insta::assert_snapshot!(
                rendered.join("\n"),
                @"■ '/continue' is disabled while a task is in progress."
            );
        }
    }
}
