//! Agent answers retain their requester across view changes and local turn completion.

use super::*;
use crate::agent_question::AgentQuestionAnswer;
use crate::agent_question::AgentQuestionAnswerOutcome;
use crate::agent_question::AgentQuestionBinding;
use crate::agent_question::AgentQuestionDelivery;
use crate::backend_id::BackendId;
use codex_protocol::items::AsyncUserInputQuestion;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn agent_question_draft_survives_main_completion_and_answers_from_another_view() {
    for delivery in [AgentQuestionDelivery::Direct, AgentQuestionDelivery::Queued] {
        let (mut main, _events, mut main_ops) =
            make_chatwidget_manual(/*model_override*/ None).await;
        let root = ThreadId::new();
        let requester = ThreadId::new();
        main.thread_id = Some(root);
        main.bottom_pane
            .set_composer_text("Main's unsent draft".into(), Vec::new(), Vec::new());
        let binding = AgentQuestionBinding {
            backend_id: BackendId::new(),
            source_thread_id: root,
            requester_thread_id: requester,
            requester_label: "Reviewer [worker]".into(),
        };
        main.add_agent_questions(
            binding.clone(),
            "async-question:q1:child-call",
            &[AsyncUserInputQuestion {
                title: "Which deployment?".into(),
                options: None,
            }],
        );
        main.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        let rendered = render_bottom_popup(&main, /*width*/ 80);
        insta::allow_duplicates! {
            insta::assert_snapshot!(
                rendered.lines().find(|line| line.contains("Reviewer")).unwrap(),
                @"  1 of 1 · Reviewer [worker]"
            );
        }
        main.bottom_pane.handle_paste("!literal $answer".into());
        assert_eq!(main.take_question_drafts(), Some(Vec::new()));
        assert_eq!(main.bottom_pane.composer_text(), "Main's unsent draft");
        assert_eq!(main.bottom_pane.question_editor().unanswered_count(), 1);
        let local_state = main.capture_thread_input_state().unwrap();

        let (mut other, mut events, mut other_ops) =
            make_chatwidget_manual(/*model_override*/ None).await;
        other.thread_id = Some(requester);
        other.bottom_pane.set_composer_text(
            "The selected thread's draft".into(),
            Vec::new(),
            Vec::new(),
        );
        other.inherit_agent_questions(&mut main);
        // Replaying the source item after selecting its requester must not create a local copy.
        other.add_async_questions(
            "async-question:q1:child-call",
            &[AsyncUserInputQuestion {
                title: "Which deployment?".into(),
                options: None,
            }],
        );
        assert_eq!(other.bottom_pane.question_editor().unanswered_count(), 1);
        other.handle_key_event(KeyEvent::from(match delivery {
            AgentQuestionDelivery::Direct => KeyCode::Enter,
            AgentQuestionDelivery::Queued => KeyCode::Tab,
        }));
        let answer = std::iter::from_fn(|| events.try_recv().ok())
            .find_map(|event| match event {
                AppEvent::AnswerAgentQuestion(answer) => Some(answer),
                _ => None,
            })
            .expect("bound question answer");
        assert_eq!((&answer.binding, answer.delivery), (&binding, delivery));
        assert_eq!(
            crate::async_question_reply::display_text(&answer.text),
            Some("q1: !literal $answer".into())
        );
        assert_eq!(
            other.bottom_pane.composer_text(),
            "The selected thread's draft"
        );
        assert!(main_ops.try_recv().is_err());
        assert!(other_ops.try_recv().is_err());
        assert!(other.has_pending_agent_answer(&answer));
        other.finish_agent_question_answer(&answer, AgentQuestionAnswerOutcome::Uncertain);
        other.handle_key_event(KeyEvent::from(KeyCode::Enter));
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .all(|event| !matches!(event, AppEvent::AnswerAgentQuestion(_)))
        );
        other.finish_agent_question_answer(&answer, AgentQuestionAnswerOutcome::Rejected);
        other.handle_key_event(KeyEvent::from(KeyCode::Enter));
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, AppEvent::AnswerAgentQuestion(_)))
        );
        other.finish_agent_question_answer(&answer, AgentQuestionAnswerOutcome::Accepted);
        assert_eq!(other.bottom_pane.question_editor().unanswered_count(), 0);

        let (mut restored_main, _events, _) = make_chatwidget_manual(/*model_override*/ None).await;
        restored_main.restore_thread_input_state(
            Some(local_state),
            ThreadInputStateRestoreMode {
                preserve_in_flight_turn: false,
            },
        );
        assert_eq!(
            restored_main
                .bottom_pane
                .question_editor()
                .unanswered_count(),
            0,
            "thread-local snapshots cannot retain a second actionable copy"
        );
    }
}

#[tokio::test]
async fn identical_question_ids_from_different_agents_resolve_independently() {
    let (mut chat, _events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let root = ThreadId::new();
    chat.thread_id = Some(root);
    let questions = [AsyncUserInputQuestion {
        title: "Which deployment?".into(),
        options: None,
    }];
    let first = AgentQuestionBinding {
        backend_id: BackendId::new(),
        source_thread_id: root,
        requester_thread_id: ThreadId::new(),
        requester_label: "First".into(),
    };
    let second = AgentQuestionBinding {
        requester_thread_id: ThreadId::new(),
        requester_label: "Second".into(),
        ..first
    };
    chat.add_async_questions("same-call", &questions);
    chat.add_agent_questions(first.clone(), "same-call", &questions);
    chat.add_agent_questions(second.clone(), "same-call", &questions);
    chat.resolve_agent_question_answers(
        first.clone(),
        &[serde_json::json!(["request_user_input_async", "same-call", 0]).to_string()],
    );
    assert_eq!(chat.bottom_pane.question_editor().unanswered_count(), 2);
    chat.add_agent_questions(first, "same-call", &questions);
    assert_eq!(chat.bottom_pane.question_editor().unanswered_count(), 2);
    chat.bottom_pane.clear_pending_questions();
    assert_eq!(chat.bottom_pane.question_editor().unanswered_count(), 1);
    chat.finish_agent_question_answer(
        &AgentQuestionAnswer {
            binding: second,
            question_id: serde_json::json!(["request_user_input_async", "same-call", 0])
                .to_string(),
            text: String::new(),
            delivery: AgentQuestionDelivery::Direct,
        },
        AgentQuestionAnswerOutcome::Accepted,
    );
    assert_eq!(chat.bottom_pane.question_editor().unanswered_count(), 0);
}

#[tokio::test]
async fn displayed_question_replay_and_receipts_keep_their_backend_scope() {
    let (mut chat, _events, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let requester = ThreadId::new();
    let binding = AgentQuestionBinding {
        backend_id: BackendId::new(),
        source_thread_id: ThreadId::new(),
        requester_thread_id: requester,
        requester_label: "Retained agent".into(),
    };
    let questions = [AsyncUserInputQuestion {
        title: "Which deployment?".into(),
        options: None,
    }];
    chat.add_agent_questions(binding.clone(), "same-call", &questions);
    chat.thread_id = Some(requester);
    chat.set_question_backend(BackendId::new());
    chat.add_async_questions("same-call", &questions);
    assert_eq!(chat.bottom_pane.question_editor().unanswered_count(), 2);
    let ids = [serde_json::json!(["request_user_input_async", "same-call", 0]).to_string()];
    chat.resolve_displayed_question_answers(&ids);
    assert_eq!(chat.bottom_pane.question_editor().unanswered_count(), 1);
    chat.set_question_backend(binding.backend_id);
    chat.resolve_displayed_question_answers(&ids);
    assert_eq!(chat.bottom_pane.question_editor().unanswered_count(), 0);
}
