use super::model_info_with_context_window;
use super::openai_model_provider;
use super::set_test_compact_prompt;
use anyhow::Result;
use codex_login::CodexAuth;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_previous_model_capacity_does_not_fall_back_on_downshift() -> Result<()> {
    // Do not install start_mock_server's competing empty model catalog.
    let server = MockServer::start().await;
    let previous_model = "gpt-5.6";
    let next_model = "gpt-5.5";
    responses::mount_models_once(
        &server,
        ModelsResponse {
            models: vec![
                model_info_with_context_window(previous_model, /*context_window*/ 273_000),
                model_info_with_context_window(next_model, /*context_window*/ 125_000),
            ],
        },
    )
    .await;
    let mut replies = vec![responses::sse_response(responses::sse(vec![
        responses::ev_assistant_message("before", "before switch"),
        responses::ev_completed_with_tokens("before", /*total_tokens*/ 120_000),
    ]))];
    // One initial compaction request and five retries, all on the previous model.
    replies.extend((0..6).map(|_| {
        ResponseTemplate::new(/*status*/ 503)
            .insert_header("Retry-After", "0")
            .set_body_json(json!({"error": {"code": "server_is_overloaded"}}))
    }));
    let requests = responses::mount_response_sequence(&server, replies).await;
    let model_provider = openai_model_provider(&server);
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model(previous_model)
        .with_config(move |config| {
            config.model_provider = model_provider;
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.request_max_retries = Some(0);
            set_test_compact_prompt(config);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("before switch").await?;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "after switch".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: next_model.to_string(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    let mut retries = 0;
    let mut errors = Vec::new();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
        loop {
            match test.codex.next_event().await?.msg {
                EventMsg::StreamError(error) => {
                    assert_eq!(
                        error.codex_error_info,
                        Some(CodexErrorInfo::ServerOverloaded)
                    );
                    retries += 1;
                }
                EventMsg::Error(error) => errors.push(error.codex_error_info),
                EventMsg::TurnComplete(_) => break,
                _ => {}
            }
        }
        anyhow::Ok(())
    })
    .await??;
    assert_eq!(
        (retries, errors),
        (5, vec![Some(CodexErrorInfo::ServerOverloaded)])
    );
    assert_eq!(
        requests
            .requests()
            .iter()
            .map(|request| request.body_json()["model"].clone())
            .collect::<Vec<_>>(),
        vec![json!(previous_model); 7]
    );
    // Include unmatched requests so a current-model fallback cannot escape the assertion.
    let all_requests = server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("mock server request recording must remain enabled"))?;
    assert_eq!(
        all_requests
            .iter()
            .filter(|request| request.url.path().ends_with("/responses"))
            .count(),
        7
    );
    Ok(())
}
