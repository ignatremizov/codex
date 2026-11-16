//! The selected controller preserves exact structured admissions and rejects unsafe modes.

use super::*;
use crate::agent::api::UnifiedInputPolicy;
use crate::agent::api::UnifiedModelInput;
use crate::agent::api::UnifiedModelInputRequest;
use test_case::test_case;

#[tokio::test]
async fn prepared_encrypted_input_rejects_before_target_resolution() {
    let harness = AgentControlHarness::new().await;
    let (sender, root) = harness.start_thread().await;
    let result = harness
        .control
        .submit_unified_input(UnifiedModelInputRequest {
            caller: sender,
            source_instance_id: root.session.presentation_id().instance_id,
            sender_turn_id: "not-admitted".into(),
            call_id: "send-1".into(),
            batch_id: None,
            target: AgentTarget::Reference("not-a-live-worker".into()),
            resume_config: harness.config.clone(),
            input: UnifiedModelInput::Prepared(AgentMessage::EncryptedWithAudit {
                encrypted_content: "opaque ciphertext".into(),
                audit_content: "complete readable message".into(),
            }),
            policy: UnifiedInputPolicy::Default,
            interrupt_descendant: true,
            start_options: TurnStartOptions {
                parent_turn_id: Some("not-admitted".into()),
                ..Default::default()
            },
        })
        .await;
    let error = result
        .err()
        .expect("prepared input must fail before lookup");
    assert!(matches!(
        error.details(),
        CodexErrorDetails::UnsupportedOperation(_)
    ));
    assert!(error.to_string().contains("typed receiving-turn admission"));
    root.shutdown_and_wait().await.expect("close root");
}

#[tokio::test]
async fn durable_mailbox_input_rejects_before_target_resolution() {
    let harness = AgentControlHarness::new().await;
    let (sender, root) = harness.start_thread().await;
    let result = harness
        .control
        .submit_unified_input(UnifiedModelInputRequest {
            caller: sender,
            source_instance_id: root.session.presentation_id().instance_id,
            sender_turn_id: "not-admitted".into(),
            call_id: "send-2".into(),
            batch_id: None,
            target: AgentTarget::Reference("not-a-live-worker".into()),
            resume_config: harness.config.clone(),
            input: UnifiedModelInput::Structured(vec![UserInput::Text {
                text: "keep this typed message".into(),
                text_elements: Vec::new(),
            }]),
            policy: UnifiedInputPolicy::Mailbox {
                final_subscription: codex_protocol::WakeEventMailboxSubscription::Wake,
            },
            interrupt_descendant: false,
            start_options: TurnStartOptions {
                parent_turn_id: Some("not-admitted".into()),
                ..Default::default()
            },
        })
        .await;
    let error = result
        .err()
        .expect("durable mailbox must fail before lookup");
    assert!(matches!(
        error.details(),
        CodexErrorDetails::UnsupportedOperation(_)
    ));
    assert!(error.to_string().contains("versioned payload/claim"));
    root.shutdown_and_wait().await.expect("close root");
}

#[tokio::test]
async fn source_turn_epoch_rejects_before_target_resolution() {
    let harness = AgentControlHarness::new().await;
    let (sender, root) = harness.start_thread().await;
    let result = harness
        .control
        .submit_unified_input(UnifiedModelInputRequest {
            caller: sender,
            source_instance_id: root.session.presentation_id().instance_id,
            sender_turn_id: "not-a-current-turn".into(),
            call_id: "send-3".into(),
            batch_id: None,
            target: AgentTarget::Reference("not-a-live-worker".into()),
            resume_config: harness.config.clone(),
            input: UnifiedModelInput::Structured(vec![UserInput::Text {
                text: "keep sender provenance".into(),
                text_elements: Vec::new(),
            }]),
            policy: UnifiedInputPolicy::Default,
            interrupt_descendant: false,
            start_options: TurnStartOptions {
                parent_turn_id: Some("not-a-current-turn".into()),
                ..Default::default()
            },
        })
        .await;
    let error = result.err().expect("stale sender turn must fail first");
    assert!(matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(_)
    ));
    assert!(error.to_string().contains("originating turn"));
    root.shutdown_and_wait().await.expect("close root");
}

struct ActiveAgentTurn {
    started: Arc<tokio::sync::Notify>,
}

impl crate::tasks::SessionTask for ActiveAgentTurn {
    fn kind(&self) -> crate::state::TaskKind {
        crate::state::TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "agent_control_test.unified_input"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<crate::session::session::Session>,
        ctx: Arc<crate::session::turn_context::TurnContext>,
        _input: Vec<crate::session::TurnInput>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> crate::tasks::SessionTaskResult {
        // Synthetic tasks must publish the same live admission event as normal
        // turns. Merely spawning a task does not establish sender-turn authority.
        session
            .send_event(
                ctx.as_ref(),
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_attribution: None,
                    turn_id: ctx.sub_id.clone(),
                    root_turn_id: None,
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                    agent_queue: None,
                }),
            )
            .await;
        self.started.notify_one();
        cancellation_token.cancelled().await;
        Ok(None)
    }
}

#[test_case(false; "ordinary steers active turn")]
#[test_case(true; "distinct queued input does not steer")]
#[tokio::test]
async fn structured_unified_input_preserves_exact_source_and_turn_admission(queued: bool) {
    use crate::UserAgentResponseHandling;
    use crate::UserAgentSpawnOptions;
    use crate::agent::api::UnifiedInputStatus;
    use codex_protocol::protocol::AgentResponseFinalDelivery;

    const SOURCE_TURN: &str = "unified-source";
    const RECEIVER_TURN: &str = "unified-receiver";
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let sender = root
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn sender");
    let receiver = root
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn receiver");
    let sender = harness
        .manager
        .get_thread(sender.target_thread_id)
        .await
        .expect("sender");
    let receiver = harness
        .manager
        .get_thread(receiver.target_thread_id)
        .await
        .expect("receiver");
    for (thread, name) in [(&sender, SOURCE_TURN), (&receiver, RECEIVER_TURN)] {
        let started = Arc::new(tokio::sync::Notify::new());
        let turn = thread
            .session
            .new_turn_with_default_settings(name.to_string(), Default::default())
            .await;
        thread
            .session
            .spawn_task(
                turn,
                Vec::new(),
                ActiveAgentTurn {
                    started: Arc::clone(&started),
                },
            )
            .await;
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), started.notified())
            .await
            .expect("active turn started");
        // The receiving and sending turns must be live admission identities,
        // not synthetic tasks that never published their TurnStarted event.
        std::assert_eq!(
            thread.session.active_agent_response_turn_id().as_deref(),
            Some(name),
        );
    }
    let policy = if queued {
        UnifiedInputPolicy::Queue {
            commentary: false,
            final_delivery: AgentResponseFinalDelivery::None,
        }
    } else {
        UnifiedInputPolicy::Response {
            commentary: false,
            final_delivery: AgentResponseFinalDelivery::None,
        }
    };
    // Exercise the exact selected agent-tree controller, not the harness's
    // unbound manager prototype. An unbound prototype has a different root
    // session identity and must fail owns_messaging_runtime even for a live turn.
    let selected_control = sender.session.services.agent_control.as_ref();
    std::assert_eq!(selected_control.identity(), sender.session.session_id());
    let result = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 10),
        selected_control.submit_unified_input(UnifiedModelInputRequest {
            caller: sender.session.thread_id(),
            source_instance_id: sender.session.presentation_id().instance_id,
            sender_turn_id: SOURCE_TURN.to_owned(),
            call_id: "unified-tool-input".to_owned(),
            batch_id: Some("unified-batch".to_owned()),
            target: AgentTarget::Id(receiver.session.thread_id()),
            resume_config: harness.config.clone(),
            input: UnifiedModelInput::Structured(vec![UserInput::Text {
                text: "Original agent-authored structured text".to_string(),
                text_elements: Vec::new(),
            }]),
            policy,
            interrupt_descendant: false,
            start_options: TurnStartOptions {
                parent_turn_id: Some(SOURCE_TURN.to_owned()),
                ..Default::default()
            },
        }),
    )
    .await
    .expect("native structured submission resolved")
    .expect("native structured admission");
    std::assert_eq!(
        result.status,
        if queued {
            UnifiedInputStatus::Queued
        } else {
            UnifiedInputStatus::Submitted
        }
    );
    std::assert_eq!(
        result.target_turn_id.as_deref(),
        if queued { None } else { Some(RECEIVER_TURN) }
    );
    assert!(result.warning.is_none());
    assert!(!result.submission_id.is_empty());
    std::assert_eq!(
        harness
            .control
            .runtime
            .upgrade()
            .expect("native manager")
            .agent_turn_queue
            .has_pending(receiver.session.thread_id()),
        queued,
    );
    sender.shutdown_and_wait().await.expect("shutdown sender");
    receiver
        .shutdown_and_wait()
        .await
        .expect("shutdown receiver");
    root.shutdown_and_wait().await.expect("shutdown root");
}
