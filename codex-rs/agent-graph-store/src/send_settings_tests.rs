use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::AgentGraphStore;
use crate::AgentSendMode;
use crate::AgentSendScope;
use crate::AgentSendSetting;
use crate::LocalAgentGraphStore;

#[tokio::test]
async fn send_settings_survive_reopen_with_disabled_and_scopes_isolated() {
    let home = TempDir::new().unwrap();
    let config = SqliteConfig::new_for_testing(home.path().abs());
    let sender = ThreadId::new();
    let receiver = ThreadId::new();
    let directed = AgentSendScope::Directed {
        sender_thread_id: sender,
        receiver_thread_id: receiver,
    };
    let reverse = AgentSendScope::Directed {
        sender_thread_id: receiver,
        receiver_thread_id: sender,
    };
    let subtree = AgentSendScope::Subtree {
        supervisor_thread_id: sender,
    };
    let absent = AgentSendScope::Subtree {
        supervisor_thread_id: receiver,
    };
    let expected = vec![
        AgentSendSetting {
            scope: directed,
            mode: AgentSendMode::Disabled,
            revision: 2,
        },
        AgentSendSetting {
            scope: subtree,
            mode: AgentSendMode::Enabled,
            revision: 1,
        },
        AgentSendSetting {
            scope: reverse,
            mode: AgentSendMode::Enabled,
            revision: 1,
        },
    ];
    {
        let state = StateRuntime::init(config.clone(), "test-provider".to_string())
            .await
            .unwrap();
        let store: Arc<dyn AgentGraphStore> = Arc::new(LocalAgentGraphStore::new(state));
        assert_eq!(
            store
                .read_agent_send_settings(vec![directed, subtree, absent])
                .await
                .unwrap(),
            Vec::<AgentSendSetting>::new()
        );
        assert_eq!(
            store
                .replace_agent_send_setting(directed, AgentSendMode::Enabled)
                .await
                .unwrap(),
            AgentSendSetting {
                scope: directed,
                mode: AgentSendMode::Enabled,
                revision: 1,
            }
        );
        for setting in &expected {
            assert_eq!(
                store
                    .replace_agent_send_setting(setting.scope, setting.mode)
                    .await
                    .unwrap(),
                *setting
            );
        }
        assert_eq!(
            store
                .read_agent_send_settings(vec![directed, absent, subtree, directed, reverse])
                .await
                .unwrap(),
            expected
        );
    }
    let state = StateRuntime::init(config, "test-provider".to_string())
        .await
        .unwrap();
    let store = LocalAgentGraphStore::new(state);
    assert_eq!(
        store
            .read_agent_send_settings(vec![directed, subtree, reverse, absent])
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        store
            .replace_agent_send_setting(directed, AgentSendMode::Disabled)
            .await
            .unwrap(),
        AgentSendSetting {
            scope: directed,
            mode: AgentSendMode::Disabled,
            revision: 3,
        }
    );
}

#[tokio::test]
async fn concurrent_setting_writes_return_distinct_revisions_and_last_commit_wins() {
    let home = TempDir::new().unwrap();
    let state = StateRuntime::init(
        SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .unwrap();
    let first = LocalAgentGraphStore::new(Arc::clone(&state));
    let second = LocalAgentGraphStore::new(state);
    let scope = AgentSendScope::Subtree {
        supervisor_thread_id: ThreadId::new(),
    };
    let (enabled, disabled) = tokio::join!(
        first.replace_agent_send_setting(scope, AgentSendMode::Enabled),
        second.replace_agent_send_setting(scope, AgentSendMode::Disabled)
    );
    let mut writes = [enabled.unwrap(), disabled.unwrap()];
    writes.sort_by_key(|setting| setting.revision);
    assert_eq!(
        writes
            .iter()
            .map(|setting| setting.revision)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        first.read_agent_send_settings(vec![scope]).await.unwrap(),
        vec![writes[1].clone()]
    );
}
