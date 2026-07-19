use std::io::Read;

use codex_protocol::protocol::SessionMeta;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn write_source(path: &Path, thread_id: ThreadId) -> anyhow::Result<Vec<u8>> {
    fs::create_dir_all(path.parent().expect("source parent"))?;
    let meta = SessionMeta {
        id: thread_id,
        session_id: thread_id.into(),
        timestamp: "2026-09-23T12:00:00Z".into(),
        cwd: path.parent().expect("source parent").to_owned(),
        ..Default::default()
    };
    let records = [
        json!({"timestamp": "2026-09-23T12:00:00Z", "ordinal": 0,
            "type": "session_meta", "payload": meta}),
        json!({"timestamp": "2026-09-23T12:00:01Z", "ordinal": 1,
        "type": "compacted", "payload": {"message": "audit sentinel",
            "replacement_history": [{"type": "message", "role": "user", "content": [
                {"type": "input_image", "image_url": "data:image/png;base64,old"}
            ]}]}}),
    ];
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, &record)?;
        bytes.push(b'\n');
    }
    fs::write(path, &bytes)?;
    Ok(bytes)
}

#[tokio::test]
async fn readers_and_discovery_use_manifest_without_publishing_history() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let id = ThreadId::default();
    let path = home
        .path()
        .join("sessions/2026/09/23")
        .join(format!("rollout-2026-09-23T12-00-00-{id}.jsonl"));
    let expected = write_source(&path, id)?;
    let backup = prepare_backup(&path)?;
    let manifest = manifest_path(&path)?;
    let receipt = fs::read(&manifest)?;
    fs::remove_file(&path)?;

    assert_eq!(
        crate::find_thread_path_by_id_str(home.path(), &id.to_string(), /*state_db_ctx*/ None)
            .await?,
        Some(path.clone())
    );
    let mut actual = Vec::new();
    crate::open_rollout_seekable_reader(&path)?.read_to_end(&mut actual)?;
    assert_eq!(actual, expected);
    let mut reader = crate::open_rollout_line_reader(&path.with_extension("jsonl.zst")).await?;
    let mut lines = Vec::new();
    while let Some(line) = reader.next_line().await? {
        lines.push(line);
    }
    assert_eq!(
        lines.join("\n") + "\n",
        String::from_utf8(expected.clone())?
    );
    assert_eq!(fs::read(&backup)?, expected);
    assert_eq!(fs::read(&manifest)?, receipt);
    assert!(!path.exists());
    assert!(!path.with_extension("jsonl.zst").exists());

    recover_compacted_media_backup_if_needed(&path)?;
    assert_eq!(fs::read(&path)?, expected);
    Ok(())
}

#[test]
fn manifest_selects_exact_preimage_and_rejects_changed_content() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("rollout.jsonl");
    let expected = write_source(&path, ThreadId::default())?;
    let backup = prepare_backup(&path)?;
    // Another syntactically plausible backup, even one with different valid history, has no vote.
    let other = path.with_file_name(format!(
        ".rollout.jsonl.pre-media-vacuum-{}.bak",
        Uuid::now_v7()
    ));
    write_source(&other, ThreadId::default())?;
    fs::remove_file(&path)?;
    let mut actual = Vec::new();
    open_recovery_source(&path)?
        .expect("authorized source")
        .read_to_end(&mut actual)?;
    assert_eq!(actual, expected);

    let changed = String::from_utf8(expected.clone())?.replace("sentinel", "modified");
    assert_eq!(changed.len(), expected.len());
    fs::write(&backup, changed)?;
    let error = open_recovery_source(&path).expect_err("same-size corruption must fail");
    assert!(
        error
            .to_string()
            .contains("does not match the authorized source")
    );
    assert!(!path.exists());
    assert!(other.exists());
    Ok(())
}

#[test]
fn unregistered_backups_and_corrupt_manifests_never_authorize_recovery() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("rollout.jsonl");
    let expected = write_source(&path, ThreadId::default())?;
    let backup = prepare_backup(&path)?;
    let manifest = manifest_path(&path)?;
    let receipt = fs::read(&manifest)?;
    fs::remove_file(&path)?;
    fs::remove_file(&manifest)?;
    assert!(open_recovery_source(&path)?.is_none());
    recover_compacted_media_backup_if_needed(&path)?;
    assert!(!path.exists());

    fs::write(&manifest, b"{\"version\":")?;
    assert!(open_recovery_source(&path).is_err());
    assert_eq!(fs::read(&manifest)?, b"{\"version\":");
    assert_eq!(fs::read(&backup)?, expected);
    fs::write(&manifest, receipt)?;
    assert!(open_recovery_source(&path)?.is_some());
    Ok(())
}

#[test]
fn manifest_binds_thread_filename_format_and_safe_backup_name() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let id = ThreadId::default();
    let path = home
        .path()
        .join(format!("rollout-2026-09-23T12-00-00-{id}.jsonl"));
    write_source(&path, id)?;
    prepare_backup(&path)?;
    let manifest = manifest_path(&path)?;
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&manifest)?)?;
    fs::remove_file(&path)?;
    for (key, value) in [
        ("thread_id", json!(ThreadId::default())),
        ("canonical", json!("another.jsonl")),
        ("format", json!("zstd")),
        ("backup", json!("../another.jsonl")),
        ("version", json!(2)),
    ] {
        let mut changed = original.clone();
        changed[key] = value;
        fs::write(&manifest, serde_json::to_vec(&changed)?)?;
        assert!(open_recovery_source(&path).is_err(), "{key}");
        assert!(!path.exists());
    }
    Ok(())
}

#[test]
fn append_retires_preimage_and_existing_canonical_never_falls_back() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("rollout.jsonl");
    write_source(&path, ThreadId::default())?;
    let backup = prepare_backup(&path)?;
    fs::write(&path, b"invalid canonical\n")?;
    assert!(open_recovery_source(&path)?.is_none());
    let mut actual = String::new();
    crate::open_rollout_seekable_reader(&path)?.read_to_string(&mut actual)?;
    assert_eq!(actual, "invalid canonical\n");
    // Restore controlled fixture content before entering the writer-owned append path.
    write_source(&path, ThreadId::default())?;
    crate::compression::materialize_rollout_for_append_blocking(&path)?;
    assert!(!backup.exists());
    assert!(!manifest_path(&path)?.exists());
    fs::remove_file(&path)?;
    assert!(open_recovery_source(&path)?.is_none());
    Ok(())
}

#[test]
fn ordinal_offsets_address_decoded_plain_or_compressed_bytes() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("rollout.jsonl");
    let bytes = write_source(&path, ThreadId::default())?;
    let first_line_end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("header")
        + 1;
    assert_eq!(
        crate::last_rollout_ordinal_before_offset(&path, first_line_end as u64)?,
        Some(0)
    );
    assert_eq!(
        crate::last_rollout_ordinal_before_offset(&path, first_line_end as u64 + 8)?,
        None
    );
    assert_eq!(
        crate::last_rollout_ordinal_before_offset(&path, bytes.len() as u64 + 1)?,
        None
    );
    let compressed = path.with_extension("jsonl.zst");
    fs::write(
        &compressed,
        zstd::stream::encode_all(bytes.as_slice(), /*level*/ 3)?,
    )?;
    fs::remove_file(&path)?;
    assert_eq!(
        crate::last_rollout_ordinal_before_offset(&path, first_line_end as u64)?,
        Some(0)
    );
    assert_eq!(
        crate::last_rollout_ordinal_before_offset(&compressed, bytes.len() as u64)?,
        Some(1)
    );
    assert_eq!(
        crate::last_rollout_ordinal_before_offset(&compressed, first_line_end as u64 + 8)?,
        None
    );
    assert!(!path.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn recovery_reads_do_not_need_a_writable_rollout_directory() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir()?;
    let path = home.path().join("rollout.jsonl");
    let expected = write_source(&path, ThreadId::default())?;
    let backup = prepare_backup(&path)?;
    fs::remove_file(&path)?;
    fs::set_permissions(home.path(), fs::Permissions::from_mode(/*mode*/ 0o500))?;
    let result = (|| -> io::Result<Vec<u8>> {
        let mut actual = Vec::new();
        crate::open_rollout_seekable_reader(&path)?.read_to_end(&mut actual)?;
        Ok(actual)
    })();
    fs::set_permissions(home.path(), fs::Permissions::from_mode(/*mode*/ 0o700))?;
    assert_eq!(result?, expected);
    assert_eq!(fs::read(backup)?, expected);
    assert!(!path.exists());
    Ok(())
}
