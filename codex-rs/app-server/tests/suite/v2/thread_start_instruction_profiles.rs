use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

use super::DEFAULT_READ_TIMEOUT;

#[tokio::test]
async fn shared_server_keeps_root_instruction_profiles_isolated() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let accounting_base = "Accounting root base: reconcile the supplied ledger.";
    let accounting_developer = "Accounting root developer: report reconciliation differences.";
    let research_base = "Research root base: investigate the supplied research question.";
    let research_developer = "Research root developer: distinguish evidence from inference.";
    let profiles = [
        (Some(accounting_base), Some(accounting_developer)),
        (Some(research_base), Some(research_developer)),
        (None, None),
    ];
    let mut threads = Vec::new();

    // Keep all roots loaded before any model request, including the ordinary start
    // whose omitted instruction fields must not borrow either earlier profile.
    for (base, developer) in profiles {
        let request_id = app_server
            .send_thread_start_request_with_auto_env(ThreadStartParams {
                base_instructions: base.map(str::to_string),
                developer_instructions: developer.map(str::to_string),
                ..Default::default()
            })
            .await?;
        let response: ThreadStartResponse =
            timeout(DEFAULT_READ_TIMEOUT, app_server.read_response(request_id)).await??;
        assert!(
            threads
                .iter()
                .all(|thread_id| thread_id != &response.thread.id),
            "each start must create a distinct root"
        );
        threads.push(response.thread.id);
    }

    // Revisit the accounting root after exercising both other roots to detect
    // mutation of an already-loaded thread as well as leaking launch defaults.
    for index in [0, 1, 2, 0] {
        let response_mock = responses::mount_sse_once(
            &server,
            create_final_assistant_message_sse_response("Done")?,
        )
        .await;
        timeout(
            DEFAULT_READ_TIMEOUT,
            app_server.start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: threads[index].clone(),
                input: vec![UserInput::Text {
                    text: "Describe your task.".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            }),
        )
        .await??;

        let request = response_mock.single_request();
        let base = request.instructions_text();
        let developer_texts = request.message_input_texts("developer");
        let (expected_base, expected_developer) = profiles[index];
        if let Some(expected_base) = expected_base {
            assert_eq!(base, expected_base);
        } else {
            assert!(
                !base.is_empty(),
                "ordinary roots retain default instructions"
            );
        }

        // Developer fragments share a message with server-owned guidance. Count
        // complete supplied strings within that bundle to also catch duplication.
        let profile_developer_counts =
            [accounting_developer, research_developer].map(|instruction| {
                developer_texts
                    .iter()
                    .map(|text| text.matches(instruction).count())
                    .sum::<usize>()
            });
        assert_eq!(
            profile_developer_counts,
            [
                usize::from(expected_developer == Some(accounting_developer)),
                usize::from(expected_developer == Some(research_developer)),
            ]
        );

        // Check the whole structured request too: a foreign profile must not
        // reappear in a different role or in the root's conversation history.
        for (other_index, (other_base, other_developer)) in profiles.iter().enumerate() {
            if other_index != index {
                for instruction in [other_base, other_developer].into_iter().flatten() {
                    assert!(
                        !request.body_contains_text(instruction),
                        "root {index} received root {other_index}'s instruction: {instruction}"
                    );
                }
            }
        }
    }
    Ok(())
}
