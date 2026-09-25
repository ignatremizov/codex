use codex_app_server_protocol::InterAgentMessageSource;
use codex_app_server_protocol::ThreadTimelineEntry;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use pretty_assertions::assert_eq;

use super::*;
use crate::ItemSortKey;
use crate::ListItemsParams;
use crate::ListTimelineParams;
use crate::SearchThreadOccurrencesParams;

fn transcript(suffix: &str, turn_id: &str, text: &str) -> RolloutItem {
    let mut item = ResponseItem::AgentMessage {
        id: Some(ResponseItemId::with_suffix("amsg", suffix)),
        author: "/root".to_string(),
        recipient: "/root/worker".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: text.to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    item.set_turn_id_if_missing(turn_id);
    RolloutItem::ResponseItem(item.into())
}

async fn transcript_projection() -> ProjectionFixture {
    let fixture = populated_projection().await;
    fixture
        .store
        .append_items(AppendThreadItemsParams {
            thread_id: fixture.thread_id,
            items: vec![transcript("mail", "mail-turn", "needle-delivery")],
        })
        .await
        .expect("append canonical transcript");
    fixture
        .store
        .shutdown_thread(fixture.thread_id)
        .await
        .expect("close writer");
    fixture
}

async fn erase_transcript_projection(pool: &sqlx::SqlitePool, thread_id: ThreadId) {
    for query in [
        "DELETE FROM thread_items WHERE thread_id = ? AND item_id = 'amsg_mail'",
        "DELETE FROM thread_turns WHERE thread_id = ? AND turn_id = 'mail-turn'",
        "DELETE FROM fork_thread_history_projection_state WHERE thread_id = ?",
    ] {
        sqlx::query(query)
            .bind(thread_id.to_string())
            .execute(pool)
            .await
            .expect("simulate upstream-only projection");
    }
}

async fn complete_projection_rows(
    pool: &sqlx::SqlitePool,
    thread_id: ThreadId,
) -> Vec<Vec<String>> {
    let mut rows = projection_rows(pool, thread_id).await;
    rows.push(
        sqlx::query_scalar(
            "SELECT json_array(thread_id, next_rollout_byte_offset, next_rollout_ordinal, projection_version) \
             FROM fork_thread_history_projection_state WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_all(pool)
        .await
        .expect("fork projection checkpoint"),
    );
    rows
}

fn item_params(thread_id: ThreadId) -> ListItemsParams {
    ListItemsParams {
        thread_id,
        turn_id: None,
        include_archived: false,
        cursor: None,
        page_size: 100,
        sort_direction: SortDirection::Asc,
        sort_key: ItemSortKey::CreatedAtOrdinal,
        after_updated_at_ordinal: None,
    }
}

#[tokio::test]
async fn missing_or_stale_fork_checkpoint_rebuilds_at_plain_and_compressed_eof() {
    for representation in ["plain", "compressed"] {
        for checkpoint in ["missing", "stale_offset", "stale_ordinal"] {
            let fixture = transcript_projection().await;
            let expected = complete_projection_rows(&fixture.pool, fixture.thread_id).await;
            let upstream = projection_state(&fixture.pool, fixture.thread_id).await;
            erase_transcript_projection(&fixture.pool, fixture.thread_id).await;
            if checkpoint != "missing" {
                let stale = match checkpoint {
                    "stale_offset" => (upstream.0 - 1, upstream.1),
                    "stale_ordinal" => (upstream.0, upstream.1 - 1),
                    _ => unreachable!("checkpoint case"),
                };
                sqlx::query(
                    "INSERT INTO fork_thread_history_projection_state \
                     (thread_id, next_rollout_byte_offset, next_rollout_ordinal) VALUES (?, ?, ?)",
                )
                .bind(fixture.thread_id.to_string())
                .bind(stale.0)
                .bind(stale.1)
                .execute(&fixture.pool)
                .await
                .expect("stale fork checkpoint");
            }
            let physical_path = if representation == "compressed" {
                compress_rollout(&fixture.rollout_path);
                fixture.rollout_path.with_extension("jsonl.zst")
            } else {
                fixture.rollout_path.clone()
            };
            let canonical = fs::read(&physical_path).expect("canonical bytes");

            super::super::materialize_to_sqlite(
                &fixture.store,
                fixture.thread_id,
                &fixture.rollout_path,
            )
            .await
            .expect("upgrade projection at EOF");

            assert_eq!(
                complete_projection_rows(&fixture.pool, fixture.thread_id).await,
                expected,
            );
            assert_eq!(
                fs::read(&physical_path).expect("canonical unchanged"),
                canonical
            );
            assert_eq!(fixture.rollout_path.exists(), representation == "plain");
            // A current marker takes the real EOF fast path, not another replacement.
            sqlx::query(
                "CREATE TRIGGER reject_replay BEFORE DELETE ON thread_items \
                 BEGIN SELECT RAISE(ABORT, 'unexpected replay'); END",
            )
            .execute(&fixture.pool)
            .await
            .expect("detect redundant replay");
            super::super::materialize_to_sqlite(
                &fixture.store,
                fixture.thread_id,
                &fixture.rollout_path,
            )
            .await
            .expect("current EOF is a no-op");
        }
    }
}

#[tokio::test]
async fn stale_fork_checkpoint_rebuilds_old_prefix_before_projecting_new_suffix() {
    let fixture = transcript_projection().await;
    let (_, ordinal) = projection_state(&fixture.pool, fixture.thread_id).await;
    erase_transcript_projection(&fixture.pool, fixture.thread_id).await;
    let suffix = rollout_line(
        Some(u64::try_from(ordinal).expect("ordinal")),
        transcript("suffix", "suffix-turn", "later delivery"),
    );
    append_suffix(&fixture.rollout_path, &format!("{suffix}\n"));

    let page = fixture
        .store
        .list_items(item_params(fixture.thread_id))
        .await
        .expect("refresh old prefix and suffix");

    assert_eq!(
        page.items
            .iter()
            .map(|item| (item.turn_id.as_str(), item.item_id.as_str()))
            .collect::<Vec<_>>(),
        [
            ("original-turn", "original-agent"),
            ("mail-turn", "amsg_mail"),
            ("suffix-turn", "amsg_suffix"),
        ],
    );
}

#[tokio::test]
async fn replay_preserves_canonical_identity_time_and_opaque_transcript_content() {
    let fixture = transcript_projection().await;
    let (_, next_ordinal) = projection_state(&fixture.pool, fixture.thread_id).await;
    let ordinal = u64::try_from(next_ordinal).expect("ordinal");
    let mut response = ResponseItem::AgentMessage {
        id: Some(ResponseItemId::with_suffix("amsg", "private")),
        author: "/root".to_string(),
        recipient: "/root/worker".to_string(),
        content: vec![
            AgentMessageInputContent::InputText {
                text: "audit secret".to_string(),
            },
            AgentMessageInputContent::EncryptedContent {
                encrypted_content: "ciphertext".to_string(),
            },
        ],
        internal_chat_message_metadata_passthrough: None,
    };
    response.set_turn_id_if_missing("private-turn");
    let line = rollout_line(Some(ordinal), RolloutItem::ResponseItem(response.into()));
    append_suffix(&fixture.rollout_path, &format!("{line}\n"));

    let page = fixture
        .store
        .list_items(item_params(fixture.thread_id))
        .await
        .expect("replay");
    let stored = page
        .items
        .into_iter()
        .find(|item| item.item_id == "amsg_private")
        .expect("opaque transcript");
    let item: ThreadItem = serde_json::from_slice(&stored.item_json).expect("typed transcript");

    assert_eq!(
        (
            stored.turn_id,
            stored.item_id,
            stored.updated_at_ordinal,
            stored.created_at_ms,
            item
        ),
        (
            "private-turn".to_string(),
            "amsg_private".to_string(),
            ordinal,
            1_735_689_600_000,
            ThreadItem::AgentMessage {
                id: "amsg_private".to_string(),
                text: "Agent message from `/root`:\n\nInput message encrypted".to_string(),
                inter_agent_source: Some(InterAgentMessageSource {
                    author: "/root".to_string(),
                    recipient: "/root/worker".to_string(),
                }),
                attribution: None,
                input: None,
                phase: Some(MessagePhase::Commentary),
                memory_citation: None,
                delivery: None,
                questions: None,
            },
        ),
    );
}

#[tokio::test]
async fn failed_upgrade_keeps_prior_rows_realtime_and_both_checkpoints() {
    let fixture = transcript_projection().await;
    sqlx::query(
        "UPDATE fork_thread_history_projection_state SET next_rollout_ordinal = 0 \
         WHERE thread_id = ?",
    )
    .bind(fixture.thread_id.to_string())
    .execute(&fixture.pool)
    .await
    .expect("stale checkpoint");
    let before = complete_projection_rows(&fixture.pool, fixture.thread_id).await;
    sqlx::query(
        "CREATE TRIGGER reject_upgrade BEFORE INSERT ON thread_items \
         WHEN NEW.item_id = 'amsg_mail' BEGIN SELECT RAISE(ABORT, 'upgrade failure'); END",
    )
    .execute(&fixture.pool)
    .await
    .expect("fail inside replacement transaction");
    super::super::materialize_to_sqlite(&fixture.store, fixture.thread_id, &fixture.rollout_path)
        .await
        .expect_err("transaction fails after replacement begins");
    assert_eq!(
        complete_projection_rows(&fixture.pool, fixture.thread_id).await,
        before,
    );
    sqlx::query("DROP TRIGGER reject_upgrade")
        .execute(&fixture.pool)
        .await
        .expect("remove failure");
    let moved = fixture.rollout_path.with_extension("unavailable");
    fs::rename(&fixture.rollout_path, &moved).expect("make canonical read fail");
    super::super::materialize_to_sqlite(&fixture.store, fixture.thread_id, &fixture.rollout_path)
        .await
        .expect_err("failed source read");
    assert_eq!(
        complete_projection_rows(&fixture.pool, fixture.thread_id).await,
        before,
    );
    fs::rename(moved, &fixture.rollout_path).expect("restore source");
    super::super::materialize_to_sqlite(&fixture.store, fixture.thread_id, &fixture.rollout_path)
        .await
        .expect("retry complete upgrade");
    assert!(
        super::super::super::thread_history::fork_projection_is_current(
            &fixture.store,
            fixture.thread_id,
            u64::try_from(projection_state(&fixture.pool, fixture.thread_id).await.0)
                .expect("byte offset"),
            u64::try_from(projection_state(&fixture.pool, fixture.thread_id).await.1)
                .expect("ordinal"),
        )
        .await
        .expect("matching checkpoints")
    );
}

#[tokio::test]
async fn every_paginated_read_upgrades_visible_ancestor_transcripts() {
    for representation in ["plain", "compressed"] {
        for surface in ["items", "turns", "timeline", "search"] {
            let fixture = transcript_projection().await;
            let base =
                prepare_paginated_fork(&fixture.store, fixture.thread_id, ForkBoundary::Latest)
                    .await
                    .history_base
                    .expect("source prefix");
            let child = ThreadId::new();
            create_paginated_subagent_thread(
                &fixture.store,
                child,
                Some(base),
                /*subagent_history_start_ordinal*/ None,
            )
            .await;
            fixture
                .store
                .persist_thread(child, PersistContext::Standard)
                .await
                .expect("persist child");
            fixture
                .store
                .shutdown_thread(child)
                .await
                .expect("close child");
            let expected = complete_projection_rows(&fixture.pool, fixture.thread_id).await;
            erase_transcript_projection(&fixture.pool, fixture.thread_id).await;
            if representation == "compressed" {
                compress_rollout(&fixture.rollout_path);
            }

            match surface {
                "items" => {
                    let page = fixture
                        .store
                        .list_items(item_params(child))
                        .await
                        .expect("items");
                    assert!(page.items.iter().any(|item| item.item_id == "amsg_mail"));
                }
                "turns" => {
                    let page = fixture
                        .store
                        .list_turns(ListTurnsParams {
                            thread_id: child,
                            include_archived: false,
                            cursor: None,
                            page_size: 100,
                            sort_direction: SortDirection::Asc,
                            items_view: StoredTurnItemsView::NotLoaded,
                        })
                        .await
                        .expect("turns");
                    assert!(page.turns.iter().any(|turn| turn.turn_id == "mail-turn"));
                }
                "timeline" => {
                    let page = fixture
                        .store
                        .list_timeline(ListTimelineParams {
                            thread_id: child,
                            cursor: None,
                            page_size: 100,
                        })
                        .await
                        .expect("timeline");
                    assert!(page.items.iter().any(|entry| matches!(
                        entry, ThreadTimelineEntry::Item { item, .. } if item.id() == "amsg_mail"
                    )));
                }
                "search" => {
                    let page = fixture
                        .store
                        .search_thread_occurrences(SearchThreadOccurrencesParams {
                            thread_id: child,
                            search_term: "needle-delivery".to_string(),
                            cursor: None,
                            page_size: 100,
                        })
                        .await
                        .expect("search");
                    assert_eq!(
                        page.items
                            .iter()
                            .map(|item| item.item_id.as_str())
                            .collect::<Vec<_>>(),
                        ["amsg_mail"],
                    );
                }
                _ => unreachable!("read surface"),
            }
            assert_eq!(
                complete_projection_rows(&fixture.pool, fixture.thread_id).await,
                expected,
            );
            assert_eq!(fixture.rollout_path.exists(), representation == "plain");
        }
    }
}

#[tokio::test]
async fn transcript_turn_placeholder_yields_to_lifecycle_but_never_downgrades_real_status() {
    let home = TempDir::new().expect("home");
    let store = projection_store(home.path()).await;
    let thread = ThreadId::new();
    create_paginated_thread(&store, thread).await;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: thread,
            items: vec![transcript("early", "mail-turn", "before lifecycle")],
        })
        .await
        .expect("item without lifecycle");
    let pool = store.thread_history_db().await.expect("history database");
    assert_eq!(
        sqlx::query_as::<_, (String, Option<i64>)>(
            "SELECT status, rollout_end_ordinal FROM thread_turns \
             WHERE thread_id = ? AND turn_id = 'mail-turn'",
        )
        .bind(thread.to_string())
        .fetch_one(pool)
        .await
        .expect("synthetic turn"),
        ("completed".to_string(), None),
    );
    store
        .append_items(AppendThreadItemsParams {
            thread_id: thread,
            items: vec![turn_started("mail-turn")],
        })
        .await
        .expect("canonical lifecycle replaces placeholder");
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM thread_turns WHERE thread_id = ? AND turn_id = 'mail-turn'",
        )
        .bind(thread.to_string())
        .fetch_one(pool)
        .await
        .expect("canonical status"),
        "inProgress",
    );
    for status in ["inProgress", "failed", "interrupted", "completed"] {
        sqlx::query(
            "UPDATE thread_turns SET status = ?, rollout_end_ordinal = \
             CASE WHEN ? = 'inProgress' THEN NULL ELSE 2 END WHERE thread_id = ?",
        )
        .bind(status)
        .bind(status)
        .bind(thread.to_string())
        .execute(pool)
        .await
        .expect("real turn state");
        let before = projection_rows(pool, thread).await[0].clone();
        store
            .append_items(AppendThreadItemsParams {
                thread_id: thread,
                items: vec![
                    transcript(status, "mail-turn", "another delivery"),
                    turn_started("mail-turn"),
                ],
            })
            .await
            .expect("delivery preserves status");
        assert_eq!(projection_rows(pool, thread).await[0], before);
    }
    store.shutdown_thread(thread).await.expect("close writer");
}

#[tokio::test]
async fn deleting_projection_removes_fork_marker_in_the_same_transaction() {
    let fixture = transcript_projection().await;
    let before = complete_projection_rows(&fixture.pool, fixture.thread_id).await;
    sqlx::query(
        "CREATE TRIGGER reject_marker_delete BEFORE DELETE ON fork_thread_history_projection_state \
         BEGIN SELECT RAISE(ABORT, 'marker delete failure'); END",
    )
    .execute(&fixture.pool)
    .await
    .expect("fail marker deletion");
    super::super::super::thread_history::delete_thread(&fixture.store, fixture.thread_id)
        .await
        .expect_err("projection cleanup failure");
    assert_eq!(
        complete_projection_rows(&fixture.pool, fixture.thread_id).await,
        before
    );
    sqlx::query("DROP TRIGGER reject_marker_delete")
        .execute(&fixture.pool)
        .await
        .expect("allow cleanup");
    fixture
        .store
        .delete_thread(DeleteThreadParams {
            thread_id: fixture.thread_id,
        })
        .await
        .expect("authorized thread deletion");
    assert_eq!(
        complete_projection_rows(&fixture.pool, fixture.thread_id).await,
        vec![Vec::<String>::new(); 5],
    );
}

#[tokio::test]
async fn cancelled_read_releases_lifecycle_reservations_without_recursive_writer_locking() {
    let fixture = transcript_projection().await;
    let writer = fixture
        .store
        .live_writer_locks
        .lock(fixture.thread_id)
        .await;
    let coordination = fixture
        .store
        .live_writer_locks
        .coordination(fixture.thread_id)
        .await;
    let store = fixture.store.clone();
    let thread = fixture.thread_id;
    let read = tokio::spawn(async move { store.list_items(item_params(thread)).await });
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        while coordination.lifecycle.try_write().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("read reserved lifecycle before waiting for writer");
    read.abort();
    assert!(read.await.expect_err("cancel read").is_cancelled());
    assert!(coordination.lifecycle.try_write().is_ok());
    drop(writer);
    fixture
        .store
        .list_items(item_params(thread))
        .await
        .expect("later read succeeds");
}
