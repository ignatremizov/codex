use super::*;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use crate::history_cell::HistoryCell;
use crate::test_support::test_path_buf;
use codex_protocol::user_input::ByteRange;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn mail_keeps_typed_draft_rebases_elements_and_retries_identically_after_focus_restore() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    let draft = UserMessage {
        text: "/mail review @docs [Image #2]".into(),
        local_images: vec![LocalImageAttachment {
            placeholder: "[Image #2]".into(),
            path: test_path_buf("/tmp/mail.png"),
        }],
        remote_image_urls: vec!["data:image/png;base64,cGlj".into()],
        text_elements: vec![
            TextElement {
                byte_range: ByteRange { start: 13, end: 18 },
                placeholder: Some("@docs".into()),
            },
            TextElement {
                byte_range: ByteRange { start: 19, end: 29 },
                placeholder: Some("[Image #2]".into()),
            },
        ],
        mention_bindings: vec![MentionBinding {
            sigil: '@',
            mention: "docs".into(),
            path: "app://docs".into(),
        }],
    };
    chat.restore_user_message_to_composer(draft.clone());
    chat.handle_slash_command_with_args_dispatch(
        SlashCommand::Mail,
        "review @docs".into(),
        Vec::new(),
    );
    let submission = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            AppEvent::SubmitUserMailbox(submission) => Some(submission),
            _ => None,
        })
        .expect("typed mail event");
    let mut expected = draft.clone();
    expected.text = "review @docs [Image #2]".into();
    expected.text_elements[0].byte_range = ByteRange { start: 7, end: 12 };
    expected.text_elements[1].byte_range = ByteRange { start: 13, end: 23 };
    assert_eq!(submission.user_message, expected);
    assert_eq!(submission.thread_id, thread_id);
    assert_eq!(submission.input, chat.user_inputs_from_message(&expected));
    assert!(chat.input_queue.queued_user_messages.is_empty());
    assert!(ops.try_recv().is_err());

    chat.finish_mailbox_submission(submission.clone(), Err("connection lost".into()));
    let captured = chat.capture_thread_input_state();
    chat.restore_thread_input_state(
        captured,
        ThreadInputStateRestoreMode {
            preserve_in_flight_turn: true,
        },
    );
    chat.handle_slash_command_with_args_dispatch(
        SlashCommand::Mail,
        "review @docs".into(),
        Vec::new(),
    );
    let retried = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            AppEvent::SubmitUserMailbox(submission) => Some(submission),
            _ => None,
        })
        .expect("retry mail event");
    assert_eq!(retried, submission);
}

#[tokio::test]
async fn enter_and_queue_key_submit_mail_without_steering_or_queuing() {
    for key in [KeyCode::Enter, KeyCode::Tab] {
        let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
        chat.thread_id = Some(ThreadId::new());
        chat.bottom_pane.set_task_running(/*running*/ true);
        chat.turn_lifecycle.agent_turn_running = true;
        chat.bottom_pane
            .set_composer_text("/mai".into(), Vec::new(), Vec::new());
        chat.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(chat.bottom_pane.composer_text(), "/mail ");
        chat.bottom_pane.set_composer_text(
            "/mail !this is user content, not a shell command".into(),
            Vec::new(),
            Vec::new(),
        );
        chat.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE));
        let submissions = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::SubmitUserMailbox(submission) => Some(submission.user_message.text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            submissions,
            vec!["!this is user content, not a shell command"]
        );
        assert!(chat.input_queue.queued_user_messages.is_empty());
        assert!(chat.input_queue.pending_steers.is_empty());
        assert!(ops.try_recv().is_err());
    }
}

#[tokio::test]
async fn mailbox_failure_preserves_new_draft_and_attachment_only_mail_is_not_a_turn() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(ThreadId::new());
    chat.bottom_pane
        .set_composer_text("/mail".into(), Vec::new(), Vec::new());
    chat.bottom_pane
        .set_remote_image_urls(vec!["data:image/png;base64,cGlj".into()]);
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let submission = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            AppEvent::SubmitUserMailbox(submission) => Some(submission),
            _ => None,
        })
        .expect("attachment-only mail");
    assert_eq!(
        submission.input,
        vec![UserInput::Image {
            url: "data:image/png;base64,cGlj".into(),
            detail: None
        }]
    );
    chat.bottom_pane
        .set_composer_text("new draft".into(), Vec::new(), Vec::new());
    chat.finish_mailbox_submission(submission, Err("offline".into()));
    let restored = chat.bottom_pane.composer_draft_snapshot();
    assert!(restored.text.contains("/mail "));
    assert!(restored.text.contains("new draft"));
    assert_eq!(
        restored.remote_image_urls,
        vec!["data:image/png;base64,cGlj"]
    );
    assert!(ops.try_recv().is_err());
    assert!(chat.input_queue.queued_user_messages.is_empty());
}

#[tokio::test]
async fn mailbox_response_states_do_not_claim_unread_after_consumption_or_rejection() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    for state in [
        ThreadMailboxMessageState::Pending,
        ThreadMailboxMessageState::Claimed,
        ThreadMailboxMessageState::Consumed,
        ThreadMailboxMessageState::Rejected,
    ] {
        chat.finish_mailbox_submission(
            MailboxSubmission {
                thread_id,
                user_message: "Review later.".into(),
                input: Vec::new(),
                client_user_message_id: "client-id".into(),
            },
            Ok(ThreadMailboxAddResponse {
                message_id: "mail-id".into(),
                state,
                rejection_reason: Some("recipient unavailable".into()),
            }),
        );
    }
    let rendered = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(
                cell.display_lines(/*width*/ 120)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered.trim(), @r"
    › Review later.

    Mailbox: accepted · unread; the agent selects when to consume.

    › Review later.

    Mailbox: claimed for delivery · consumption not yet confirmed.

    › Review later.

    Mailbox: already consumed · not submitted again.

    › Review later.

    Mailbox: rejected · recipient unavailable
    ");
    let user =
        history_cell::new_user_prompt("Review later.".into(), Vec::new(), Vec::new(), Vec::new());
    let receipt = history_cell::mailbox::MailboxAcceptanceHistoryCell::new(
        "Review later.".into(),
        "Mailbox: accepted".into(),
    );
    let user_lines = user.display_lines(/*width*/ 120);
    assert_eq!(
        &receipt.display_lines(/*width*/ 120)[..user_lines.len()],
        user_lines.as_slice()
    );
    assert!(receipt.transcript_navigation_kind().is_none());
}
