use super::CatalogSkillProvider;
use super::configure_catalog_test;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_skills_extension::SkillProviders;
use codex_skills_extension::SkillsExtensionConfig;
use codex_skills_extension::catalog::SkillAuthority;
use codex_skills_extension::catalog::SkillCatalog;
use codex_skills_extension::catalog::SkillCatalogEntry;
use codex_skills_extension::catalog::SkillPackageId;
use codex_skills_extension::catalog::SkillResourceId;
use codex_skills_extension::catalog::SkillSourceKind;
use codex_skills_extension::install_with_providers;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[derive(Clone, Copy)]
enum Mention {
    PlainText,
    Typed,
}

fn skill_input(mention: Mention) -> TurnInputRequest {
    let mut input = vec![UserInput::Text {
        text: "Use $unwrap.".to_string(),
        text_elements: Vec::new(),
    }];
    if matches!(mention, Mention::Typed) {
        input.push(UserInput::Skill {
            name: "unwrap".to_string(),
            path: "skill://ambient/unwrap/SKILL.md".into(),
        });
    }
    TurnInputRequest::user_input(input)
}

fn inventory_updates(request: &Value) -> Vec<String> {
    request["input"]
        .as_array()
        .expect("request input")
        .iter()
        .filter(|item| item["role"] == "developer")
        .flat_map(|item| item["content"].as_array().expect("message content"))
        .filter_map(|content| content["text"].as_str())
        .filter(|text| text.contains("<promoted_skills>[{"))
        .map(str::to_string)
        .collect()
}

#[test_case(Mention::PlainText; "plain dollar unwrap")]
#[test_case(Mention::Typed; "typed skill takes precedence")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_steer_promotes_before_sampling_and_survives_interrupt(
    mention: Mention,
) -> Result<()> {
    let (release_first, first_gate) = oneshot::channel();
    let (release_second, second_gate) = oneshot::channel();
    let (release_third, third_gate) = oneshot::channel();
    let (server, _completions) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("resp-1")]),
            },
            StreamingSseChunk {
                gate: Some(first_gate),
                body: responses::sse(vec![
                    responses::ev_function_call(
                        "plan-step",
                        "update_plan",
                        r#"{"plan":[{"step":"Inspect the task","status":"in_progress"}]}"#,
                    ),
                    responses::ev_completed("resp-1"),
                ]),
            },
        ],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("resp-2")]),
            },
            StreamingSseChunk {
                gate: Some(second_gate),
                body: responses::sse(vec![responses::ev_completed("resp-2")]),
            },
        ],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("resp-3")]),
            },
            StreamingSseChunk {
                gate: Some(third_gate),
                body: responses::sse(vec![responses::ev_completed("resp-3")]),
            },
        ],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("resp-4"),
                responses::ev_completed("resp-4"),
            ]),
        }],
    ])
    .await;
    let mut extensions = ExtensionRegistryBuilder::new();
    let catalog = |kind, authority, description| SkillCatalog {
        entries: vec![
            SkillCatalogEntry::new(
                SkillPackageId(format!("skill://{authority}/unwrap")),
                SkillAuthority::new(kind, authority),
                "unwrap",
                description,
                SkillResourceId::new(format!("skill://{authority}/unwrap/SKILL.md")),
            )
            // Typed skill selection resolves the display locator, independently of
            // the opaque package and resource identities asserted below.
            .with_display_path(format!("skill://{authority}/unwrap/SKILL.md"))
            .hidden_from_prompt(),
        ],
        warnings: Vec::new(),
    };
    install_with_providers(
        &mut extensions,
        SkillProviders::new()
            .with_host_provider(Arc::new(CatalogSkillProvider {
                catalog: catalog(SkillSourceKind::Host, "ambient", "Ambient host skill."),
            }))
            .with_executor_provider(Arc::new(CatalogSkillProvider {
                catalog: catalog(
                    SkillSourceKind::Executor,
                    "selected",
                    "Selected executor skill.",
                ),
            })),
        |config: &Config| SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: false,
            orchestrator_skills_enabled: false,
            shadow_selection_enabled: false,
        },
    );
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(configure_catalog_test)
        .build_with_streaming_server_auto_env(&server)
        .await?;
    let started = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Inspect the task.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = started else {
        panic!("first input must start a turn");
    };
    timeout(
        Duration::from_secs(10),
        server.wait_for_request_count(/*count*/ 1),
    )
    .await?;
    let first: Value = serde_json::from_slice(&server.requests().await[0])?;
    assert_eq!(inventory_updates(&first), Vec::<String>::new());
    assert!(!first.to_string().contains("- unwrap:"));

    assert_eq!(
        test.codex.start_or_steer_turn(skill_input(mention)).await?,
        TurnInputSubmission::Steered {
            turn_id: turn_id.clone(),
        },
    );
    release_first.send(()).expect("first response still gated");
    timeout(
        Duration::from_secs(10),
        server.wait_for_request_count(/*count*/ 2),
    )
    .await?;
    let second: Value = serde_json::from_slice(&server.requests().await[1])?;
    let promoted = inventory_updates(&second);
    assert_eq!(promoted.len(), 1);
    let (kind, authority, description, other_description) = match mention {
        Mention::PlainText => (
            "executor",
            "selected",
            "Selected executor skill.",
            "Ambient host skill.",
        ),
        Mention::Typed => (
            "host",
            "ambient",
            "Ambient host skill.",
            "Selected executor skill.",
        ),
    };
    assert!(promoted[0].contains(&format!("- unwrap: {description}")));
    assert!(!promoted[0].contains(other_description));
    assert!(promoted[0].contains(&format!("skill://{authority}/unwrap")));
    let identities = promoted[0]
        .split_once("<promoted_skills>")
        .expect("promotion marker")
        .1
        .split_once("</promoted_skills>")
        .expect("closing promotion marker")
        .0;
    let encode = |text: &str| {
        text.as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    assert_eq!(
        serde_json::from_str::<Value>(identities)?,
        json!([{
            "authorityKindHex": encode(kind),
            "authorityIdHex": encode(authority),
            "packageHex": encode(&format!("skill://{authority}/unwrap")),
            "resourceHex": encode(&format!("skill://{authority}/unwrap/SKILL.md")),
        }]),
    );
    let items = second["input"].as_array().expect("request input");
    let call = items
        .iter()
        .position(|item| item["type"] == "function_call" && item["call_id"] == "plan-step")
        .expect("initial tool call retained");
    let output = items
        .iter()
        .position(|item| item["type"] == "function_call_output" && item["call_id"] == "plan-step")
        .expect("initial tool result retained");
    let promotion = items
        .iter()
        .position(|item| item.to_string().contains("<promoted_skills>[{"))
        .expect("developer promotion retained");
    assert!(call < output && output < promotion);

    assert_eq!(
        test.codex.start_or_steer_turn(skill_input(mention)).await?,
        TurnInputSubmission::Steered {
            turn_id: turn_id.clone(),
        },
    );
    release_second
        .send(())
        .expect("second response still gated");
    timeout(
        Duration::from_secs(10),
        server.wait_for_request_count(/*count*/ 3),
    )
    .await?;
    let third: Value = serde_json::from_slice(&server.requests().await[2])?;
    assert_eq!(inventory_updates(&third), promoted);

    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    // Release the abandoned server stream only after interruption is observed.
    let _ = release_third.send(());
    let restarted = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Continue with the recorded inventory.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started {
        turn_id: restarted_id,
    } = restarted
    else {
        panic!("input after interruption must start a new turn");
    };
    assert_ne!(restarted_id, turn_id);
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let fourth: Value = serde_json::from_slice(&server.requests().await[3])?;
    assert_eq!(inventory_updates(&fourth), promoted);
    Ok(())
}
