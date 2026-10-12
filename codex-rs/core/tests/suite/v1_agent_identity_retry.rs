use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::Match;
use wiremock::matchers::header;

fn identity_entries(request: &ResponsesRequest) -> Result<Vec<Value>> {
    let mut entries = Vec::new();
    for text in request.message_input_texts("developer") {
        let Some(body) = text.strip_prefix("<agent_identity_context>\n") else {
            continue;
        };
        let body = body
            .strip_suffix("\n</agent_identity_context>")
            .context("closed identity fragment")?;
        let value: Value =
            serde_json::from_str(body.lines().last().context("identity fragment payload")?)?;
        if let Some(full) = value.as_array() {
            entries = full.clone();
        } else {
            // This scenario only adds an agent: accept either a replacement authority
            // or its incremental equivalent, without pinning the serialized delta.
            for entry in value["upsert"].as_array().context("added identities")? {
                entries.retain(|existing| existing["ref"] != entry["ref"]);
                entries.push(entry.clone());
            }
        }
    }
    Ok(entries)
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_spawn_refreshes_receiving_root_identity(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
            config.model_provider.stream_max_retries = Some(1);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    let root_id = test.session_configured.thread_id.to_string();
    let child_root_id = root_id.clone();
    // The model tool requires nonempty input. Route child traffic independently,
    // so its scheduling cannot consume either of the parent's response fixtures.
    responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            !header("thread-id", child_root_id.as_str()).matches(request)
        },
        responses::sse(vec![responses::ev_completed("child-completed")]),
    )
    .await;
    let first_root_id = root_id.clone();
    let first = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            header("thread-id", first_root_id.as_str()).matches(request)
                && serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| {
                    body["input"].as_array().is_some_and(|input| {
                        !input
                            .iter()
                            .any(|item| item["call_id"] == "spawn-before-stream-error")
                    })
                })
        },
        // Intentionally close after the tool call, without response.completed.
        // The retry path must drain this spawn before rebuilding its request.
        responses::sse(vec![
            responses::ev_response_created("failed-parent-stream"),
            responses::ev_function_call_with_namespace(
                "spawn-before-stream-error",
                "multi_agent_v1",
                "spawn_agent",
                &json!({
                    "task": "retry-worker",
                    "message": "Finish without using tools.",
                    "w": "x"
                })
                .to_string(),
            ),
        ]),
    )
    .await;
    let retry = responses::mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            header("thread-id", root_id.as_str()).matches(request)
                && serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| {
                    body["input"].as_array().is_some_and(|input| {
                        input.iter().any(|item| {
                            item["type"] == "function_call_output"
                                && item["call_id"] == "spawn-before-stream-error"
                        })
                    })
                })
        },
        responses::sse(vec![
            responses::ev_assistant_message("retry-answer", "Spawn acknowledged."),
            responses::ev_completed("retried-parent-stream"),
        ]),
    )
    .await;

    test.submit_turn("Create a worker and acknowledge the result.")
        .await?;
    assert_eq!(
        identity_entries(&first.single_request())?,
        Vec::<Value>::new(),
        "a root-only namespace has no identity fragment",
    );
    let retried = retry.single_request();
    let receipt: Value = serde_json::from_str(
        &retried
            .function_call_output_text("spawn-before-stream-error")
            .context("retry includes the drained spawn receipt")?,
    )?;
    let child_ref = receipt["ref"].as_str().context("spawn assigned a ref")?;
    assert_ne!(child_ref, "1");
    let child_id: ThreadId = receipt["agent_id"]
        .as_str()
        .context("spawn receipt retains canonical UUID")?
        .parse()?;
    assert_ne!(child_id, test.session_configured.thread_id);
    assert_eq!(
        identity_entries(&retried)?,
        vec![
            json!({"ref":"1", "nickname":"Main", "task_path":"/root", "state":"active"}),
            json!({
                "ref": child_ref,
                "nickname": receipt["nickname"],
                "task_path": receipt["task_path"],
                "state": "active",
            }),
        ],
        "the retried request must resolve the ref in its freshly drained tool receipt",
    );

    test.codex.flush_rollout().await?;
    let history = test
        .thread_store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let canonical = history
        .segments
        .iter()
        .flatten()
        .find_map(|item| {
            let RolloutItem::ResponseItem(envelope) = item else {
                return None;
            };
            let ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } = &envelope.item
            else {
                return None;
            };
            (call_id.as_deref() == Some("spawn-before-stream-error"))
                .then(|| output.text_content())
                .flatten()
        })
        .context("canonical spawn receipt")?;
    assert_eq!(serde_json::from_str::<Value>(canonical)?, receipt);
    Ok(())
}
