//! Numbered answer previews and transcript history share the stable question reference.

use super::*;
use codex_context_fragments::AnsweredQuestion;
use codex_context_fragments::ContextualUserFragment;
use codex_protocol::items::AsyncUserInputQuestion;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn numbered_answers_keep_their_reference_after_earlier_questions_are_removed() {
    for queued in [false, true] {
        let (mut chat, _rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.thread_id = Some(ThreadId::new());
        chat.input_queue.suppress_queue_autosend = queued;
        chat.add_async_questions(
            "async-question:q1:call",
            &[
                AsyncUserInputQuestion {
                    title: "An earlier question".into(),
                    options: None,
                },
                AsyncUserInputQuestion {
                    title: "Should I include the wait-restoration fix before the final build?"
                        .into(),
                    options: None,
                },
            ],
        );
        chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        chat.handle_key_event(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        chat.bottom_pane
            .handle_paste("Include the wait-restoration fix".into());
        chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

        let canonical = AnsweredQuestion::new(
            r#"["request_user_input_async","async-question:q1:call",1]"#,
            "Should I include the wait-restoration fix before the final build?",
            "Include the wait-restoration fix",
        )
        .render();
        if queued {
            assert_eq!(
                chat.input_queue.queued_user_messages.front().unwrap().text,
                canonical
            );
            let preview = chat.pop_latest_queued_composer_state().unwrap();
            assert_eq!(preview.text, "q2: Include the wait-restoration fix");
            assert!(ops.try_recv().is_err());
        } else {
            let Op::UserTurn { items, .. } = ops.try_recv().unwrap() else {
                panic!("user turn")
            };
            assert_eq!(
                items,
                vec![UserInput::Text {
                    text: canonical.clone(),
                    text_elements: Vec::new(),
                }]
            );
        }

        let cells = crate::thread_transcript::thread_items_to_transcript_cells(
            /*thread_id*/ None,
            &chat.config.cwd,
            [AppServerThreadItem::UserMessage {
                id: "answer".into(),
                client_id: None,
                content: vec![UserInput::Text {
                    text: canonical,
                    text_elements: Vec::new(),
                }],
            }],
            crate::thread_transcript::RawReasoningVisibility::Hidden,
            /*config*/ None,
        );
        insta::allow_duplicates! {
            insta::assert_snapshot!(
                lines_to_single_string(&cells[0].transcript_lines(/*width*/ 80)),
                @r"

                › q2: Include the wait-restoration fix
                "
            );
        }
    }
}
