use codex_core::NotSubmittedReason;
use codex_core::StartIfIdleSubmission;
use codex_core::SuspendTurnOutcome;
use codex_core::TurnInputRequest;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::PermissionProfileSnapshot;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::responses;
use core_test_support::skip_if_remote;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use test_case::test_case;

#[derive(Clone, Copy)]
enum Attachment {
    Live,
    Resumed,
    ResumedModelDefault,
    Suspended,
}

#[derive(Clone, Copy)]
enum Roots {
    Runtime,
    WithProfile,
}

#[test_case(ThreadHistoryMode::Legacy, Attachment::Live, Roots::Runtime; "legacy live")]
#[test_case(ThreadHistoryMode::Paginated, Attachment::Live, Roots::Runtime; "paginated live")]
#[test_case(ThreadHistoryMode::Legacy, Attachment::Resumed, Roots::Runtime; "legacy resumed")]
#[test_case(ThreadHistoryMode::Paginated, Attachment::Resumed, Roots::Runtime; "paginated resumed")]
#[test_case(ThreadHistoryMode::Paginated, Attachment::ResumedModelDefault, Roots::Runtime; "resumed equivalent model default")]
#[test_case(ThreadHistoryMode::Legacy, Attachment::Suspended, Roots::Runtime; "legacy unfinished restart")]
#[test_case(ThreadHistoryMode::Paginated, Attachment::Suspended, Roots::Runtime; "paginated unfinished restart")]
#[test_case(ThreadHistoryMode::Legacy, Attachment::Live, Roots::WithProfile; "legacy profile root")]
#[test_case(ThreadHistoryMode::Paginated, Attachment::Resumed, Roots::WithProfile; "paginated profile root")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_preserves_original_input_and_completed_tool_work(
    mode: ThreadHistoryMode,
    attachment: Attachment,
    roots: Roots,
) -> anyhow::Result<()> {
    skip_if_remote!(
        Ok(()),
        "continuation currently certifies only thread-configured local environments"
    );
    let server = responses::start_mock_server().await;
    let initial = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("plan"),
            responses::ev_function_call(
                "saved-plan-call",
                "update_plan",
                r#"{"plan":[{"step":"Finish the original task","status":"in_progress"}]}"#,
            ),
            responses::ev_completed("plan"),
        ]),
    )
    .await;
    let blocked = responses::mount_sse_once_match_with_delay(
        &server,
        |request: &wiremock::Request| responses::body_contains(request, "saved-plan-call"),
        responses::sse_completed("interrupted-response"),
        Duration::from_secs(/*secs*/ 60),
    )
    .await;
    let test = test_codex()
        .with_history_mode(mode)
        .with_config(move |config| {
            config.update_plan_enabled = true;
            if matches!(attachment, Attachment::ResumedModelDefault) {
                // An unknown model has an Auto summary default. Start with that value
                // explicitly selected, then let cold resume use the same model default.
                config.model = Some("continuation-fixture-model".to_string());
                config.model_reasoning_summary = Some(ReasoningSummary::Auto);
            }
            if matches!(roots, Roots::WithProfile) {
                let profile = PermissionProfileSnapshot::active_with_profile_workspace_roots(
                    config.permissions.permission_profile().clone(),
                    ActivePermissionProfile::new("continuation-fixture"),
                    vec![config.cwd.join("profile-root").into()],
                );
                config
                    .permissions
                    .set_permission_profile_from_session_snapshot(profile)
                    .expect("retain the configured policy with an additional profile root");
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let admission = test
        .codex
        .start_turn_if_idle(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Finish the original task".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let StartIfIdleSubmission::Started {
        turn_id,
        root_turn_id,
    } = admission
    else {
        anyhow::bail!("original turn was not admitted");
    };
    // Wait for the actual post-tool request, proving both dispatch and result persistence.
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        while blocked.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    let original_input = initial.single_request().input();
    let saved_output = blocked
        .single_request()
        .function_call_output("saved-plan-call");
    assert_eq!(
        test.codex.continue_turn().await?,
        StartIfIdleSubmission::NotSubmitted {
            reason: NotSubmittedReason::NotIdle
        }
    );
    if matches!(attachment, Attachment::Suspended) {
        assert_eq!(
            test.codex.suspend_turn_and_shutdown().await?,
            SuspendTurnOutcome::Suspended {
                turn_id: turn_id.clone(),
            }
        );
    } else {
        test.codex.submit(Op::Interrupt).await?;
    }
    let mut original_plans = Vec::new();
    wait_for_event(&test.codex, |event| {
        if let EventMsg::PlanUpdate(plan) = event {
            original_plans.push(plan.clone());
        }
        match attachment {
            Attachment::Suspended => matches!(event, EventMsg::ShutdownComplete),
            Attachment::Live | Attachment::Resumed | Attachment::ResumedModelDefault => {
                matches!(event, EventMsg::TurnAborted(_))
            }
        }
    })
    .await;
    assert_eq!(original_plans.len(), 1);

    let thread = match attachment {
        Attachment::Live => Arc::clone(&test.codex),
        Attachment::Resumed | Attachment::ResumedModelDefault | Attachment::Suspended => {
            if matches!(
                attachment,
                Attachment::Resumed | Attachment::ResumedModelDefault
            ) {
                test.codex.shutdown_and_wait().await?;
            }
            let mut config = test.config.clone();
            if matches!(attachment, Attachment::ResumedModelDefault) {
                config.model_reasoning_summary = None;
            }
            let history = test
                .thread_store
                .load_latest_model_context(LoadThreadHistoryParams {
                    thread_id: test.session_configured.thread_id,
                    include_archived: true,
                })
                .await?;
            test.thread_manager
                .resume_thread_with_history(
                    config,
                    InitialHistory::Resumed(ResumedHistory {
                        conversation_id: history.thread_id,
                        history: Arc::new(history.items),
                        history_revision: history.revision,
                        rollout_path: test.session_configured.rollout_path.clone(),
                    }),
                    test.thread_manager.auth_manager(),
                    /*parent_trace*/ None,
                    ClientMcpExtensions::default(),
                )
                .await?
                .thread
        }
    };
    let continuation = responses::mount_sse_once_with_delay(
        &server,
        responses::sse_completed("continued"),
        Duration::from_secs(/*secs*/ 60),
    )
    .await;
    assert_eq!(
        thread.continue_turn().await?,
        StartIfIdleSubmission::Started {
            turn_id: turn_id.clone(),
            root_turn_id: root_turn_id.clone(),
        }
    );
    tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        while continuation.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    assert_eq!(
        thread.continue_turn().await?,
        StartIfIdleSubmission::NotSubmitted {
            reason: NotSubmittedReason::NotIdle
        }
    );
    thread.submit(Op::Interrupt).await?;
    let mut replayed_plans = Vec::new();
    wait_for_event(&thread, |event| {
        if let EventMsg::PlanUpdate(plan) = event {
            replayed_plans.push(plan.clone());
        }
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    let final_response =
        responses::mount_sse_once(&server, responses::sse_completed("finished")).await;
    assert_eq!(
        thread.continue_turn().await?,
        StartIfIdleSubmission::Started {
            turn_id,
            root_turn_id,
        }
    );
    wait_for_event(&thread, |event| {
        if let EventMsg::PlanUpdate(plan) = event {
            replayed_plans.push(plan.clone());
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(
        replayed_plans.is_empty(),
        "saved plan must not execute again"
    );
    assert!(thread.continue_turn().await.is_err());
    for request in [
        continuation.single_request(),
        final_response.single_request(),
    ] {
        let user_input = request
            .input()
            .into_iter()
            .filter(|item| item["role"] == "user")
            .collect::<Vec<_>>();
        assert_eq!(
            user_input,
            original_input
                .iter()
                .filter(|item| item["role"] == "user")
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            request.function_call_output("saved-plan-call"),
            saved_output
        );
        assert!(!request.body_contains_text("<turn_aborted>"));
        assert!(!request.body_contains_text("generic.turn_aborted"));
    }
    assert_eq!(initial.requests().len(), 1);
    assert_eq!(blocked.requests().len(), 1);
    thread.shutdown_and_wait().await?;
    Ok(())
}
