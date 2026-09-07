use super::*;
use crate::app::App;
use crate::app::agent_observation_display::AgentResponseObservationBinding;
use crate::render::renderable::Renderable;
use codex_app_server_protocol::AgentResponseHandling;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadStatusChangedNotification;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn activity_rows(app: &App) -> String {
    let width = 100;
    let area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        width,
        app.chat_widget.desired_height(width),
    );
    let mut buffer = Buffer::empty(area);
    app.chat_widget.render(area, &mut buffer);
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .filter(|line| line.contains("/agent to view"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn live_status_notifications_update_idle_footer_and_switches_rescope_it() {
    let mut app = crate::app::test_support::make_test_app().await;
    let root = ThreadId::new();
    let child = ThreadId::new();
    let second = ThreadId::new();
    let other_root = ThreadId::new();
    let other_child = ThreadId::new();
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    for (id, parent) in [
        (root, None),
        (second, Some(root)),
        (other_root, None),
        (other_child, Some(other_root)),
    ] {
        app.agent_navigation.upsert(
            id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
        app.agent_navigation.set_parent_thread_id(id, parent);
    }
    app.agent_navigation.note_response_observation(
        root,
        child,
        AgentResponseObservationBinding::NextTurn,
        Some(AgentResponseHandling::Wake),
    );
    app.agent_navigation.reserve_prompt_response(root, child);
    let startup_policy = app.agent_navigation.response_observation(root, child);
    app.enqueue_thread_notification(
        child,
        ServerNotification::ThreadStarted(codex_app_server_protocol::ThreadStartedNotification {
            thread: codex_app_server_protocol::Thread {
                id: child.to_string(),
                extra: None,
                session_id: root.to_string(),
                forked_from_id: None,
                parent_thread_id: Some(root.to_string()),
                preview: String::new(),
                ephemeral: false,
                section: None,
                section_entered_at: None,
                project_id: None,
                history_mode: Default::default(),
                model_provider: "test".to_string(),
                model: None,
                reasoning_effort: None,
                created_at: 0,
                updated_at: 0,
                recency_at: None,
                status: ThreadStatus::Active {
                    active_flags: Vec::new(),
                },
                path: None,
                cwd: app.config.cwd.clone(),
                cli_version: "test".to_string(),
                source: codex_app_server_protocol::SessionSource::Unknown,
                can_accept_direct_input: None,
                thread_source: None,
                agent_nickname: None,
                agent_role: None,
                git_info: None,
                name: None,
                turns: Vec::new(),
            },
        }),
    )
    .await
    .expect("startup status");
    assert_eq!(app.agent_navigation.parent_thread_id(child), Some(root));
    assert_eq!(
        (
            app.agent_navigation.response_observation(root, child),
            app.agent_navigation.reserved_prompt_source(child),
        ),
        (startup_policy, Some(root)),
        "startup liveness must not consume a future-turn policy"
    );
    insta::assert_snapshot!(activity_rows(&app), @"  1 agent running · /agent to view");
    for id in [child, second, other_child] {
        app.enqueue_thread_notification(
            id,
            ServerNotification::ThreadStatusChanged(ThreadStatusChangedNotification {
                thread_id: id.to_string(),
                status: ThreadStatus::Active {
                    active_flags: Vec::new(),
                },
            }),
        )
        .await
        .expect("live status");
    }
    assert!(!app.chat_widget.is_user_turn_pending_or_running());
    insta::assert_snapshot!(activity_rows(&app), @"  2 agents running · /agent to view");
    app.active_thread_id = Some(child);
    app.sync_active_agent_label();
    insta::assert_snapshot!(activity_rows(&app), @"  1 agent running · /agent to view");

    app.active_thread_id = Some(root);
    for (id, status) in [
        (child, ThreadStatus::Idle),
        (second, ThreadStatus::NotLoaded),
    ] {
        app.enqueue_thread_notification(
            id,
            ServerNotification::ThreadStatusChanged(ThreadStatusChangedNotification {
                thread_id: id.to_string(),
                status,
            }),
        )
        .await
        .expect("finish/unload");
    }
    insta::assert_snapshot!(activity_rows(&app), @"");
    app.agent_navigation
        .record_sub_agent_activity(crate::multi_agents::SubAgentActivityDisplay {
            thread_id: second,
            agent_path: "/root/second".to_string(),
            is_running_hint: true,
        });
    app.sync_active_agent_label();
    assert_eq!(
        activity_rows(&app),
        "",
        "late activity cannot revive an unloaded agent"
    );
    app.primary_thread_id = Some(other_root);
    app.active_thread_id = Some(other_root);
    app.sync_active_agent_label();
    insta::assert_snapshot!(activity_rows(&app), @"  1 agent running · /agent to view");
    app.mark_agent_picker_thread_closed(other_child);
    insta::assert_snapshot!(activity_rows(&app), @"");
    assert!(!app.chat_widget.is_user_turn_pending_or_running());
}

#[tokio::test]
async fn generic_status_preserves_bound_and_next_turn_policies_and_reserved_source() {
    let mut app = crate::app::test_support::make_test_app().await;
    let root = ThreadId::new();
    let child = ThreadId::new();
    let observer = ThreadId::new();
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.agent_navigation.upsert(
        child, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    app.agent_navigation.set_parent_thread_id(child, Some(root));
    app.agent_navigation.note_response_observation(
        root,
        child,
        AgentResponseObservationBinding::NextTurn,
        Some(AgentResponseHandling::Wake),
    );
    app.agent_navigation.note_response_observation(
        observer,
        child,
        AgentResponseObservationBinding::Bound,
        Some(AgentResponseHandling::new(
            /*commentary*/ false,
            codex_app_server_protocol::AgentFinalResponseHandling::Presentation,
            /*target_messages*/ false,
            /*queue_input*/ true,
        )),
    );
    app.agent_navigation.reserve_prompt_response(root, child);
    let expected = (
        app.agent_navigation.response_observation(root, child),
        app.agent_navigation.response_observation(observer, child),
        app.agent_navigation.reserved_prompt_source(child),
    );
    for status in [
        ThreadStatus::Active {
            active_flags: Vec::new(),
        },
        ThreadStatus::Idle,
        ThreadStatus::NotLoaded,
        ThreadStatus::Active {
            active_flags: Vec::new(),
        },
    ] {
        let running = matches!(status, ThreadStatus::Active { .. });
        app.enqueue_thread_notification(
            child,
            ServerNotification::ThreadStatusChanged(ThreadStatusChangedNotification {
                thread_id: child.to_string(),
                status,
            }),
        )
        .await
        .expect("generic status");
        assert_eq!(
            app.agent_navigation
                .running_agent_count(Some(root), Some(root)),
            usize::from(running)
        );
        assert_eq!(
            (
                app.agent_navigation.response_observation(root, child),
                app.agent_navigation.response_observation(observer, child),
                app.agent_navigation.reserved_prompt_source(child),
            ),
            expected,
            "visual liveness cannot bind NextTurn f or discard independent queued x"
        );
    }
}

#[test]
fn count_follows_lifecycle_current_thread_and_root_scope() {
    let mut state = AgentNavigationState::default();
    let root = ThreadId::new();
    let child = ThreadId::new();
    let grandchild = ThreadId::new();
    let other_root = ThreadId::new();
    let other_child = ThreadId::new();
    for (id, parent) in [
        (root, None),
        (child, Some(root)),
        (grandchild, Some(child)),
        (other_root, None),
        (other_child, Some(other_root)),
    ] {
        state.upsert(
            id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
        state.set_parent_thread_id(id, parent);
        state.mark_running(id);
    }
    assert_eq!(state.running_agent_count(Some(root), Some(root)), 2);
    assert_eq!(state.running_agent_count(Some(root), Some(child)), 1);
    assert_eq!(
        state.running_agent_count(Some(other_root), Some(other_root)),
        1
    );
    state.mark_stopped(child);
    assert_eq!(state.running_agent_count(Some(root), Some(root)), 1);
    state.mark_closed(grandchild);
    state.mark_running(grandchild);
    assert_eq!(state.running_agent_count(Some(root), Some(root)), 0);
    state.mark_running(child);
    assert_eq!(state.running_agent_count(Some(root), Some(root)), 1);
    state.remove(child);
    assert_eq!(state.running_agent_count(Some(root), Some(root)), 0);
    assert_eq!(state.running_agent_count(/*root*/ None, Some(root)), 0);
}

#[test]
fn active_root_aliases_supply_membership_before_parent_metadata_arrives() {
    let mut state = AgentNavigationState::default();
    let root = ThreadId::new();
    let child = ThreadId::new();
    for (id, agent_ref) in [(root, 1), (child, 2)] {
        state.upsert(
            id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
        );
        state.aliases.insert(
            id,
            super::super::AgentAliasEntry {
                agent_ref,
                nickname: None,
                task_path: None,
                state: AgentAliasState::Active,
            },
        );
        state.mark_running(id);
    }
    assert_eq!(state.running_agent_count(Some(root), Some(root)), 1);
    assert_eq!(
        state.running_agent_count(Some(ThreadId::new()), Some(root)),
        0
    );
    state.aliases.get_mut(&child).expect("alias").state = AgentAliasState::Transferred;
    assert_eq!(state.running_agent_count(Some(root), Some(root)), 0);
}
