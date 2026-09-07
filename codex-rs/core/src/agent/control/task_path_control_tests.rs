use super::*;
use crate::UserAgentResponseHandling;
use crate::UserAgentSpawnOptions;

#[tokio::test]
async fn nested_task_labels_and_selectors_use_the_issuing_threads_alias() {
    let harness = AgentControlHarness::new().await;
    let (root_id, root) = harness.start_thread().await;
    let backend = root
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("Backend/認証".to_string()),
            ..Default::default()
        })
        .await
        .expect("spawn labeled backend");
    let backend_thread = harness
        .manager
        .get_thread(backend.target_thread_id)
        .await
        .expect("backend is published");
    let review = backend_thread
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("Review".to_string()),
            ..Default::default()
        })
        .await
        .expect("spawn caller-relative reviewer");
    assert_eq!(
        (backend.task_path.as_deref(), review.task_path.as_deref()),
        (
            Some("/root/Backend/認証"),
            Some("/root/Backend/認証/Review")
        )
    );
    let control = &root.session.services.agent_control;
    for (caller, selector, expected) in [
        (root_id, "task:Backend/認証", backend.target_thread_id),
        (root_id, "Backend/認証/Review", review.target_thread_id),
        (
            backend.target_thread_id,
            "task:Review",
            review.target_thread_id,
        ),
        (
            backend.target_thread_id,
            "/root/Backend/認証/Review",
            review.target_thread_id,
        ),
        (review.target_thread_id, "/root", root_id),
    ] {
        assert_eq!(
            control
                .resolve_controlled_v1_agent_target(caller, selector)
                .await
                .expect("resolve task selector"),
            expected
        );
    }
    assert!(
        control
            .resolve_controlled_v1_agent_target(root_id, "task:Review")
            .await
            .is_err()
    );
    assert!(
        control
            .resolve_controlled_v1_agent_target(backend.target_thread_id, "nick:Review",)
            .await
            .is_err()
    );
    assert_eq!(
        backend_thread
            .resolve_user_agent_target("task:Review")
            .await
            .expect("human lookup uses issuing thread"),
        review.target_thread_id
    );
    assert_eq!(
        control
            .resolve_agent_task_path(backend.target_thread_id, "Review")
            .await
            .expect("directory prefix uses the same caller"),
        "/root/Backend/認証/Review"
    );
    root.close_agent("task:Backend/認証", UserAgentResponseHandling::Presentation)
        .await
        .expect("close subtree");
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn concurrent_duplicate_spawns_publish_one_member_and_closed_labels_remain_owned() {
    let harness = AgentControlHarness::new().await;
    let (root_id, root) = harness.start_thread().await;
    let options = UserAgentSpawnOptions {
        task: Some("backend".to_string()),
        ..Default::default()
    };
    let (first, second) = tokio::join!(
        root.spawn_agent(options.clone()),
        root.spawn_agent(options.clone()),
    );
    let (child, collision) = match (first, second) {
        (Ok(child), Err(collision)) | (Err(collision), Ok(child)) => (child, collision),
        outcomes => panic!("expected one atomic label owner, got {outcomes:?}"),
    };
    assert!(
        collision
            .to_string()
            .contains(&child.target_thread_id.to_string())
    );
    let control = &root.session.services.agent_control;
    let before_close = harness
        .manager
        .get_thread(child.target_thread_id)
        .await
        .expect("published winner")
        .config_snapshot()
        .await;
    let aliases = control
        .list_session_agent_aliases()
        .await
        .expect("list aliases");
    assert_eq!(
        aliases
            .iter()
            .filter(|alias| alias.task_path.as_deref() == Some("/root/backend"))
            .map(|alias| alias.thread_id)
            .collect::<Vec<_>>(),
        vec![child.target_thread_id]
    );
    root.close_agent("task:backend", UserAgentResponseHandling::Presentation)
        .await
        .expect("close label owner");
    let collision = root
        .spawn_agent(options)
        .await
        .expect_err("closed label still conflicts");
    assert!(collision.to_string().contains("resume the existing agent"));
    assert_eq!(
        control
            .resolve_resumable_v1_agent_target(root_id, "task:backend")
            .await
            .expect("closed label remains selectable"),
        child.target_thread_id
    );
    let resumed = root
        .resume_agent(
            "task:backend",
            /*task*/ None,
            UserAgentResponseHandling::Presentation,
        )
        .await
        .expect("resume existing owner");
    assert_eq!(
        (
            resumed.target_thread_id,
            resumed.agent_ref,
            resumed.task_path
        ),
        (
            child.target_thread_id,
            child.agent_ref,
            Some("/root/backend".to_string())
        )
    );
    let after_resume = harness
        .manager
        .get_thread(child.target_thread_id)
        .await
        .expect("resumed winner")
        .config_snapshot()
        .await;
    assert_eq!(
        (
            after_resume.model,
            after_resume.reasoning_effort,
            after_resume.service_tier
        ),
        (
            before_close.model,
            before_close.reasoning_effort,
            before_close.service_tier
        )
    );
    root.close_agent("task:backend", UserAgentResponseHandling::Presentation)
        .await
        .expect("close resumed child");
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn task_labels_do_not_override_nickname_priority_or_allow_same_root_resume_rename() {
    let harness = AgentControlHarness::new().await;
    let (root_id, root) = harness.start_thread().await;
    let reserved = root
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("/root".to_string()),
            ..Default::default()
        })
        .await
        .expect_err("only Main can own /root");
    assert!(reserved.to_string().contains("reserved for Main"));
    let original = root
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("spawn unlabeled agent");
    let nickname = original.nickname.as_ref().expect("assigned nickname");
    let labeled = root
        .spawn_agent(UserAgentSpawnOptions {
            task: Some(nickname.clone()),
            ..Default::default()
        })
        .await
        .expect("task label can share a nickname");
    let control = &root.session.services.agent_control;
    assert_eq!(
        control
            .resolve_agent_task_path(original.target_thread_id, "review")
            .await
            .expect("unlabeled callers resolve from root"),
        "/root/review"
    );
    assert_eq!(
        control
            .resolve_controlled_v1_agent_target(root_id, nickname)
            .await
            .expect("nickname keeps priority"),
        original.target_thread_id
    );
    assert_eq!(
        control
            .resolve_controlled_v1_agent_target(root_id, &format!("task:{nickname}"))
            .await
            .expect("explicit task selector bypasses nickname"),
        labeled.target_thread_id
    );
    let error = root
        .resume_agent(
            &labeled.target_thread_id.to_string(),
            Some("renamed".to_string()),
            UserAgentResponseHandling::Presentation,
        )
        .await
        .expect_err("same-root resume cannot edit labels");
    assert!(
        error
            .to_string()
            .contains("same-root resume does not rename")
    );
    assert_eq!(
        control
            .current_agent_alias(labeled.target_thread_id)
            .await
            .expect("load unchanged alias")
            .and_then(|alias| alias.task_path),
        Some(format!("/root/{nickname}"))
    );
    for child in [original.target_thread_id, labeled.target_thread_id] {
        root.close_agent(&child.to_string(), UserAgentResponseHandling::Presentation)
            .await
            .expect("close child");
    }
    root.shutdown_and_wait().await.expect("shutdown root");
}

#[tokio::test]
async fn adoption_assigns_a_caller_relative_task_and_reports_the_committed_mapping() {
    let harness = AgentControlHarness::new().await;
    let (old_root_id, old_root) = harness.start_thread().await;
    let imported = old_root
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("work".to_string()),
            ..Default::default()
        })
        .await
        .expect("spawn source assignment");
    let imported_thread = harness
        .manager
        .get_thread(imported.target_thread_id)
        .await
        .expect("imported child exists");
    persist_thread_for_tree_resume(&imported_thread, "retain original work").await;
    old_root
        .close_agent("task:work", UserAgentResponseHandling::Presentation)
        .await
        .expect("close source before adoption");

    let (_, new_root) = harness.start_thread().await;
    let backend = new_root
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("work".to_string()),
            ..Default::default()
        })
        .await
        .expect("another root can reuse the same task label");
    let backend_thread = harness
        .manager
        .get_thread(backend.target_thread_id)
        .await
        .expect("destination supervisor exists");
    let adopted = backend_thread
        .resume_agent(
            &imported.target_thread_id.to_string(),
            Some("Review".to_string()),
            UserAgentResponseHandling::Presentation,
        )
        .await
        .expect("adopt with a new caller-relative assignment");
    assert_eq!(
        (adopted.target_thread_id, adopted.task_path),
        (
            imported.target_thread_id,
            Some("/root/work/Review".to_string())
        )
    );
    assert_eq!(
        adopted
            .ownership_transfer
            .expect("ownership transferred")
            .task_path_mapping,
        vec![codex_agent_graph_store::AgentTaskPathMapping {
            thread_id: imported.target_thread_id,
            previous_task_path: Some("/root/work".to_string()),
            task_path: Some("/root/work/Review".to_string()),
        }]
    );
    assert!(
        old_root
            .session
            .services
            .agent_control
            .resolve_controlled_v1_agent_target(old_root_id, "task:work",)
            .await
            .is_err()
    );
    assert_eq!(
        backend_thread
            .resolve_user_agent_target("task:Review")
            .await
            .expect("current destination path is selectable"),
        imported.target_thread_id
    );
    new_root
        .close_agent("task:work", UserAgentResponseHandling::Presentation)
        .await
        .expect("close adopted subtree");
    old_root
        .shutdown_and_wait()
        .await
        .expect("shutdown former root");
    new_root
        .shutdown_and_wait()
        .await
        .expect("shutdown destination root");
}

#[tokio::test]
async fn user_control_audit_fills_current_labels_without_overwriting_supplied_snapshots() {
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    persist_thread_for_tree_resume(&root, "source audit history").await;
    let child = root
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("backend".to_string()),
            ..Default::default()
        })
        .await
        .expect("spawn labeled child");
    let mut expected = Vec::new();
    for task_path in [None, Some("/root/previous-assignment".to_string())] {
        let mut item = UserAgentControlItem::succeeded(UserAgentControlAction::Spawn);
        item.target_thread_id = Some(child.target_thread_id);
        item.task_path = task_path;
        root.record_user_agent_control(item.clone())
            .await
            .expect("record task attribution");
        if item.task_path.is_none() {
            item.task_path = child.task_path.clone();
        }
        expected.push(item);
    }
    let history = root
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .expect("read persisted audit")
        .history
        .expect("history included");
    let actual = history
        .items
        .into_iter()
        .filter_map(|item| {
            let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item else {
                return None;
            };
            let TurnItem::UserAgentControl(item) = event.item else {
                return None;
            };
            Some(item)
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    root.close_agent("task:backend", UserAgentResponseHandling::Presentation)
        .await
        .expect("close child");
    root.shutdown_and_wait().await.expect("shutdown root");
}
