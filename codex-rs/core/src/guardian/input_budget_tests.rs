//! Exercises reviewer history safety at cancellation and budget boundaries.

use super::*;
use codex_guardian_context::ContextPresentation;
use codex_guardian_context::ContextProfile;
use codex_guardian_context::NodeReplContext;
use codex_guardian_context::NodeReplResponse;
use codex_guardian_context::NodeReplReviewEvidenceMode;
use codex_guardian_context::PlannedAction;
use codex_guardian_context::PlannedActionKind;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ImageDetail;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use test_case::test_case;

fn required_context(text: String) -> ComposedContext {
    let action = PlannedAction {
        tool_descriptions: None,
        json: text,
        kind: PlannedActionKind::Command,
        reason: None,
    };
    let context = super::super::prompt::collect_guardian_context(
        &Vec::<ResponseItem>::new(),
        super::super::GUARDIAN_MAX_TOOL_ENTRY_TOKENS,
        &[],
        &[],
        Some(&action),
        /*permissions*/ None,
        /*node_repl*/ None,
    )
    .expect("collect required context");
    let transcript = ContextProfile::synchronous()
        .render_transcript(context.transcript_entries(), /*entry_number_offset*/ 0);
    context
        .compose(
            ContextPresentation::SyncFull {
                session_id: "test-parent",
            },
            transcript,
        )
        .expect("compose required context")
}

#[tokio::test]
async fn cancelled_startup_does_not_record_unselected_review_evidence() {
    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let context = required_context("unselected review evidence ".repeat(/*n*/ 10_000));
    let content = context.clone().into_user_inputs().unwrap();
    session
        .services
        .thread_extension_data
        .insert(PendingReviewContext(context));
    session
        .set_session_startup_prewarm(
            crate::session::startup_prewarm::SessionStartupPrewarmHandle::new(
                tokio::spawn(std::future::pending()),
                std::time::Instant::now(),
                crate::client::WEBSOCKET_CONNECT_TIMEOUT,
            ),
        )
        .await;
    session
        .spawn_task(
            turn,
            vec![TurnInput::UserInput {
                metadata: Default::default(),
                content,
                client_id: None,
            }],
            crate::tasks::RegularTask::new(),
        )
        .await;
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(started.msg, EventMsg::TurnStarted(_)));
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;

    let history = session.clone_history().await;
    let recorded = history.raw_items().collect::<Vec<_>>();
    assert!(
        !serde_json::to_string(&recorded)
            .unwrap()
            .contains("unselected review evidence")
    );
    assert!(
        session
            .services
            .thread_extension_data
            .get::<PendingReviewContext>()
            .is_some()
    );
}

#[tokio::test]
async fn feasibility_rejects_required_evidence_inside_the_reserved_margin() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let context = required_context("required action".to_owned());
    let base = session.get_prompt_base_instructions().await;
    let prefix = codex_protocol::protocol::TruncationPolicy::Bytes(base.text.len()).token_budget();
    let model = Arc::make_mut(&mut Arc::make_mut(&mut turn.initial_settings).model_info);
    model.effective_context_window_percent = 100;
    Arc::make_mut(&mut turn.config).model_context_window =
        Some(i64::try_from(context.estimated_tokens() + prefix + 128).unwrap());
    session
        .services
        .thread_extension_data
        .insert(PendingReviewContext(context));

    assert!(check_pending(&session, &turn).await.is_err());
}

#[tokio::test]
async fn finalization_overflow_marks_the_reviewer_exhausted() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config).model_context_window = Some(1);
    let session = Arc::new(session);
    let step = session
        .capture_step_context(Arc::new(turn), &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let context = required_context("required action".to_owned());
    let mut input = vec![TurnInput::UserInput {
        metadata: Default::default(),
        content: context.clone().into_user_inputs().unwrap(),
        client_id: None,
    }];
    session
        .services
        .thread_extension_data
        .insert(PendingReviewContext(context));

    assert!(
        finalize(&session, &step, &mut input, HistoryTruncation::Preserve)
            .await
            .is_err()
    );
    assert!(
        session
            .services
            .thread_extension_data
            .get::<super::super::request_budget::ExhaustedReviewBudget>()
            .is_some()
    );
}

#[tokio::test]
async fn compacted_review_restores_originals_once_and_persists_the_request_prefix() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let step = session
        .capture_step_context(Arc::new(turn), &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    session
        .record_user_prompt_and_emit_turn_item(
            &step.turn,
            &step.settings.model_info,
            &[codex_protocol::user_input::UserInput::Text {
                text: "Do not publish.".to_owned(),
                text_elements: vec![],
            }],
            /*client_id*/ None,
            crate::session::UserInputMetadata {
                acceptance_order: Some(0),
                ..Default::default()
            },
            codex_thread_store::PersistContext::Standard,
        )
        .await;
    let context = super::super::prompt::build_guardian_prompt_items(
        &session,
        /*retry_reason*/ None,
        super::super::GuardianApprovalRequest::RequestPermissions {
            id: "review".to_owned(),
            environment_id: "local".to_owned(),
            turn_id: "parent".to_owned(),
            reason: None,
            permissions: Default::default(),
        },
        super::super::prompt::GuardianPromptMode::Full,
    )
    .await
    .unwrap()
    .context;
    step.turn.extension_data.insert(RetainedReviewContext {
        context: context.retained_instructions(),
        history_version: session.clone_history().await.history_version(),
    });
    // Simulate a reviewer compaction that discarded the earlier user-message block.
    session
        .replace_history(vec![], /*reference_context_item*/ None)
        .await;
    let mut prompt = build_prompt(vec![], &step, session.get_prompt_base_instructions().await);
    let metadata = session
        .responses_metadata(&step, CodexResponsesRequestKind::Turn)
        .await;
    super::super::request_budget::prepare_prompt(&session, &mut prompt, &step, &metadata)
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&prompt.input)
            .unwrap()
            .contains("Do not publish.")
    );
    let persisted = session
        .clone_history()
        .await
        .for_prompt(&step.settings.model_info.input_modalities);
    assert_eq!(prompt.input, persisted);
    assert!(
        persisted
            .iter()
            .all(crate::context::is_guardian_context_message)
    );
    assert!(
        !persisted
            .iter()
            .any(crate::context::is_user_authorization_message)
    );
    super::super::request_budget::prepare_prompt(&session, &mut prompt, &step, &metadata)
        .await
        .unwrap();
    assert_eq!(prompt.input, persisted);
}

#[test_case(false; "text transcript")]
#[test_case(true; "native encrypted transcript")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_selection_uses_post_compaction_history_without_replaying_text(
    native_transcript: bool,
) {
    const IMAGE: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .disable(Feature::UnifiedImageBudget)
        .unwrap();
    let model = Arc::make_mut(&mut Arc::make_mut(&mut turn.initial_settings).model_info);
    model.used_fallback_model_metadata = false;
    model.context_window = Some(100_000);
    model.max_context_window = Some(100_000);
    model.input_modalities = vec![InputModality::Text, InputModality::Image];
    let session = Arc::new(session);
    let step = StepContext::for_test(Arc::new(turn));
    let screenshot = UserInput::Image {
        image: ImageReference::Inline {
            image_url: IMAGE.to_owned(),
        },
        detail: Some(ImageDetail::High),
    };
    let screenshots = [screenshot.clone()];
    let evidence = NodeReplContext {
        responses: vec![NodeReplResponse {
            sequence: 7,
            provenance: "tool=node_repl.js cell=1 call=screenshot",
            items: &screenshots,
        }],
        omitted_responses: 0,
        mode: NodeReplReviewEvidenceMode::Multimodal,
    };
    let parent_history = Vec::from_iter(native_transcript.then_some(ResponseItem::AgentMessage {
        id: None,
        author: "/root".into(),
        recipient: "/root/worker".into(),
        content: vec![AgentMessageInputContent::EncryptedContent {
            encrypted_content: "opaque-parent-reply".to_owned(),
        }],
        internal_chat_message_metadata_passthrough: None,
    }));
    let context = super::super::prompt::collect_guardian_context(
        &parent_history,
        super::super::GUARDIAN_MAX_TOOL_ENTRY_TOKENS,
        &[],
        &[],
        /*planned_action*/ None,
        /*permissions*/ None,
        Some(&evidence),
    )
    .unwrap();
    let transcript = ContextProfile::synchronous()
        .render_transcript(context.transcript_entries(), /*entry_number_offset*/ 0);
    let context = context
        .compose(
            ContextPresentation::SyncFull {
                session_id: "test-parent",
            },
            transcript,
        )
        .unwrap();
    let admitted_text = UserInput::Text {
        text: "already-admitted REPL text".to_owned(),
        text_elements: Vec::new(),
    };
    for history_contains_image in [true, false] {
        session
            .services
            .thread_extension_data
            .insert(PendingReviewContext(context.clone()));
        let mut retained = vec![admitted_text.clone()];
        if history_contains_image {
            retained.push(screenshot.clone());
        }
        session
            .replace_history(
                vec![session.response_item_from_user_input(retained)],
                /*reference_context_item*/ None,
            )
            .await;
        let content = match context.clone().into_user_inputs() {
            Ok(content) => {
                assert!(!native_transcript);
                content
            }
            Err(codex_guardian_context::SectionError::UnsupportedDelivery {
                section: "conversation_transcript",
            }) => {
                assert!(native_transcript);
                vec![UserInput::Text {
                    text: super::super::prompt::GUARDIAN_TRANSCRIPT_START.to_owned(),
                    text_elements: Vec::new(),
                }]
            }
            Err(error) => panic!("unexpected review conversion: {error}"),
        };
        let mut input = vec![TurnInput::UserInput {
            metadata: Default::default(),
            content,
            client_id: None,
        }];
        finalize(&session, &step, &mut input, HistoryTruncation::Preserve)
            .await
            .unwrap();

        let mut expected = context.clone();
        expected.retain_images(|_, _| !history_contains_image);
        if native_transcript {
            let actual = input
                .iter()
                .map(|input| {
                    let TurnInput::ResponseItem(envelope) = input else {
                        panic!("native review context must retain its message boundaries");
                    };
                    let mut item = envelope.item.clone();
                    if let ResponseItem::Message {
                        internal_chat_message_metadata_passthrough,
                        ..
                    } = &mut item
                    {
                        // Session-owned user-content kinds are not part of the source context.
                        // Native author, recipient, ciphertext, and message order remain exact.
                        *internal_chat_message_metadata_passthrough = None;
                    }
                    item
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected.into_messages());
            assert!(actual.contains(&parent_history[0]));
        } else {
            let TurnInput::UserInput { content, .. } = &input[0] else {
                panic!("expected finalized review input");
            };
            assert_eq!(*content, expected.into_user_inputs().unwrap());
            assert!(!content.contains(&admitted_text));
        }
    }
}
