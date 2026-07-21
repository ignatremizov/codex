use codex_app_server_protocol::ContextCompactedNotification;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStatus;
use codex_core::config::ConfigBuilder;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use codex_utils_path_uri::PathUri;
use codex_utils_sandbox_summary::summarize_permission_profile;
use owo_colors::Style;
use pretty_assertions::assert_eq;

use super::EventProcessorWithHumanOutput;
use super::config_summary_entries;
use super::final_message_from_turn_items;
use super::reasoning_text;
use super::should_print_final_message_to_stdout;
use super::should_print_final_message_to_tty;
use crate::event_processor::CodexStatus;
use crate::event_processor::EventProcessor;

#[test]
fn compaction_preserves_complete_output_without_replacing_the_final_answer() {
    for show_compact_summary in [true, false] {
        let mut processor = EventProcessorWithHumanOutput {
            bold: Style::new(),
            cyan: Style::new(),
            dimmed: Style::new(),
            green: Style::new(),
            italic: Style::new(),
            magenta: Style::new(),
            red: Style::new(),
            yellow: Style::new(),
            show_agent_reasoning: true,
            show_raw_agent_reasoning: false,
            show_compact_summary,
            last_message_path: None,
            final_message: Some("final answer".to_string()),
            final_message_rendered: true,
            emit_final_message_on_shutdown: false,
            last_total_token_usage: None,
        };
        let message = "  leading spaces\n\nUnicode 界\ntrailing spaces  \n";
        assert_eq!(
            processor.compaction_section("Compacted prompt", message),
            "Compacted prompt\n    leading spaces\n  \n  Unicode 界\n  trailing spaces  \n  "
        );
        assert_eq!(
            processor.compaction_section("Compacted summary", "summary"),
            "Compacted summary\n  summary"
        );
        let error = "Decoder unavailable.\n\n  retained detail  ";
        let expected = if show_compact_summary {
            format!(
                "compacted prompt decoding failed: {error}\n{}",
                processor.compaction_section("Compacted prompt", message)
            )
        } else {
            format!("compacted prompt decoding failed: {error}")
        };
        assert_eq!(
            processor.compaction_output(Some("summary"), Some(message), Some(error)),
            expected,
        );
        assert_eq!(
            processor.compaction_output(/*summary*/ None, /*message*/ None, Some(" \n ")),
            "context compacted",
        );
        let completed = ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item: ThreadItem::ContextCompaction {
                id: "compact-1".to_string(),
                summary: Some("summary".to_string()),
                message: Some(message.to_string()),
                available_skills: vec!["test-tui".to_string()],
                decode_error: None,
            },
        });
        let compatibility = ServerNotification::ContextCompacted(ContextCompactedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            summary: Some("summary".to_string()),
            message: Some(message.to_string()),
            available_skills: vec!["test-tui".to_string()],
            decode_error: None,
        });
        assert_eq!(
            [
                processor.process_server_notification(completed),
                processor.process_server_notification(compatibility),
                processor.process_server_notification(ServerNotification::ContextCompactionStatus(
                    codex_app_server_protocol::ContextCompactionStatusNotification {
                        thread_id: "thread-1".into(),
                        turn_id: "turn-1".into(),
                        item_id: "compact-1".into(),
                        message: "Decoding".into(),
                    },
                )),
            ],
            [
                CodexStatus::Running,
                CodexStatus::Running,
                CodexStatus::Running
            ]
        );
        assert_eq!(
            (
                processor.final_message.as_deref(),
                processor.final_message_rendered,
                processor.emit_final_message_on_shutdown,
            ),
            (Some("final answer"), true, false)
        );
    }
}

#[test]
fn suppresses_final_stdout_message_when_both_streams_are_terminals() {
    assert!(!should_print_final_message_to_stdout(
        Some("hello"),
        /*stdout_is_terminal*/ true,
        /*stderr_is_terminal*/ true
    ));
}

#[test]
fn prints_final_stdout_message_when_stdout_is_not_terminal() {
    assert!(should_print_final_message_to_stdout(
        Some("hello"),
        /*stdout_is_terminal*/ false,
        /*stderr_is_terminal*/ true
    ));
}

#[test]
fn prints_final_stdout_message_when_stderr_is_not_terminal() {
    assert!(should_print_final_message_to_stdout(
        Some("hello"),
        /*stdout_is_terminal*/ true,
        /*stderr_is_terminal*/ false
    ));
}

#[test]
fn suppresses_final_stdout_message_when_missing() {
    assert!(!should_print_final_message_to_stdout(
        /*final_message*/ None, /*stdout_is_terminal*/ false,
        /*stderr_is_terminal*/ false
    ));
}

#[test]
fn prints_final_tty_message_when_not_yet_rendered() {
    assert!(should_print_final_message_to_tty(
        Some("hello"),
        /*final_message_rendered*/ false,
        /*stdout_is_terminal*/ true,
        /*stderr_is_terminal*/ true
    ));
}

#[test]
fn suppresses_final_tty_message_when_already_rendered() {
    assert!(!should_print_final_message_to_tty(
        Some("hello"),
        /*final_message_rendered*/ true,
        /*stdout_is_terminal*/ true,
        /*stderr_is_terminal*/ true
    ));
}

#[test]
fn reasoning_text_prefers_summary_when_raw_reasoning_is_hidden() {
    let text = reasoning_text(
        &["summary".to_string()],
        &["raw".to_string()],
        /*show_raw_agent_reasoning*/ false,
    );

    assert_eq!(text.as_deref(), Some("summary"));
}

#[test]
fn reasoning_text_uses_raw_content_when_enabled() {
    let text = reasoning_text(
        &["summary".to_string()],
        &["raw".to_string()],
        /*show_raw_agent_reasoning*/ true,
    );

    assert_eq!(text.as_deref(), Some("raw"));
}

#[test]
fn summarizes_disabled_permission_profile_as_danger_full_access() {
    let cwd = PathUri::from_abs_path(&test_path_buf("/tmp").abs());

    assert_eq!(
        summarize_permission_profile(
            &PermissionProfile::Disabled,
            &cwd,
            std::slice::from_ref(&cwd),
        ),
        "danger-full-access"
    );
}

#[test]
fn summarizes_external_permission_profile() {
    let cwd = PathUri::from_abs_path(&test_path_buf("/tmp").abs());

    assert_eq!(
        summarize_permission_profile(
            &PermissionProfile::External {
                network: NetworkSandboxPolicy::Enabled,
            },
            &cwd,
            std::slice::from_ref(&cwd),
        ),
        "external-sandbox (network access enabled)"
    );
}

#[test]
fn summarizes_managed_workspace_write_permission_profile() {
    let cwd = test_path_buf("/tmp/project").abs();
    let cache_root = test_path_buf("/tmp/cache").abs();
    let profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: cwd.clone().into(),
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: cache_root.clone().into(),
                },
                access: FileSystemAccessMode::Write,
                missing_path_behavior: None,
            },
        ]),
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        summarize_permission_profile(
            &profile,
            &PathUri::from_abs_path(&cwd),
            &[cwd.clone().into(), cache_root.clone().into()],
        ),
        format!("workspace-write [workdir, {}]", cache_root.display())
    );
}

#[test]
fn summarizes_managed_read_only_permission_profile() {
    let cwd = PathUri::from_abs_path(&test_path_buf("/tmp/project").abs());
    let profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::restricted(Vec::new()),
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        summarize_permission_profile(&profile, &cwd, std::slice::from_ref(&cwd)),
        "read-only"
    );
}

#[tokio::test]
async fn config_summary_entries_include_runtime_workspace_roots() {
    let codex_home = tempfile::tempdir().expect("create codex home");
    let cwd = tempfile::tempdir().expect("create cwd");
    let extra_root = tempfile::tempdir().expect("create extra root");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build default config");
    let cwd = cwd.path().to_path_buf().abs();
    let extra_root = extra_root.path().to_path_buf().abs();
    let expected_extra_root_name = extra_root
        .file_name()
        .expect("extra root should have file name")
        .to_string_lossy()
        .to_string();
    config.cwd = cwd.clone();
    config.workspace_roots = vec![cwd.clone(), extra_root];
    config
        .permissions
        .set_workspace_roots(config.workspace_roots.clone());
    config
        .permissions
        .set_permission_profile(PermissionProfile::workspace_write_with(
            &[],
            NetworkSandboxPolicy::Restricted,
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        ))
        .expect("set permission profile");

    let session_configured_event = SessionConfiguredEvent {
        session_id: SessionId::new(),
        thread_id: ThreadId::new(),
        forked_from_id: None,
        parent_thread_id: None,
        thread_source: None,
        thread_name: None,
        model: "gpt-5.4".to_string(),
        model_provider_id: config.model_provider_id.clone(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        approvals_reviewer: config.approvals_reviewer,
        permission_profile: config.permissions.effective_permission_profile(),
        active_permission_profile: None,
        cwd,
        reasoning_effort: None,
        initial_messages: None,
        network_proxy: None,
        rollout_path: None,
    };

    let summary_entries = config_summary_entries(&config, &session_configured_event);
    let sandbox_summary = summary_entries
        .iter()
        .find_map(|(key, value)| (*key == "sandbox").then_some(value))
        .expect("sandbox summary entry");
    assert!(
        sandbox_summary.starts_with("workspace-write [workdir, ")
            && sandbox_summary.contains(&expected_extra_root_name),
        "expected runtime workspace root in sandbox summary: {summary_entries:?}"
    );
}

#[test]
fn final_message_from_turn_items_uses_latest_agent_message() {
    let message = final_message_from_turn_items(&[
        ThreadItem::AgentMessage {
            id: "msg-1".to_string(),
            text: "first".to_string(),
            inter_agent_source: None,
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
        ThreadItem::Plan {
            id: "plan-1".to_string(),
            text: "plan".to_string(),
        },
        ThreadItem::AgentMessage {
            id: "msg-2".to_string(),
            text: "second".to_string(),
            inter_agent_source: None,
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
    ]);

    assert_eq!(message.as_deref(), Some("second"));
}

#[test]
fn final_message_from_turn_items_ignores_inter_agent_messages() {
    let message = final_message_from_turn_items(&[
        ThreadItem::AgentMessage {
            id: "msg-1".to_string(),
            text: "ordinary answer".to_string(),
            inter_agent_source: None,
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
        ThreadItem::AgentMessage {
            id: "msg-2".to_string(),
            text: "worker transcript".to_string(),
            inter_agent_source: Some(codex_app_server_protocol::InterAgentMessageSource {
                author: "/root".to_string(),
                recipient: "/root/worker".to_string(),
            }),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
    ]);

    assert_eq!(message.as_deref(), Some("ordinary answer"));
}

#[test]
fn inter_agent_item_does_not_overwrite_rendered_final_message() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        show_compact_summary: true,
        last_message_path: None,
        final_message: Some("ordinary answer".to_string()),
        final_message_rendered: true,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
    };

    processor.render_item_completed(ThreadItem::AgentMessage {
        id: "msg-2".to_string(),
        text: "worker transcript".to_string(),
        inter_agent_source: Some(codex_app_server_protocol::InterAgentMessageSource {
            author: "/root".to_string(),
            recipient: "/root/worker".to_string(),
        }),
        phase: None,
        memory_citation: None,
        delivery: None,
        questions: None,
    });

    assert_eq!(processor.final_message.as_deref(), Some("ordinary answer"));
    assert!(processor.final_message_rendered);
}

#[test]
fn final_message_from_turn_items_falls_back_to_latest_plan() {
    let message = final_message_from_turn_items(&[
        ThreadItem::Reasoning {
            id: "reasoning-1".to_string(),
            summary: vec!["inspect".to_string()],
            content: Vec::new(),
        },
        ThreadItem::Plan {
            id: "plan-1".to_string(),
            text: "first plan".to_string(),
        },
        ThreadItem::Plan {
            id: "plan-2".to_string(),
            text: "final plan".to_string(),
        },
    ]);

    assert_eq!(message.as_deref(), Some("final plan"));
}

#[test]
fn turn_completed_recovers_final_message_from_turn_items() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        show_compact_summary: true,
        last_message_path: None,
        final_message: None,
        final_message_rendered: false,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![ThreadItem::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "final answer".to_string(),
                    inter_agent_source: None,
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                }],
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message.as_deref(), Some("final answer"));
}

#[test]
fn turn_completed_overwrites_stale_final_message_from_turn_items() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        show_compact_summary: true,
        last_message_path: None,
        final_message: Some("stale answer".to_string()),
        final_message_rendered: true,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: vec![ThreadItem::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "final answer".to_string(),
                    inter_agent_source: None,
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                }],
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message.as_deref(), Some("final answer"));
    assert!(!processor.final_message_rendered);
}

#[test]
fn turn_completed_preserves_streamed_final_message_when_turn_items_are_empty() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        show_compact_summary: true,
        last_message_path: None,
        final_message: Some("streamed answer".to_string()),
        final_message_rendered: false,
        emit_final_message_on_shutdown: false,
        last_total_token_usage: None,
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message.as_deref(), Some("streamed answer"));
    assert!(processor.emit_final_message_on_shutdown);
}

#[test]
fn turn_failed_clears_stale_final_message() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        show_compact_summary: true,
        last_message_path: None,
        final_message: Some("partial answer".to_string()),
        final_message_rendered: true,
        emit_final_message_on_shutdown: true,
        last_total_token_usage: None,
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Failed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message, None);
    assert!(!processor.final_message_rendered);
    assert!(!processor.emit_final_message_on_shutdown);
}

#[test]
fn turn_interrupted_clears_stale_final_message() {
    let mut processor = EventProcessorWithHumanOutput {
        bold: Style::new(),
        cyan: Style::new(),
        dimmed: Style::new(),
        green: Style::new(),
        italic: Style::new(),
        magenta: Style::new(),
        red: Style::new(),
        yellow: Style::new(),
        show_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        show_compact_summary: true,
        last_message_path: None,
        final_message: Some("partial answer".to_string()),
        final_message_rendered: true,
        emit_final_message_on_shutdown: true,
        last_total_token_usage: None,
    };

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Interrupted,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(
        status,
        crate::event_processor::CodexStatus::InitiateShutdown
    );
    assert_eq!(processor.final_message, None);
    assert!(!processor.final_message_rendered);
    assert!(!processor.emit_final_message_on_shutdown);
}
