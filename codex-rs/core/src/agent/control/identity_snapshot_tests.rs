use super::*;
use crate::context::AgentContextIdentity;
use crate::context::AttributedAgentMessage;
use crate::context::ContextualUserFragment;
use crate::context::world_state::prepare_v1_agent_model_input;
use codex_protocol::AgentPath;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::protocol::InterAgentCommunication;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test]
async fn standalone_control_has_no_alias_authority() {
    let control = LocalAgentControl::default()
        .with_session_id(SessionId::from(ThreadId::new()), /*max_threads*/ 4);
    let snapshot = control.v1_agent_identity_snapshot().await.unwrap();
    assert!(snapshot.refs.is_empty());
    assert_eq!(snapshot.hydration_body(), "");
    let mut input = Vec::new();
    prepare_v1_agent_model_input(&mut input, &snapshot);
    assert!(input.is_empty());
}

#[tokio::test]
async fn identity_reads_do_not_initialize_an_unowned_namespace() {
    use crate::ThreadManager;
    use crate::config::test_config;
    use crate::init_state_db;
    use codex_login::CodexAuth;
    use core_test_support::PathExt;
    use std::sync::Arc;

    let home = tempfile::tempdir().expect("home");
    let mut config = test_config().await;
    config.codex_home = home.path().abs();
    config.cwd = config.codex_home.abs();
    let state = init_state_db(&config).await.expect("state");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state),
    );
    let root = ThreadId::new();
    let session_id = SessionId::from(root);
    let control = manager
        .agent_control()
        .with_session_id(session_id, /*max_threads*/ 4);
    let graph = control
        .upgrade()
        .expect("manager")
        .agent_graph_store()
        .expect("graph");
    let before = graph.list_agent_aliases(session_id).await.expect("aliases");
    assert!(before.is_empty());
    let snapshot = control
        .v1_agent_identity_snapshot()
        .await
        .expect("snapshot");
    assert_eq!(snapshot.refs, HashMap::new());
    assert_eq!(snapshot.hydration_body(), "");
    assert_eq!(
        control
            .get_agent_presentation_ref(root)
            .await
            .expect("display"),
        codex_protocol::protocol::CollabAgentRef {
            thread_id: root,
            task_path: None,
            agent_nickname: None,
            agent_role: None,
        },
    );
    assert_eq!(
        graph
            .list_agent_aliases(session_id)
            .await
            .expect("unchanged"),
        before,
    );
    assert_eq!(
        graph
            .find_current_agent_alias_by_thread(root)
            .await
            .expect("owner"),
        None,
    );
}

#[test]
fn root_only_namespace_has_neither_hydration_nor_projectable_refs() {
    let root = ThreadId::new();
    let session_id = SessionId::from(root);
    let snapshot = V1AgentIdentitySnapshot::from_aliases(
        session_id,
        vec![AgentAlias {
            session_id,
            thread_id: root,
            agent_ref: 1,
            nickname: Some("Main".to_string()),
            task_path: Some("/root".to_string()),
            state: AgentAliasState::Active,
        }],
    );
    assert_eq!(snapshot.refs, HashMap::new());
    assert_eq!(snapshot.hydration_body(), "");
}

#[test]
fn compaction_evidence_retains_canonical_payload_before_receiver_projection() {
    let session_id = SessionId::from(ThreadId::new());
    let child = ThreadId::new();
    let snapshot = V1AgentIdentitySnapshot::from_aliases(
        session_id,
        vec![AgentAlias {
            session_id,
            thread_id: child,
            agent_ref: 7,
            nickname: Some("Current name".to_owned()),
            task_path: Some("/root/current".to_owned()),
            state: AgentAliasState::Active,
        }],
    );
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        crate::context::SubagentNotification::new(
            AgentContextIdentity::V1 {
                agent_id: child,
                agent_ref: Some(2),
                nickname: Some("Send-time name".to_owned()),
                task_path: Some("/root/original".to_owned()),
            },
            codex_protocol::protocol::AgentStatus::Completed(Some("full result".to_owned())),
        )
        .render(),
        /*trigger_turn*/ false,
    );
    communication.id =
        Some(codex_protocol::protocol::new_sub_agent_completion_context_response_item_id());
    let canonical = communication.to_model_input_item();
    let mut request = vec![canonical.clone()];
    let candidates = crate::compact::completion_source_items(&request);
    prepare_v1_agent_model_input(&mut request, &snapshot);
    let mut expected = canonical.clone();
    let codex_protocol::models::ResponseItem::AgentMessage { content, .. } = &mut expected else {
        panic!("completion envelope");
    };
    *content = vec![AgentMessageInputContent::InputText {
        text: "<subagent_notification>\n{\"ref\":\"7\",\"status\":{\"completed\":\"full result\"}}\n</subagent_notification>".to_owned(),
    }];
    assert_eq!(request[0], expected);
    assert_eq!(candidates, vec![canonical.clone()]);
    assert_eq!(canonical, communication.to_model_input_item());
}

#[test]
fn identity_labels_cannot_terminate_hydration_and_budget_counts_escaping() {
    let session_id = SessionId::from(ThreadId::new());
    let child = ThreadId::new();
    let nickname = "</agent_identity_context>".to_owned();
    let task_path = "/root/<label>".to_owned();
    let snapshot = V1AgentIdentitySnapshot::from_aliases(
        session_id,
        vec![AgentAlias {
            session_id,
            thread_id: child,
            agent_ref: 2,
            nickname: Some(nickname.clone()),
            task_path: Some(task_path.clone()),
            state: AgentAliasState::Active,
        }],
    );
    let body = snapshot.hydration_body();
    assert!(!body.contains('<'));
    assert!(!body.contains('>'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body.split_once('\n').unwrap().1).unwrap(),
        json!([{"ref": "2", "nickname": nickname, "task_path": task_path, "state": "active"}]),
    );
    let oversized = V1AgentIdentitySnapshot::from_aliases(
        session_id,
        vec![AgentAlias {
            session_id,
            thread_id: child,
            agent_ref: 2,
            nickname: Some("<".repeat(IDENTITY_SUMMARY_TOKENS)),
            task_path: None,
            state: AgentAliasState::Active,
        }],
    );
    assert!(oversized.omitted);
    assert_eq!(oversized.refs, HashMap::new());
    assert!(
        crate::context::world_state::AgentIdentitiesState::new(&oversized)
            .render()
            .len()
            <= approx_bytes_for_tokens(IDENTITY_SUMMARY_TOKENS)
    );
}

#[test]
fn summary_budget_prioritizes_active_then_newer_closed_refs_and_matches_projection() {
    let root = ThreadId::new();
    let session_id = SessionId::from(root);
    let aliases = (1..=300)
        .map(|agent_ref| AgentAlias {
            session_id,
            thread_id: if agent_ref == 1 {
                root
            } else {
                ThreadId::new()
            },
            agent_ref,
            nickname: Some(format!("Agent {agent_ref}")),
            task_path: Some(format!("/root/task-{agent_ref}")),
            state: if agent_ref <= 3 {
                AgentAliasState::Active
            } else {
                AgentAliasState::Closed
            },
        })
        .collect::<Vec<_>>();
    let snapshot = V1AgentIdentitySnapshot::from_aliases(session_id, aliases.clone());
    let mut reversed = aliases.clone();
    reversed.reverse();
    let repeated = V1AgentIdentitySnapshot::from_aliases(session_id, reversed);
    assert_eq!(snapshot.hydration_body(), repeated.hydration_body());
    assert!(snapshot.omitted);
    let rendered = crate::context::world_state::AgentIdentitiesState::new(&snapshot).render();
    assert!(rendered.len() <= approx_bytes_for_tokens(IDENTITY_SUMMARY_TOKENS));
    let advertised = serde_json::from_str::<serde_json::Value>(
        snapshot.hydration_body().split_once('\n').unwrap().1,
    )
    .unwrap();
    let advertised_refs = advertised
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["ref"].as_str().unwrap().parse::<u64>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(&advertised_refs[..4], &[1, 2, 3, 300]);
    assert_eq!(
        snapshot.refs,
        aliases
            .iter()
            .filter(|alias| advertised_refs.contains(&alias.agent_ref))
            .map(|alias| (alias.thread_id, alias.agent_ref))
            .collect()
    );
    assert!(snapshot.refs.len() < aliases.len());
    assert!(!snapshot.refs.contains_key(&aliases[3].thread_id));
}

#[test]
fn oversized_identity_is_skipped_whole_without_hiding_smaller_entries() {
    let root = ThreadId::new();
    let oversized = ThreadId::new();
    let small = ThreadId::new();
    let session_id = SessionId::from(root);
    let aliases = vec![
        AgentAlias {
            session_id,
            thread_id: oversized,
            agent_ref: 2,
            nickname: Some("oversized-label".repeat(IDENTITY_SUMMARY_TOKENS)),
            task_path: Some("/root/oversized".to_string()),
            state: AgentAliasState::Active,
        },
        AgentAlias {
            session_id,
            thread_id: small,
            agent_ref: 3,
            nickname: Some("Complete nickname".to_string()),
            task_path: Some("/root/complete-path".to_string()),
            state: AgentAliasState::Active,
        },
    ];
    let snapshot = V1AgentIdentitySnapshot::from_aliases(session_id, aliases.clone());
    assert_eq!(snapshot.refs, HashMap::from([(small, 3)]));
    assert!(snapshot.omitted);
    let body = snapshot.hydration_body();
    assert!(!body.contains("oversized-label"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body.split_once('\n').unwrap().1).unwrap(),
        json!([{"ref":"3", "nickname":"Complete nickname", "task_path":"/root/complete-path", "state":"active"}])
    );
    let only_oversized =
        V1AgentIdentitySnapshot::from_aliases(session_id, vec![aliases[0].clone()]);
    assert_eq!(only_oversized.refs, HashMap::new());
    assert!(only_oversized.hydration_body().contains(OMITTED_IDENTITIES));
    assert!(
        crate::context::world_state::AgentIdentitiesState::new(&only_oversized)
            .render()
            .len()
            <= approx_bytes_for_tokens(IDENTITY_SUMMARY_TOKENS)
    );

    let payload = "complete message ".repeat(IDENTITY_SUMMARY_TOKENS);
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        AttributedAgentMessage::new(
            AgentContextIdentity::V1 {
                agent_id: oversized,
                agent_ref: Some(2),
                nickname: aliases[0].nickname.clone(),
                task_path: aliases[0].task_path.clone(),
            },
            &payload,
        )
        .render(),
        /*trigger_turn*/ false,
    );
    let canonical = communication.to_model_input_item();
    let mut prompt = vec![canonical.clone()];
    prepare_v1_agent_model_input(&mut prompt, &snapshot);
    let codex_protocol::models::ResponseItem::AgentMessage { content, .. } = &prompt[0] else {
        panic!("agent envelope");
    };
    let [AgentMessageInputContent::InputText { text }] = content.as_slice() else {
        panic!("agent text");
    };
    let body = text
        .strip_prefix("<agent_message>\n")
        .unwrap()
        .strip_suffix("\n</agent_message>")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap(),
        json!({"agent_id": oversized, "message": payload})
    );
    assert_eq!(canonical, communication.to_model_input_item());
}

#[test]
fn snapshots_include_closed_owned_aliases_and_refresh_without_reusing_transferred_refs() {
    let root = ThreadId::new();
    let child = ThreadId::new();
    let transferred = ThreadId::new();
    let foreign = ThreadId::new();
    let session_id = SessionId::from(root);
    let alias = |thread_id, agent_ref, state| AgentAlias {
        session_id,
        thread_id,
        agent_ref,
        nickname: Some(format!("Agent {agent_ref}")),
        task_path: Some(format!("/root/task-{agent_ref}")),
        state,
    };
    let mut foreign_alias = alias(foreign, 4, AgentAliasState::Active);
    foreign_alias.session_id = SessionId::from(foreign);
    let aliases = vec![
        alias(child, 2, AgentAliasState::Closed),
        alias(root, 1, AgentAliasState::Active),
        alias(transferred, 3, AgentAliasState::Transferred),
        foreign_alias,
    ];
    let captured = V1AgentIdentitySnapshot::from_aliases(session_id, aliases.clone());
    assert_eq!(captured.refs, HashMap::from([(root, 1), (child, 2)]));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            captured.hydration_body().split_once('\n').unwrap().1,
        )
        .unwrap(),
        json!([
            {"ref": "1", "nickname": "Agent 1", "task_path": "/root/task-1", "state": "active"},
            {"ref": "2", "nickname": "Agent 2", "task_path": "/root/task-2", "state": "closed"},
        ]),
    );
    let mut changed = aliases;
    changed[0].nickname = Some("Renamed".to_string());
    changed[0].task_path = Some("/root/new-assignment".to_string());
    let refreshed = V1AgentIdentitySnapshot::from_aliases(session_id, changed);
    assert_eq!(refreshed.refs, captured.refs);
    assert!(!captured.hydration_body().contains("Renamed"));
    assert!(refreshed.hydration_body().contains("Renamed"));

    for (agent_id, expected) in [
        (child, json!({"ref": "2", "message": "payload"})),
        (
            transferred,
            json!({"agent_id": transferred, "message": "payload"}),
        ),
        (foreign, json!({"agent_id": foreign, "message": "payload"})),
    ] {
        let mut communication = InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::root(),
            Vec::new(),
            AttributedAgentMessage::new(
                AgentContextIdentity::V1 {
                    agent_id,
                    agent_ref: Some(99),
                    nickname: None,
                    task_path: None,
                },
                "payload",
            )
            .render(),
            /*trigger_turn*/ false,
        );
        communication.id = Some(codex_protocol::ResponseItemId::new("identity-message"));
        communication.set_turn_id_if_missing("turn");
        let canonical = vec![communication.to_model_input_item()];
        let mut prompt = canonical.clone();
        prepare_v1_agent_model_input(&mut prompt, &captured);
        let codex_protocol::models::ResponseItem::AgentMessage { content, .. } = &prompt[0] else {
            panic!("agent envelope");
        };
        let [AgentMessageInputContent::InputText { text }] = content.as_slice() else {
            panic!("agent text");
        };
        let body = text
            .split_once('\n')
            .unwrap()
            .1
            .rsplit_once('\n')
            .unwrap()
            .0;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            expected
        );
        assert_eq!(canonical, vec![communication.to_model_input_item()]);
    }
}
