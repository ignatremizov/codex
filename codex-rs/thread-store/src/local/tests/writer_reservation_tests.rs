use super::*;

#[tokio::test]
async fn reservation_deduplicates_ids_and_excludes_another_store() {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let store = LocalThreadStore::new(config.clone(), /*state_db*/ None);
    let other = LocalThreadStore::new(config, /*state_db*/ None);
    let id = ThreadId::new();
    let reservation = store.reserve_thread_writers(vec![id, id]).await.unwrap();
    assert!(other.acquire_writer_lock(id).is_err());
    drop(reservation);
    assert!(other.acquire_writer_lock(id).is_ok());
}

#[tokio::test]
async fn failed_reservation_releases_only_newly_acquired_locks() {
    let home = TempDir::new().unwrap();
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let first = ThreadId::from_string("00000000-0000-0000-0000-000000000001").unwrap();
    let second = ThreadId::from_string("00000000-0000-0000-0000-000000000002").unwrap();
    let existing = store.acquire_writer_lock(second).unwrap();
    assert!(
        store
            .reserve_thread_writers(vec![second, first])
            .await
            .is_err()
    );
    assert!(store.acquire_writer_lock(first).is_ok());
    assert!(store.acquire_writer_lock(second).is_err());
    drop(existing);
    assert!(store.acquire_writer_lock(second).is_ok());
}

#[tokio::test]
async fn reservation_cannot_reuse_an_existing_live_writer() {
    let home = TempDir::new().unwrap();
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let id = ThreadId::new();
    store.create_thread(create_thread_params(id)).await.unwrap();
    assert!(store.reserve_thread_writers(vec![id]).await.is_err());
    store.flush_thread(id).await.unwrap();
    store.shutdown_thread(id).await.unwrap();
    assert!(store.reserve_thread_writers(vec![id]).await.is_ok());
}
