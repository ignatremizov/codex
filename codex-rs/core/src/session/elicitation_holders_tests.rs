use std::collections::HashMap;
use std::sync::Arc;

use codex_core_plugins::PluginCommandAttribution;
use codex_plugin::PluginId;
use codex_protocol::approvals::ExecApprovalKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsArgs;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::tests::make_session_and_context_with_rx;
use crate::session::step_context::StepContext;
use crate::state::ActiveTurn;

async fn wait_until_held(pause_state: &mut watch::Receiver<bool>) {
    pause_state
        .wait_for(|paused| *paused)
        .await
        .expect("elicitation service should remain available");
}

async fn wait_until_released(pause_state: &mut watch::Receiver<bool>) {
    pause_state
        .wait_for(|paused| !*paused)
        .await
        .expect("elicitation service should remain available");
}

#[tokio::test]
async fn command_approval_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();
    #[allow(deprecated)]
    let cwd = turn_context.cwd.clone();
    let plugin_attribution = PluginCommandAttribution {
        plugin_id: PluginId::parse("sample@openai-curated").expect("valid plugin id"),
        normalized_relative_path: "scripts/run.py".to_string(),
    };

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    ExecApprovalKind::Command,
                    "call-1".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string()],
                    cwd.into(),
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ Some(plugin_attribution),
                )
                .await
        }
    });

    let event = events.recv().await.expect("approval event");
    let codex_protocol::protocol::EventMsg::ExecApprovalRequest(event) = event.msg else {
        panic!("expected command approval event");
    };
    assert_eq!(event.plugin_id.as_deref(), Some("sample@openai-curated"));
    assert_eq!(event.script_path.as_deref(), Some("scripts/run.py"));
    wait_until_held(&mut pause_state).await;
    session
        .notify_approval("call-1", ReviewDecision::Approved)
        .await;
    request.await.expect("approval task");
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn zero_command_approval_timeout_rejects_without_prompting() {
    let (session, mut turn_context, events) = make_session_and_context_with_rx().await;
    Arc::make_mut(
        &mut Arc::get_mut(&mut turn_context)
            .expect("turn context should be uniquely owned")
            .config,
    )
    .approval_timeout_ms = Some(0);
    #[allow(deprecated)]
    let cwd = turn_context.cwd.clone();

    let decision = session
        .request_command_approval(
            turn_context.as_ref(),
            ExecApprovalKind::Command,
            "call-immediate".to_string(),
            /*approval_id*/ None,
            /*environment_id*/ None,
            vec!["echo".to_string()],
            cwd.into(),
            /*reason*/ None,
            /*network_approval_context*/ None,
            /*proposed_execpolicy_amendment*/ None,
            /*additional_permissions*/ None,
            /*available_decisions*/ None,
            /*plugin_attribution_override*/ None,
        )
        .await;

    assert_eq!(decision, ReviewDecision::TimedOut);
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn command_approval_timeout_rejects_and_releases_elicitation() {
    let (session, mut turn_context, events) = make_session_and_context_with_rx().await;
    Arc::make_mut(
        &mut Arc::get_mut(&mut turn_context)
            .expect("turn context should be uniquely owned")
            .config,
    )
    .approval_timeout_ms = Some(60_000);
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();
    #[allow(deprecated)]
    let cwd = turn_context.cwd.clone();
    tokio::time::pause();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    ExecApprovalKind::Command,
                    "call-timeout".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string()],
                    cwd.into(),
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ None,
                )
                .await
        }
    });

    let event = events.recv().await.expect("approval event");
    let EventMsg::ExecApprovalRequest(event) = event.msg else {
        panic!("expected command approval event");
    };
    assert_eq!(
        event.expires_at_ms,
        Some(event.started_at_ms.saturating_add(60_000))
    );
    wait_until_held(&mut pause_state).await;

    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(/*secs*/ 60)).await;
    tokio::time::resume();

    assert_eq!(
        request.await.expect("approval task"),
        ReviewDecision::TimedOut
    );
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn maximum_command_approval_timeout_remains_resolvable() {
    let (session, mut turn_context, events) = make_session_and_context_with_rx().await;
    Arc::make_mut(
        &mut Arc::get_mut(&mut turn_context)
            .expect("turn context should be uniquely owned")
            .config,
    )
    .approval_timeout_ms = Some(u64::MAX);
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    #[allow(deprecated)]
    let cwd = turn_context.cwd.clone();
    tokio::time::pause();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    ExecApprovalKind::Command,
                    "call-maximum-timeout".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string()],
                    cwd.into(),
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ None,
                )
                .await
        }
    });

    let event = events.recv().await.expect("approval event");
    let EventMsg::ExecApprovalRequest(event) = event.msg else {
        panic!("expected command approval event");
    };
    assert_eq!(event.expires_at_ms, Some(i64::MAX));
    tokio::time::advance(std::time::Duration::from_secs(/*secs*/ 1)).await;
    assert!(!request.is_finished());

    session
        .notify_approval("call-maximum-timeout", ReviewDecision::Approved)
        .await;
    assert_eq!(
        request.await.expect("approval task"),
        ReviewDecision::Approved
    );
    tokio::time::resume();
}

#[tokio::test]
async fn command_approval_response_before_timeout_wins() {
    let (session, mut turn_context, events) = make_session_and_context_with_rx().await;
    Arc::make_mut(
        &mut Arc::get_mut(&mut turn_context)
            .expect("turn context should be uniquely owned")
            .config,
    )
    .approval_timeout_ms = Some(60_000);
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    #[allow(deprecated)]
    let cwd = turn_context.cwd.clone();
    tokio::time::pause();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    ExecApprovalKind::Command,
                    "call-approved".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string()],
                    cwd.into(),
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ None,
                )
                .await
        }
    });

    events.recv().await.expect("approval event");
    session
        .notify_approval("call-approved", ReviewDecision::Approved)
        .await;

    assert_eq!(
        request.await.expect("approval task"),
        ReviewDecision::Approved
    );
    tokio::time::resume();
}

#[tokio::test]
async fn claimed_command_approval_survives_ordered_queue_delay() {
    let (session, mut turn_context, events) = make_session_and_context_with_rx().await;
    Arc::make_mut(
        &mut Arc::get_mut(&mut turn_context)
            .expect("turn context should be uniquely owned")
            .config,
    )
    .approval_timeout_ms = Some(60_000);
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    #[allow(deprecated)]
    let cwd = turn_context.cwd.clone();
    tokio::time::pause();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    ExecApprovalKind::Command,
                    "call-claimed".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string()],
                    cwd.into(),
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ None,
                )
                .await
        }
    });

    events.recv().await.expect("approval event");
    assert!(session.claim_pending_approval("call-claimed").await);
    tokio::time::advance(std::time::Duration::from_secs(/*secs*/ 60)).await;
    tokio::task::yield_now().await;
    assert!(!request.is_finished());

    assert!(
        session
            .notify_approval("call-claimed", ReviewDecision::Approved)
            .await
    );
    assert_eq!(
        request.await.expect("approval task"),
        ReviewDecision::Approved
    );
    tokio::time::resume();
}

#[tokio::test]
async fn duplicate_command_approval_id_aborts_new_request() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    #[allow(deprecated)]
    let cwd = turn_context.cwd.clone();

    let first_request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        let cwd = cwd.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    ExecApprovalKind::Command,
                    "duplicate-call".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string(), "first".to_string()],
                    cwd.into(),
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ None,
                )
                .await
        }
    });
    events.recv().await.expect("first approval event");

    let second_request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    ExecApprovalKind::Command,
                    "duplicate-call".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string(), "second".to_string()],
                    cwd.into(),
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ None,
                )
                .await
        }
    });
    assert_eq!(
        second_request.await.expect("second approval task"),
        ReviewDecision::Abort
    );
    assert!(
        session
            .notify_approval("duplicate-call", ReviewDecision::Approved)
            .await
    );
    assert_eq!(
        first_request.await.expect("first approval task"),
        ReviewDecision::Approved
    );
}

#[tokio::test]
async fn command_approval_response_after_timeout_is_ignored() {
    let (session, mut turn_context, events) = make_session_and_context_with_rx().await;
    Arc::make_mut(
        &mut Arc::get_mut(&mut turn_context)
            .expect("turn context should be uniquely owned")
            .config,
    )
    .approval_timeout_ms = Some(60_000);
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    #[allow(deprecated)]
    let cwd = turn_context.cwd.clone();
    tokio::time::pause();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_command_approval(
                    turn_context.as_ref(),
                    ExecApprovalKind::Command,
                    "call-late".to_string(),
                    /*approval_id*/ None,
                    /*environment_id*/ None,
                    vec!["echo".to_string()],
                    cwd.into(),
                    /*reason*/ None,
                    /*network_approval_context*/ None,
                    /*proposed_execpolicy_amendment*/ None,
                    /*additional_permissions*/ None,
                    /*available_decisions*/ None,
                    /*plugin_attribution_override*/ None,
                )
                .await
        }
    });

    events.recv().await.expect("approval event");
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(/*secs*/ 60)).await;
    assert!(
        !session
            .notify_approval("call-late", ReviewDecision::Approved)
            .await
    );

    assert_eq!(
        request.await.expect("approval task"),
        ReviewDecision::TimedOut
    );
    tokio::time::resume();
}

#[tokio::test]
async fn patch_approval_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_patch_approval(
                    turn_context.as_ref(),
                    "call-1".to_string(),
                    HashMap::new(),
                    /*reason*/ None,
                    /*grant_root*/ None,
                )
                .await
        }
    });

    events.recv().await.expect("approval event");
    wait_until_held(&mut pause_state).await;
    session
        .notify_approval("call-1", ReviewDecision::Approved)
        .await;
    request.await.expect("approval task");
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn permission_request_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            let environment = turn_context
                .environments
                .primary()
                .expect("primary environment")
                .selection();
            session
                .request_permissions_for_environment(
                    &StepContext::for_test(Arc::clone(&turn_context)),
                    "call-1".to_string(),
                    RequestPermissionsArgs {
                        environment_id: None,
                        reason: None,
                        permissions: RequestPermissionProfile::default(),
                    },
                    environment,
                    CancellationToken::new(),
                )
                .await
        }
    });

    events.recv().await.expect("permission request event");
    wait_until_held(&mut pause_state).await;
    session
        .notify_request_permissions_response(
            "call-1",
            RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
                strict_auto_review: false,
            },
        )
        .await;
    request.await.expect("permission request task");
    wait_until_released(&mut pause_state).await;
}

#[tokio::test]
async fn request_user_input_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_user_input(
                    turn_context.as_ref(),
                    "call-1".to_string(),
                    RequestUserInputArgs {
                        questions: Vec::new(),
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await
        }
    });

    events.recv().await.expect("request user input event");
    wait_until_held(&mut pause_state).await;

    let response = RequestUserInputResponse {
        answers: HashMap::new(),
    };
    session
        .notify_user_input_response(&turn_context.sub_id, response)
        .await;

    request.await.expect("request user input task");
    wait_until_released(&mut pause_state).await;
}
