//! A history-forked role has the same instruction contract during sampling and compaction.

use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::TurnInputRequest;
use codex_core::UserAgentForkMode;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_core::config::AgentRoleConfig;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_login::CodexAuth;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::ThreadIdle;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use test_case::test_case;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(/*secs*/ 15);
const MODEL: &str = "gpt-5.6-sol";
const PARENT: &str = "Parent configuration: use the parent role, not the child role.";
const CHILD: &str = "Child configuration: preserve the child role during compaction.";
const MANAGED: &str = "Independent managed-policy fragment must remain unchanged.";
const CLIENT: &str = "Independent client-authored developer message must remain unchanged.";
const CONFIG_KIND: &str = "generic.developer_instructions";
const COMPACT_PROMPT: &str = "Produce the role-compaction regression checkpoint.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Compaction {
    Local,
    RemoteV2,
}

#[derive(Clone, Copy)]
enum Instructions {
    Replace,
    Add,
}

async fn finish(thread: &CodexThread, turn_id: &str) -> Result<()> {
    let completed = timeout(
        TIMEOUT,
        wait_for_event_match(thread, |event| match event {
            EventMsg::TurnComplete(event) if event.turn_id == turn_id => Some(event.clone()),
            _ => None,
        }),
    )
    .await
    .with_context(|| format!("completion of {turn_id}"))?;
    assert!(completed.error.is_none(), "turn failed: {completed:?}");
    ThreadIdle::wait(thread).await;
    Ok(())
}

async fn sample(thread: &CodexThread, prompt: &str) -> Result<()> {
    let submitted = thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let TurnInputSubmission::Started { turn_id } = submitted else {
        anyhow::bail!("expected a fresh turn for {prompt}: {submitted:?}");
    };
    finish(thread, &turn_id).await
}

async fn canonical_items(
    test: &TestCodex,
    thread: &CodexThread,
    thread_id: ThreadId,
) -> Result<Vec<RolloutItem>> {
    thread.flush_rollout().await?;
    Ok(test
        .thread_store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?
        .segments
        .into_iter()
        .flatten()
        .collect())
}

fn configuration_texts(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "developer")
        .flat_map(|item| {
            item["internal_chat_message_metadata_passthrough"]["content_item_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(_, kind)| **kind == CONFIG_KIND)
                .filter_map(move |(index, _)| item["content"][index]["text"].as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn preserved_developer_items(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .filter(|item| {
            item["role"] == "developer"
                && item["content"].as_array().is_some_and(|content| {
                    content
                        .iter()
                        .any(|fragment| fragment["text"] == MANAGED || fragment["text"] == CLIENT)
                })
        })
        .cloned()
        .collect()
}

#[test_case(Compaction::Local, Instructions::Replace, ThreadHistoryMode::Legacy; "local_replace_legacy")]
#[test_case(Compaction::Local, Instructions::Add, ThreadHistoryMode::Legacy; "local_add_legacy")]
#[test_case(Compaction::RemoteV2, Instructions::Replace, ThreadHistoryMode::Legacy; "remote_replace_legacy")]
#[test_case(Compaction::RemoteV2, Instructions::Add, ThreadHistoryMode::Legacy; "remote_add_legacy")]
#[test_case(Compaction::Local, Instructions::Replace, ThreadHistoryMode::Paginated; "local_replace_paginated")]
#[test_case(Compaction::Local, Instructions::Add, ThreadHistoryMode::Paginated; "local_add_paginated")]
#[test_case(Compaction::RemoteV2, Instructions::Replace, ThreadHistoryMode::Paginated; "remote_replace_paginated")]
#[test_case(Compaction::RemoteV2, Instructions::Add, ThreadHistoryMode::Paginated; "remote_add_paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_role_instructions_apply_to_compaction_without_rewriting_audit(
    compaction: Compaction,
    instructions: Instructions,
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let compact_output = match compaction {
        Compaction::Local => responses::ev_assistant_message("role-summary", "Child checkpoint."),
        Compaction::RemoteV2 => json!({
            "type": "response.output_item.done",
            "item": {"type": "compaction", "encrypted_content": "ROLE_CHECKPOINT"},
        }),
    };
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![responses::ev_completed("parent-seed")]),
            responses::sse(vec![responses::ev_completed("child-before")]),
            responses::sse(vec![
                compact_output,
                responses::ev_completed("child-compact"),
            ]),
            responses::sse(vec![responses::ev_completed("child-after")]),
        ],
    )
    .await;
    let inherited = match instructions {
        Instructions::Replace => Some(PARENT),
        Instructions::Add => None,
    };
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model(MODEL)
        .with_model_info_override(MODEL, |model| model.comp_hash = None)
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_history_mode(history_mode)
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            config.features.enable(Feature::Collab).expect("V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("not V2");
            config
                .features
                .disable(Feature::TokenBudget)
                .expect("standard compaction");
            config
                .features
                .enable(Feature::RetainClientDeveloperMessages)
                .expect("retain independent client messages");
            match compaction {
                Compaction::Local => config.features.disable(Feature::RemoteCompaction),
                Compaction::RemoteV2 => config.features.enable(Feature::RemoteCompaction),
            }
            .expect("select compaction implementation");
            config.developer_instructions = inherited.map(str::to_owned);
            config.compact_prompt = Some(COMPACT_PROMPT.into());
            config.remote_compaction_handoff_enabled = false;
            let role_path = config.codex_home.join("compaction-role.toml");
            std::fs::write(
                &role_path,
                toml::to_string(&json!({"developer_instructions": CHILD})).expect("role TOML"),
            )
            .expect("write isolated role configuration");
            config.agent_roles.insert(
                "compaction-role".into(),
                AgentRoleConfig {
                    description: Some("Role compaction regression".into()),
                    config_file: Some(role_path.to_path_buf()),
                    nickname_candidates: None,
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    sample(
        &test.codex,
        "seed the parent context before a role history fork",
    )
    .await?;
    let parent_expected = inherited.into_iter().map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(
        configuration_texts(&requests.requests()[0].input()),
        parent_expected
    );
    // Inject realistic independent fragment shapes through the existing client API. This
    // tests classification preservation, not managed-policy configuration or authority.
    test.codex
        .inject_response_items(vec![
            ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: "developer".into(),
                content: vec![ContentItem::InputText {
                    text: MANAGED.into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: Some(
                    InternalChatMessageMetadataPassthrough {
                        content_item_kinds: Some(vec![ContentItemKind(
                            "managed_config.developer_instructions".into(),
                        )]),
                        ..Default::default()
                    },
                ),
            },
            ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: "developer".into(),
                content: vec![ContentItem::InputText {
                    text: CLIENT.into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        ])
        .await?;
    let parent_id = test.session_configured.thread_id;
    let parent_before = canonical_items(&test, &test.codex, parent_id).await?;
    let spawned = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            role: Some("compaction-role".into()),
            fork_mode: UserAgentForkMode::All,
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?;
    let child_id = spawned.target_thread_id;
    let child = test.thread_manager.get_thread(child_id).await?;
    assert_eq!(child.config_snapshot().await.history_mode, history_mode);
    sample(&child, "sample with the inherited child's effective role").await?;
    let before_request = requests.requests()[1].clone();
    assert_eq!(
        before_request.header("thread-id"),
        Some(child_id.to_string())
    );
    let expected = vec![CHILD.to_owned()];
    assert_eq!(configuration_texts(&before_request.input()), expected);
    let independent = preserved_developer_items(&before_request.input());
    assert_eq!(independent.len(), 2);
    let child_before = canonical_items(&test, &child, child_id).await?;
    let original_input = child_before
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(serde_json::to_value(&item.item)),
            _ => None,
        })
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(configuration_texts(&original_input), parent_expected);

    child.submit(Op::Compact).await?;
    let started = timeout(
        TIMEOUT,
        wait_for_event_match(&child, |event| match event {
            EventMsg::ItemStarted(event)
                if matches!(&event.item, TurnItem::ContextCompaction(_)) =>
            {
                Some(event.clone())
            }
            _ => None,
        }),
    )
    .await
    .context("child compaction starts")?;
    let completed = timeout(
        TIMEOUT,
        wait_for_event_match(&child, |event| match event {
            EventMsg::ItemCompleted(event)
                if event.turn_id == started.turn_id && event.item.id() == started.item.id() =>
            {
                Some(event.clone())
            }
            _ => None,
        }),
    )
    .await
    .context("child compaction checkpoint completes")?;
    assert_eq!(completed.thread_id, child_id);
    finish(&child, &started.turn_id).await?;
    let compact_request = requests.requests()[2].clone();
    assert_eq!(
        compact_request.header("thread-id"),
        Some(child_id.to_string())
    );
    assert_eq!(configuration_texts(&compact_request.input()), expected);
    assert_eq!(
        preserved_developer_items(&compact_request.input()),
        independent
    );
    assert_eq!(
        compact_request.inputs_of_type("compaction_trigger").len(),
        usize::from(compaction == Compaction::RemoteV2)
    );
    assert_eq!(
        compact_request.body_contains_text(COMPACT_PROMPT),
        compaction == Compaction::Local
    );
    let child_after = canonical_items(&test, &child, child_id).await?;
    assert!(child_after.len() >= child_before.len());
    assert_eq!(
        serde_json::to_value(&child_after[..child_before.len()])?,
        serde_json::to_value(&child_before)?
    );
    assert!(
        child_after
            .iter()
            .any(|item| matches!(item, RolloutItem::Compacted(_)))
    );
    let parent_after = canonical_items(&test, &test.codex, parent_id).await?;
    assert!(parent_after.len() >= parent_before.len());
    assert_eq!(
        serde_json::to_value(&parent_after[..parent_before.len()])?,
        serde_json::to_value(&parent_before)?
    );

    sample(&child, "continue the child after the role-aware checkpoint").await?;
    let all = requests.requests();
    assert_eq!(all.len(), 4);
    assert_eq!(all[3].header("thread-id"), Some(child_id.to_string()));
    assert_eq!(configuration_texts(&all[3].input()), expected);
    child.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
