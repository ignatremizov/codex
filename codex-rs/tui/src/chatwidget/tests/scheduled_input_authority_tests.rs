use super::*;
use crate::scheduled_input::ScheduleAction;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn schedule_mutations_preserve_quarantined_draft_media_and_queue_before_preparation() {
    for external_writer in [false, true] {
        for args in ["in 2h retained prompt", "cancel retained-id"] {
            let (mut chat, mut events, mut operations) =
                make_chatwidget_manual(/*model_override*/ None).await;
            chat.thread_id = Some(ThreadId::new());
            if external_writer {
                chat.show_external_writer_thread();
            } else {
                chat.show_provider_error(
                    codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
                        model_provider: "old-provider".into(),
                    },
                    /*rejected_message_id*/ None,
                );
                assert!(!chat.is_external_writer_view());
            }
            chat.bottom_pane
                .set_composer_text(format!("/schedule {args}"), Vec::new(), Vec::new());
            chat.bottom_pane
                .set_remote_image_urls(vec!["https://example.com/retained.png".into()]);
            chat.input_queue
                .queued_user_messages
                .push_back(UserMessage::from("queued follow-up").into());
            let before = chat.bottom_pane.composer_draft_snapshot();
            while events.try_recv().is_ok() {}

            chat.handle_slash_command_with_args_dispatch(
                SlashCommand::Schedule,
                args.into(),
                Vec::new(),
            );
            chat.dispatch_command_with_args(SlashCommand::Schedule, args.into(), Vec::new());

            let mut after = chat.bottom_pane.composer_draft_snapshot();
            after.last_composer_activity_at = before.last_composer_activity_at;
            assert_eq!(after, before);
            assert_eq!(chat.queued_user_message_texts(), vec!["queued follow-up"]);
            assert!(chat.direct_input_is_blocked());
            assert!(operations.try_recv().is_err());
            assert!(std::iter::from_fn(|| events.try_recv().ok()).all(|event| {
                !matches!(event, AppEvent::ScheduleInput { .. } | AppEvent::CodexOp(_))
            }));
        }
    }
}

#[tokio::test]
async fn direct_schedule_mutation_restores_payload_and_element_offsets_under_quarantine() {
    for external_writer in [false, true] {
        for args in ["in 2h retained", "cancel retained-id"] {
            let (mut chat, mut events, mut operations) =
                make_chatwidget_manual(/*model_override*/ None).await;
            chat.thread_id = Some(ThreadId::new());
            if external_writer {
                chat.show_external_writer_thread();
            } else {
                chat.show_provider_error(
                    codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
                        model_provider: "old-provider".into(),
                    },
                    /*rejected_message_id*/ None,
                );
            }
            let start = args.rfind(' ').expect("command argument") + 1;
            let element =
                TextElement::new((start..args.len()).into(), Some(args[start..].to_string()));
            let mut expected_element = element.clone();
            expected_element.byte_range.start += "/schedule ".len();
            expected_element.byte_range.end += "/schedule ".len();
            while events.try_recv().is_ok() {}

            chat.submit_schedule_command(UserMessage {
                text: args.into(),
                text_elements: vec![element],
                local_images: Vec::new(),
                remote_image_urls: Vec::new(),
                mention_bindings: Vec::new(),
            });

            let restored = chat.bottom_pane.composer_draft_snapshot();
            assert_eq!(restored.text, format!("/schedule {args}"));
            assert_eq!(
                restored.text_elements,
                vec![
                    TextElement::new((0.."/schedule".len()).into(), Some("/schedule".into())),
                    expected_element,
                ]
            );
            assert!(chat.direct_input_is_blocked());
            assert!(operations.try_recv().is_err());
            assert!(std::iter::from_fn(|| events.try_recv().ok()).all(|event| {
                !matches!(event, AppEvent::ScheduleInput { .. } | AppEvent::CodexOp(_))
            }));
        }
    }
}

#[tokio::test]
async fn schedule_inspection_remains_available_without_granting_input_authority() {
    for external_writer in [false, true] {
        let (mut chat, mut events, mut operations) =
            make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        if external_writer {
            chat.show_external_writer_thread();
        } else {
            chat.show_provider_error(
                codex_app_server_protocol::ModelProviderError::ModelProviderNotConfigured {
                    model_provider: "old-provider".into(),
                },
                /*rejected_message_id*/ None,
            );
        }
        while events.try_recv().is_ok() {}
        for args in ["list", "result retained-id"] {
            chat.submit_schedule_command(args.into());
        }
        let actions = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::ScheduleInput {
                    thread_id: actual,
                    action,
                    ..
                } => {
                    assert_eq!(actual, thread_id);
                    Some(action)
                }
                AppEvent::CodexOp(_) => panic!("inspection must not submit an operation"),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec![
                ScheduleAction::List,
                ScheduleAction::Result("retained-id".into())
            ]
        );
        assert!(chat.direct_input_is_blocked());
        assert!(operations.try_recv().is_err());
    }
}
