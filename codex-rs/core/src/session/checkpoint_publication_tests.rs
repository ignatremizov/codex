use super::*;
use codex_extension_api::RestoredSkillsInventory;
use codex_history::CodexHarnessMetadata;

#[tokio::test]
async fn checkpoint_and_cold_reconstruction_preserve_full_mcp_union_and_latest_empty_catalog() {
    let (mut session, turn, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |_| {},
    )
    .await;
    let rollout_path =
        attach_thread_persistence(Arc::get_mut(&mut session).expect("unique session")).await;
    let catalog = |promoted: serde_json::Value| -> ResponseItemEnvelope {
        ResponseItemEnvelope::new(serde_json::from_value(serde_json::json!({
            "type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": format!(
                "<skills_instructions>\n<promoted_skills>{promoted}</promoted_skills>\n## Skills\n</skills_instructions>"
            )}],
            "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["skills.catalog"]}
        })).expect("catalog"))
    };
    let mut first = ResponseItemEnvelope::new(crate::context::ContextualUserFragment::into(
        crate::context::McpServerUseInstructions::new("accepted".to_string(), "[]".to_string()),
    ));
    first.metadata = Some(CodexHarnessMetadata {
        user_input_order: Some(11),
        ..Default::default()
    });
    let mut second = first.clone();
    second.metadata.as_mut().expect("metadata").user_input_order = Some(12);
    let empty = catalog(serde_json::json!([]));
    let source = vec![
        first.clone(),
        catalog(serde_json::json!([{
            "authorityKindHex": "686f7374", "authorityIdHex": "61", "packageHex": "62"
        }])),
        second.clone(),
        empty.clone(),
    ];
    session
        .state
        .lock()
        .await
        .history
        .record_annotated_items(&source, turn.model_info().truncation_policy.into());
    session
        .persist_rollout_items(
            &source
                .iter()
                .cloned()
                .map(RolloutItem::ResponseItem)
                .collect::<Vec<_>>(),
        )
        .await;
    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    let installed = session
        .replace_compacted_history(
            vec![ResponseItemEnvelope::new(user_message("retained user"))],
            /*reference_context_item*/ None,
            /*world_state_baseline*/ None,
            CompactedHistoryMetadata {
                completion_source_items: Vec::new(),
                message: "summary".to_string(),
                compaction_summary_tokens: None,
                window_number,
                window_ids,
                compaction_response_id: None,
                compaction_model_hash: None,
                reviewer_compaction_hash: None,
            },
        )
        .await
        .expect("publish checkpoint");
    assert_eq!(
        &installed[..3],
        &[first.clone(), second.clone(), empty.clone()]
    );
    assert_eq!(installed, session.clone_history().await.annotated_items());
    let reconstructed = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect("cold canonical read");
    let InitialHistory::Resumed(resumed) = &reconstructed else {
        panic!("resumed history");
    };
    let checkpoint = resumed
        .history
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::Compacted(compacted) => compacted.replacement_history.as_ref(),
            _ => None,
        })
        .expect("checkpoint");
    assert_eq!(checkpoint, &installed);
    let (fresh, _) = make_session_and_context().await;
    fresh
        .record_initial_history(reconstructed)
        .await
        .expect("record reconstructed history");
    let restored = fresh.clone_history().await;
    assert_eq!(restored.annotated_items(), installed);
    let inventory = restored
        .annotated_items()
        .iter()
        .rev()
        .find_map(RestoredSkillsInventory::from_envelope)
        .expect("restored inventory");
    assert_eq!(inventory.envelope(), &empty);
}
