//! Cover complete row bindings through both metadata insertion paths and their conflict rules.

use super::StateRuntime;
use crate::SqliteConfig;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

enum WriteKind {
    InsertIfAbsent,
    Upsert,
}

#[tokio::test]
async fn metadata_writes_round_trip_full_rows_and_preserve_conflict_semantics() -> anyhow::Result<()>
{
    for kind in [WriteKind::InsertIfAbsent, WriteKind::Upsert] {
        for daybreak_enabled in [None, Some(false), Some(true)] {
            let home = unique_temp_dir();
            let runtime = StateRuntime::init(
                SqliteConfig::new_for_testing(home.as_path().abs()),
                "test-provider".to_string(),
            )
            .await?;
            let mut metadata = test_thread_metadata(&home, ThreadId::new(), home.clone());
            metadata.originator = Some("metadata-write-test".to_string());
            metadata.history_mode = ThreadHistoryMode::Paginated;
            metadata.agent_nickname = Some("worker-name".to_string());
            metadata.agent_role = Some("reviewer".to_string());
            metadata.agent_path = Some("/root/worker".to_string());
            metadata.service_tier = Some("priority".to_string());
            metadata.title = "rollout title".to_string();
            metadata.name = Some("chosen name".to_string());
            metadata.tokens_used = 123;
            metadata.git_sha = Some("test-commit".to_string());
            metadata.git_branch = Some("test-branch".to_string());
            metadata.daybreak_enabled = daybreak_enabled;

            match kind {
                WriteKind::InsertIfAbsent => {
                    assert!(runtime.insert_thread_if_absent(&metadata).await?);
                }
                WriteKind::Upsert => runtime.upsert_thread(&metadata).await?,
            }
            assert_eq!(
                runtime.get_thread(metadata.id).await?,
                Some(metadata.clone())
            );

            let mut replacement = metadata.clone();
            replacement.updated_at += chrono::Duration::milliseconds(10);
            replacement.title = "updated rollout title".to_string();
            replacement.tokens_used = 456;
            replacement.name = Some("stale rollout name".to_string());
            replacement.daybreak_enabled = Some(!daybreak_enabled.unwrap_or(false));
            match kind {
                WriteKind::InsertIfAbsent => {
                    assert!(!runtime.insert_thread_if_absent(&replacement).await?);
                }
                WriteKind::Upsert => {
                    runtime.upsert_thread(&replacement).await?;
                    metadata.updated_at = replacement.updated_at;
                    metadata.title = replacement.title;
                    metadata.tokens_used = replacement.tokens_used;
                }
            }
            // Upserts update rollout fields, but neither path overwrites user-owned choices.
            assert_eq!(runtime.get_thread(metadata.id).await?, Some(metadata));
        }
    }
    Ok(())
}
