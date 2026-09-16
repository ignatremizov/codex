//! The provisional owner retains exact runtime and graph-write outcomes through cleanup.

use super::*;
use crate::agent::control::aliases::PersistedAgentSpawn;
use crate::agent::control::spawn_guard::PendingSpawn;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tokio::time::timeout;

#[test_case::test_case(false; "acknowledged graph")]
#[test_case::test_case(true; "ambiguous graph acknowledgement")]
#[tokio::test]
async fn provisional_cleanup_joins_once_and_preserves_uncertain_history(ambiguous: bool) {
    let fixture = fixture().await;
    let state = fixture.owner.runtime.upgrade().expect("manager");
    let parent_lock = state.agent_lifecycle_lock(fixture.parent.thread_id);
    let parent_guard = Arc::clone(&parent_lock).lock_owned().await;
    fixture.child.thread.ensure_rollout_materialized().await;
    fixture
        .child
        .thread
        .flush_rollout()
        .await
        .expect("materialize child history");
    let mut pending = PendingSpawn::new(
        fixture.owner.clone(),
        Arc::clone(&state),
        Arc::clone(&fixture.child.thread),
        Some(parent_guard),
    );
    let graph = Arc::clone(&fixture.graph);
    let child = fixture.child.thread_id;
    pending.set_edge_write(tokio::spawn(async move {
        graph
            .set_thread_spawn_edge_status(child, ThreadSpawnEdgeStatus::Open)
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        if ambiguous {
            Err(CodexErr::Fatal("graph acknowledgement lost".into()))
        } else {
            Ok(PersistedAgentSpawn::default())
        }
    }));
    assert_eq!(pending.wait_for_edge().await.is_err(), ambiguous);
    assert!(Arc::clone(&parent_lock).try_lock_owned().is_err());
    let rejection = CodexErr::InvalidRequest("input was rejected before enqueue".into());
    let expected_error = rejection.to_string();
    let error = timeout(Duration::from_secs(/*secs*/ 5), pending.rollback(rejection))
        .await
        .expect("cleanup does not repoll an already completed graph worker");
    assert_eq!(error.to_string(), expected_error);
    assert!(Arc::clone(&parent_lock).try_lock_owned().is_ok());
    assert!(!fixture.child.thread.is_running());
    assert!(state.get_thread(child).await.is_err());
    assert!(Arc::ptr_eq(
        &state
            .get_thread(fixture.parent.thread_id)
            .await
            .expect("parent"),
        &fixture.parent.thread
    ));
    assert_eq!(
        fixture
            .graph
            .edges
            .lock()
            .expect("graph")
            .get(&child)
            .copied(),
        Some((
            fixture.parent.thread_id,
            if ambiguous {
                ThreadSpawnEdgeStatus::Open
            } else {
                ThreadSpawnEdgeStatus::Closed
            }
        ))
    );
    assert_eq!(
        fixture.graph.revocations.load(Ordering::Acquire),
        usize::from(!ambiguous),
        "only acknowledged graph cleanup revokes authority",
    );
    assert!(
        fixture
            .child
            .thread
            .read_thread(
                /*include_archived*/ true, /*include_history*/ false
            )
            .await
            .is_ok(),
        "both published and uncertain graph history remain available"
    );
    assert_eq!(
        fixture.store.calls().await.discard_thread,
        0,
        "a published edge must not be left referring to discarded history"
    );
    fixture
        .parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("stop parent");
}

#[tokio::test]
async fn dropping_provisional_setup_retains_parent_gate_until_edge_and_cleanup_finish() {
    let fixture = fixture().await;
    let state = fixture.owner.runtime.upgrade().expect("manager");
    let parent_lock = state.agent_lifecycle_lock(fixture.parent.thread_id);
    let mut pending = PendingSpawn::new(
        fixture.owner.clone(),
        Arc::clone(&state),
        Arc::clone(&fixture.child.thread),
        Some(Arc::clone(&parent_lock).lock_owned().await),
    );
    let (release, gate) = tokio::sync::oneshot::channel();
    let graph = Arc::clone(&fixture.graph);
    let child = fixture.child.thread_id;
    pending.set_edge_write(tokio::spawn(async move {
        gate.await.map_err(|_| CodexErr::InternalAgentDied)?;
        graph
            .set_thread_spawn_edge_status(child, ThreadSpawnEdgeStatus::Open)
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        Ok(PersistedAgentSpawn::default())
    }));
    drop(pending);
    assert!(Arc::clone(&parent_lock).try_lock_owned().is_err());
    release.send(()).expect("release original graph write");
    let _released = timeout(
        Duration::from_secs(/*secs*/ 5),
        Arc::clone(&parent_lock).lock_owned(),
    )
    .await
    .expect("cleanup retains and eventually releases the parent lifecycle gate");
    assert!(!fixture.child.thread.is_running());
    assert_eq!(
        fixture
            .graph
            .edges
            .lock()
            .expect("graph")
            .get(&child)
            .copied(),
        Some((fixture.parent.thread_id, ThreadSpawnEdgeStatus::Closed))
    );
    assert_eq!(fixture.graph.revocations.load(Ordering::Acquire), 1);
    assert!(state.get_thread(child).await.is_err());
    assert_eq!(fixture.store.calls().await.discard_thread, 0);
    fixture
        .parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("stop parent");
}

#[tokio::test]
async fn abandoned_input_attempt_keeps_the_exact_child_and_its_history() {
    let fixture = fixture().await;
    let state = fixture.owner.runtime.upgrade().expect("manager");
    fixture.child.thread.ensure_rollout_materialized().await;
    fixture
        .child
        .thread
        .flush_rollout()
        .await
        .expect("materialize child history");
    state
        .publish_restored_thread(&fixture.child.thread, Some(&fixture.parent.thread), || {
            Ok(())
        })
        .await
        .expect("publish exact provisional child");
    let mut pending = PendingSpawn::new(
        fixture.owner.clone(),
        Arc::clone(&state),
        Arc::clone(&fixture.child.thread),
        None,
    );
    pending.begin_input_attempt();
    drop(pending);
    let retained = state
        .get_thread(fixture.child.thread_id)
        .await
        .expect("uncertain child retained");
    assert!(Arc::ptr_eq(&retained, &fixture.child.thread));
    assert!(retained.session.submission_admission.requires_reload());
    assert!(
        retained
            .read_thread(
                /*include_archived*/ true, /*include_history*/ false
            )
            .await
            .is_ok()
    );
    assert_eq!(fixture.store.calls().await.discard_thread, 0);
    assert!(
        fixture
            .parent
            .thread
            .session
            .submission_admission
            .check_ready()
            .is_ok()
    );
    let _ = retained.shutdown_and_wait().await;
    fixture
        .parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("stop parent");
}

#[tokio::test]
async fn proven_rejection_before_graph_publication_can_discard_provisional_history() {
    let fixture = fixture().await;
    let state = fixture.owner.runtime.upgrade().expect("manager");
    fixture
        .graph
        .edges
        .lock()
        .expect("fresh provisional graph")
        .remove(&fixture.child.thread_id);
    let pending = PendingSpawn::new(
        fixture.owner.clone(),
        Arc::clone(&state),
        Arc::clone(&fixture.child.thread),
        None,
    );
    let _ = timeout(
        Duration::from_secs(/*secs*/ 5),
        pending.rollback(CodexErr::InvalidRequest(
            "setup rejected before graph or input publication".into(),
        )),
    )
    .await
    .expect("bounded provisional cleanup");
    assert_eq!(fixture.store.calls().await.discard_thread, 1);
    assert!(!fixture.child.thread.is_running());
    assert!(state.get_thread(fixture.child.thread_id).await.is_err());
    assert!(
        !fixture
            .graph
            .edges
            .lock()
            .expect("no published edge")
            .contains_key(&fixture.child.thread_id)
    );
    assert!(
        fixture
            .parent
            .thread
            .session
            .submission_admission
            .check_ready()
            .is_ok()
    );
    fixture
        .parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("stop parent");
}
