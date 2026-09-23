//! Thread deletion never disposes of shared evidence without an ownership fence.

use chrono::DateTime;
use chrono::Utc;
use codex_agent_message_board_extension::LocalAgentMessageBoard;
use codex_agent_message_board_extension::MessageBoardHost;
use codex_agent_message_board_extension::NotificationDelivery;
use codex_agent_message_board_extension::PostContent;
use codex_agent_message_board_extension::PostDestination;
use codex_agent_message_board_extension::PostMetadata;
use codex_agent_message_board_extension::PostRequest;
use codex_agent_message_board_extension::ReadPostRequest;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_state::SqliteConfig;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

const DATABASE: &str = "agent_message_board_1.sqlite";
const WAIT_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);

#[derive(Default)]
struct WriteGate {
    pending: AtomicBool,
    arrived: Notify,
    release: Notify,
}

#[derive(Default)]
struct Host {
    gate: Option<Arc<WriteGate>>,
}

impl MessageBoardHost for Host {
    fn agent_path(&self, _caller: ThreadId) -> BoxFuture<'_, Result<AgentPath>> {
        Box::pin(async { Ok(AgentPath::root()) })
    }

    fn resolve_agent(&self, _path: AgentPath) -> BoxFuture<'_, Result<ThreadId>> {
        unreachable!("these posts have no explicitly notified recipients")
    }

    fn current_time(&self, _caller: ThreadId) -> BoxFuture<'_, Result<DateTime<Utc>>> {
        Box::pin(async {
            if let Some(gate) = &self.gate
                && gate.pending.swap(/*val*/ false, Ordering::SeqCst)
            {
                gate.arrived.notify_one();
                timeout(WAIT_TIMEOUT, gate.release.notified())
                    .await
                    .map_err(|error| CodexErr::Io(std::io::Error::other(error)))?;
            }
            Ok(Utc::now())
        })
    }

    fn notify(
        &self,
        _recipient: ThreadId,
        _post: PostMetadata,
    ) -> BoxFuture<'_, Result<NotificationDelivery>> {
        Box::pin(async { Ok(NotificationDelivery::SkippedInactive) })
    }
}

fn request(text: &str) -> PostRequest {
    PostRequest {
        request_id: text.into(),
        destination: PostDestination::NewChannel("audit".into()),
        text: text.into(),
        agents_to_notify: Vec::new(),
    }
}

async fn content(
    board: &LocalAgentMessageBoard,
    caller: ThreadId,
    post: &PostMetadata,
) -> Result<PostContent> {
    board
        .read_post(
            caller,
            ReadPostRequest {
                message_id: post.message_id,
                offset_chars: 0,
                limit_chars: NonZeroU32::MAX,
            },
        )
        .await
}

async fn counts(
    sqlite: &SqliteConfig,
    root: SessionId,
) -> std::result::Result<(i64, i64, i64, i64), sqlx::Error> {
    let pool = sqlite
        .open_read_only_pool(&sqlite.home().join(DATABASE), /*busy_timeout*/ None)
        .await?;
    let counts = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM channels WHERE board=?),
                (SELECT COUNT(*) FROM posts WHERE board=?),
                (SELECT COUNT(*) FROM subscriptions WHERE board=?),
                (SELECT COUNT(*) FROM deleted_boards WHERE board=?)",
    )
    .bind(root.to_string())
    .bind(root.to_string())
    .bind(root.to_string())
    .bind(root.to_string())
    .fetch_one(&pool)
    .await;
    pool.close().await;
    counts
}

#[tokio::test]
async fn preflight_without_state_db_preserves_remaining_board_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteConfig::new_for_testing(dir.path().to_path_buf().try_into().unwrap());
    let root = ThreadId::new();
    let board = LocalAgentMessageBoard::open(&sqlite, root.into(), Arc::new(Host::default()))
        .await
        .unwrap();
    let post = board.post(root, request("Evidence")).await.unwrap();
    let saved = content(&board, root, &post).await.unwrap();
    let pool = sqlite
        .open_read_write_pool(&dir.path().join(DATABASE))
        .await
        .unwrap();
    // No StateRuntime exists. Removing channel/post rows must not make remaining
    // evidence disposable; an empty channel list or missing graph is not proof.
    for (expected, remove) in [
        ((1, 1, 1, 0), "DELETE FROM channels"),
        ((0, 1, 1, 0), "DELETE FROM posts"),
        ((0, 0, 1, 0), "DELETE FROM subscriptions"),
    ] {
        assert!(
            LocalAgentMessageBoard::ensure_thread_deletion_preserves_boards(
                &sqlite,
                &[root.into()]
            )
            .await
            .is_err()
        );
        assert_eq!(counts(&sqlite, root.into()).await.unwrap(), expected);
        if expected.1 == 1 {
            assert_eq!(content(&board, root, &post).await.unwrap(), saved);
        }
        sqlx::query(remove).execute(&pool).await.unwrap();
    }
    LocalAgentMessageBoard::ensure_thread_deletion_preserves_boards(&sqlite, &[root.into()])
        .await
        .unwrap();
    assert_eq!(counts(&sqlite, root.into()).await.unwrap(), (0, 0, 0, 0));
    // Opening an older board for preflight must not install the tombstone schema.
    sqlx::query("DROP TABLE deleted_boards")
        .execute(&pool)
        .await
        .unwrap();
    LocalAgentMessageBoard::ensure_thread_deletion_preserves_boards(&sqlite, &[root.into()])
        .await
        .unwrap();
    let tombstone_table: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_schema WHERE name='deleted_boards'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tombstone_table, 0);
    pool.close().await;
}

#[tokio::test]
async fn missing_and_empty_preflight_allow_a_gated_late_write_without_tombstoning() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteConfig::new_for_testing(dir.path().to_path_buf().try_into().unwrap());
    let root = ThreadId::new();
    LocalAgentMessageBoard::ensure_thread_deletion_preserves_boards(&sqlite, &[root.into()])
        .await
        .unwrap();
    assert!(!dir.path().join(DATABASE).exists());

    let gate = Arc::new(WriteGate::default());
    let board = LocalAgentMessageBoard::open(
        &sqlite,
        root.into(),
        Arc::new(Host {
            gate: Some(Arc::clone(&gate)),
        }),
    )
    .await
    .unwrap();
    gate.pending.store(/*val*/ true, Ordering::SeqCst);
    let late_board = board.clone();
    let late = tokio::spawn(async move { late_board.post(root, request("Late evidence")).await });
    timeout(WAIT_TIMEOUT, gate.arrived.notified())
        .await
        .unwrap();
    LocalAgentMessageBoard::ensure_thread_deletion_preserves_boards(&sqlite, &[root.into()])
        .await
        .unwrap();
    assert_eq!(counts(&sqlite, root.into()).await.unwrap(), (0, 0, 0, 0));
    gate.release.notify_one();
    let post = timeout(WAIT_TIMEOUT, late).await.unwrap().unwrap().unwrap();
    assert_eq!(
        content(&board, root, &post).await.unwrap(),
        PostContent {
            metadata: post,
            text: "Late evidence".into(),
            n_chars: 13,
            next_offset_chars: 13,
        }
    );
    assert_eq!(counts(&sqlite, root.into()).await.unwrap(), (1, 1, 1, 0));
}

#[tokio::test]
async fn exclusive_cleanup_rejects_gated_late_writes_and_preserves_other_handles() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteConfig::new_for_testing(dir.path().to_path_buf().try_into().unwrap());
    let root = ThreadId::new();
    let other_root = ThreadId::new();
    let gate = Arc::new(WriteGate::default());
    let host = Arc::new(Host {
        gate: Some(Arc::clone(&gate)),
    });
    let board = LocalAgentMessageBoard::open(&sqlite, root.into(), host.clone())
        .await
        .unwrap();
    let other = LocalAgentMessageBoard::open(&sqlite, other_root.into(), Arc::new(Host::default()))
        .await
        .unwrap();
    board.post(root, request("A evidence")).await.unwrap();
    let other_post = other.post(other_root, request("B evidence")).await.unwrap();
    let saved = content(&other, other_root, &other_post).await.unwrap();

    // This fixture owns A exclusively; B is an unrelated board. The gate pauses
    // an already accepted A post before its write transaction, not via a timeout.
    gate.pending.store(/*val*/ true, Ordering::SeqCst);
    let late_board = board.clone();
    let late = tokio::spawn(async move { late_board.post(root, request("Late A")).await });
    timeout(WAIT_TIMEOUT, gate.arrived.notified())
        .await
        .unwrap();
    LocalAgentMessageBoard::delete_boards(&sqlite, &[root.into()])
        .await
        .unwrap();
    assert_eq!(counts(&sqlite, root.into()).await.unwrap(), (0, 0, 0, 1));
    assert_eq!(
        content(&other, other_root, &other_post).await.unwrap(),
        saved
    );
    assert_eq!(
        counts(&sqlite, other_root.into()).await.unwrap(),
        (1, 1, 1, 0)
    );
    gate.release.notify_one();
    let error = timeout(WAIT_TIMEOUT, late)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("permanently deleted"));

    let reopened = LocalAgentMessageBoard::open(&sqlite, root.into(), host)
        .await
        .unwrap();
    let error = reopened
        .post(root, request("Reopened A"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("permanently deleted"));
    LocalAgentMessageBoard::delete_boards(&sqlite, &[root.into()])
        .await
        .unwrap();
    assert_eq!(counts(&sqlite, root.into()).await.unwrap(), (0, 0, 0, 1));
    let new_post = other
        .post(
            other_root,
            PostRequest {
                destination: PostDestination::Channel("audit".into()),
                ..request("B still writable")
            },
        )
        .await
        .unwrap();
    assert_eq!(
        content(&other, other_root, &new_post).await.unwrap(),
        PostContent {
            metadata: new_post,
            text: "B still writable".into(),
            n_chars: 16,
            next_offset_chars: 16,
        }
    );
}

#[tokio::test]
async fn failed_storage_open_neither_recovers_nor_replaces_source_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteConfig::new_for_testing(dir.path().to_path_buf().try_into().unwrap());
    let root = SessionId::new();
    let database = dir.path().join(DATABASE);
    std::fs::create_dir(&database).unwrap();
    assert!(
        LocalAgentMessageBoard::ensure_thread_deletion_preserves_boards(&sqlite, &[root])
            .await
            .is_err()
    );
    assert!(database.is_dir());
    std::fs::remove_dir(&database).unwrap();
    let corrupt = b"not a SQLite database";
    std::fs::write(&database, corrupt).unwrap();
    let backup = dir.path().join("db-backups");
    std::fs::create_dir(&backup).unwrap();
    let recovery = backup.join("recovery-input");
    std::fs::write(&recovery, b"retain recovery evidence").unwrap();
    assert!(
        LocalAgentMessageBoard::ensure_thread_deletion_preserves_boards(&sqlite, &[root])
            .await
            .is_err()
    );
    assert!(
        LocalAgentMessageBoard::delete_boards(&sqlite, &[root])
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&database).unwrap(), corrupt);
    assert_eq!(
        std::fs::read(&recovery).unwrap(),
        b"retain recovery evidence"
    );
    assert_eq!(std::fs::read_dir(&backup).unwrap().count(), 1);
}
