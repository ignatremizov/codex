use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use crate::AgentAliasAllocation;
use crate::AgentAliasTransferRequest;
use crate::StateRuntime;
use crate::runtime::test_support::unique_temp_dir;

#[tokio::test]
async fn failed_transfer_rolls_back_task_labels_ownership_refs_and_audit_together() {
    let codex_home = unique_temp_dir();
    let _cleanup = scopeguard::guard(codex_home.clone(), |codex_home| {
        let _ = std::fs::remove_dir_all(codex_home);
    });
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("database");
    let source = ThreadId::from_string("00000000-0000-0000-0000-000000002800").expect("source");
    let destination =
        ThreadId::from_string("00000000-0000-0000-0000-000000002810").expect("destination");
    let target = ThreadId::from_string("00000000-0000-0000-0000-000000002801").expect("target");
    let child = ThreadId::from_string("00000000-0000-0000-0000-000000002802").expect("child");
    let incumbent =
        ThreadId::from_string("00000000-0000-0000-0000-000000002811").expect("incumbent");
    for (root, parent, child, task_path) in [
        (source, source, target, "/root/backend"),
        (source, target, child, "/root/backend/review"),
        (destination, destination, incumbent, "/root/backend"),
    ] {
        runtime
            .allocate_agent_alias(AgentAliasAllocation {
                session_id: root.into(),
                parent_thread_id: parent,
                child_thread_id: child,
                nickname: None,
                task_path: Some(task_path.to_string()),
            })
            .await
            .expect("initial assignment");
    }
    let source_before = runtime
        .list_agent_aliases(source.into())
        .await
        .expect("source");
    let destination_before = runtime
        .list_agent_aliases(destination.into())
        .await
        .expect("destination");
    sqlx::query(
        r#"
CREATE TRIGGER fail_descendant_transfer BEFORE INSERT ON agent_alias_transfers
WHEN NEW.thread_id = '00000000-0000-0000-0000-000000002802'
BEGIN
    SELECT RAISE(ABORT, 'injected descendant audit failure');
END
        "#,
    )
    .execute(runtime.pool.as_ref())
    .await
    .expect("inject failure after target publication inside the transaction");
    let request = AgentAliasTransferRequest {
        expected_previous_session_id: Some(source.into()),
        expected_descendant_thread_ids: vec![child],
        new_session_id: destination.into(),
        new_parent_thread_id: destination,
        thread_id: target,
        nickname: None,
        task_path: None,
        authored_selector: target.to_string(),
    };
    runtime
        .transfer_agent_alias(request.clone())
        .await
        .expect_err("late transfer failure");
    assert_eq!(
        runtime
            .list_agent_aliases(source.into())
            .await
            .expect("source"),
        source_before,
    );
    assert_eq!(
        runtime
            .list_agent_aliases(destination.into())
            .await
            .expect("destination"),
        destination_before,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_alias_transfers")
            .fetch_one(runtime.pool.as_ref())
            .await
            .expect("audit rows"),
        0,
    );
    sqlx::query("DROP TRIGGER fail_descendant_transfer")
        .execute(runtime.pool.as_ref())
        .await
        .expect("remove injected failure");
    let result = runtime
        .transfer_agent_alias(request)
        .await
        .expect("retry commits labels and ownership");
    let crate::AgentAliasTransfer::Transferred { alias, .. } = result else {
        panic!("ownership must transfer");
    };
    assert_eq!(
        (alias.agent_ref, alias.task_path),
        (3, Some("/root/backend-2".to_string())),
    );
}
