//! Remote human review uses the controller approval flow and its deadline.

use super::*;
use core_test_support::skip_if_host_windows;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_no_remote_env;
use core_test_support::skip_if_sandbox;
use core_test_support::skip_if_target_windows;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(0; "immediate")]
#[test_case(100; "unanswered")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_human_network_review_uses_command_approval_deadline(
    approval_timeout_ms: u64,
) -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses the POSIX/Python network fixture");
    skip_if_host_windows!(Ok(()));
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let server = start_mock_server().await;
    let test =
        managed_network_unified_exec_builder(ManagedNetworkEnvironment::RemoteAndLocal, &[])?
            .with_config(move |config| config.approval_timeout_ms = Some(approval_timeout_ms))
            .build_with_remote_and_local_env(&server)
            .await?;
    let remote = test.executor_environment().selection().clone();
    let call_id = "remote-human-network-deadline";
    let responses = mount_exec_network_turn(
        &server,
        call_id,
        call_id,
        network_exec_args_for_environment(
            &remote_network_proxy_request_command("DEADLINE-NETWORK-RESULT"),
            REMOTE_ENVIRONMENT_ID,
        ),
    )
    .await?;
    let turn_id = submit_managed_network_turn(
        &test,
        "request network access without answering the approval",
        vec![remote],
        ApprovalsReviewer::User,
        AskForApproval::OnRequest,
    )
    .await?;
    if approval_timeout_ms > 0 {
        let approval =
            expect_network_approval_for_turn(&test, REMOTE_ENVIRONMENT_ID, &turn_id).await?;
        assert_eq!(
            approval.expires_at_ms,
            Some(approval.started_at_ms + i64::try_from(approval_timeout_ms)?),
        );
        // Deliberately leave the human request unanswered.
    }
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 15),
        wait_for_completion_without_network_prompt_for_turn(&test, &turn_id),
    )
    .await?;

    let output = responses
        .function_call_output_text(call_id)
        .context("expected the remote network deadline outcome")?;
    assert!(
        output.contains("command approval expired"),
        "expected the core-owned timeout rejection, got {output}",
    );
    Ok(())
}
