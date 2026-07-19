//! Rebuilding a derived parent projection retains its captured ancestor read boundary.

use super::*;
use crate::local::rollout_lineage::AncestorReads;
use pretty_assertions::assert_eq;

pub(super) async fn frozen_parent_projection() -> (ProjectionFixture, AncestorReads) {
    let fixture = populated_projection().await;
    let base = prepare_paginated_fork(&fixture.store, fixture.thread_id, ForkBoundary::Latest)
        .await
        .history_base
        .expect("parent snapshot");
    let child_id = ThreadId::default();
    create_paginated_subagent_thread(
        &fixture.store,
        child_id,
        Some(base),
        /*subagent_history_start_ordinal*/ None,
    )
    .await;
    fixture
        .store
        .persist_thread(child_id, PersistContext::Standard)
        .await
        .expect("persist child reference");
    let lineage = fixture
        .store
        .resolve_rollout_lineage(child_id, /*initial_path*/ None)
        .await
        .expect("resolve frozen parent reader");
    assert_eq!(lineage.reads.cutoff(&fixture.rollout_path), Some(base));
    fixture
        .store
        .shutdown_thread(fixture.thread_id)
        .await
        .expect("close parent writer");
    (fixture, lineage.reads)
}

#[tokio::test]
async fn guarded_rebuild_keeps_parent_cutoff_and_preserves_projection_on_read_failure() {
    for compressed in [false, true] {
        let (fixture, reads) = frozen_parent_projection().await;
        let before = projection_rows(&fixture.pool, fixture.thread_id).await;
        let suffix = [
            rollout_line(Some(5), turn_started("later-turn")),
            rollout_line(
                Some(6),
                completed_item(
                    fixture.thread_id,
                    "later-turn",
                    agent_message("not-inherited", MessagePhase::FinalAnswer),
                ),
            ),
            rollout_line(Some(7), turn_completed("later-turn")),
        ]
        .join("\n");
        append_suffix(&fixture.rollout_path, &format!("{suffix}\n"));
        if compressed {
            compress_rollout(&fixture.rollout_path);
        }
        let physical = if compressed {
            fixture.rollout_path.with_extension("jsonl.zst")
        } else {
            fixture.rollout_path.clone()
        };
        let canonical = fs::read(&physical).expect("canonical parent bytes");
        super::super::rebuild_projection(
            &fixture.store,
            fixture.thread_id,
            super::super::ProjectionRollout {
                path: fixture.rollout_path.clone(),
                reads: reads.clone(),
            },
        )
        .await
        .expect("rebuild only frozen parent prefix");
        assert_eq!(
            projection_rows(&fixture.pool, fixture.thread_id).await,
            before
        );
        assert_eq!(fs::read(&physical).expect("parent audit bytes"), canonical);
        if compressed {
            assert!(!fixture.rollout_path.exists());
        }

        // A shortened captured source must fail before the derived transaction replaces rows.
        fs::write(&physical, &canonical[..canonical.len() / 2]).expect("shorten fixture source");
        super::super::rebuild_projection(
            &fixture.store,
            fixture.thread_id,
            super::super::ProjectionRollout {
                path: fixture.rollout_path.clone(),
                reads,
            },
        )
        .await
        .expect_err("captured ancestor source is incomplete");
        assert_eq!(
            projection_rows(&fixture.pool, fixture.thread_id).await,
            before
        );
    }
}
