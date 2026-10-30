//! Live Legacy receipts must select the same prompt after canonical history reconstruction.

use super::*;
use crate::history_cell::UserMessageIdentity;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn live_legacy_prompts_edit_without_reopening_the_tui() -> Result<()> {
    for history in [PromptHistory::SeparateTurns, PromptHistory::MidTurnSteer] {
        for fork in [false, true] {
            for cached in [false, true] {
                let (mut app, mut events, (mut server, requests, proxy), source_id, path) =
                    prompt_source_with_history(
                        ThreadHistoryMode::Legacy,
                        HistoryCapabilities::Current,
                        PromptImages::TextOnly,
                        history,
                    )
                    .await?;
                if !fork {
                    app.config.features.disable(Feature::ForkPromptEdits)?;
                }
                let mut tui = crate::tui::test_support::make_test_tui()?;
                drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
                let original = server
                    .thread_read(source_id, /*include_turns*/ true)
                    .await?;
                let original_bytes = std::fs::read(&path)?;
                let selected_number = match history {
                    PromptHistory::SeparateTurns => 2,
                    PromptHistory::MidTurnSteer => 3,
                };
                let selected_index = nth_user_position(&app.transcript_cells, selected_number)
                    .expect("selected prompt");
                let replayed = app.transcript_cells[selected_index]
                    .as_any()
                    .downcast_ref::<UserHistoryCell>()
                    .expect("user cell");
                let canonical_identity =
                    replayed.identity.get().expect("replayed identity").clone();
                let (canonical_turn_index, canonical_item) = original
                    .turns
                    .iter()
                    .enumerate()
                    .find_map(|(index, turn)| {
                        turn.items
                            .iter()
                            .find(|item| {
                                turn.id == canonical_identity.turn_id
                                    && item.id() == canonical_identity.item_id
                            })
                            .map(|item| (index, item.clone()))
                    })
                    .expect("persisted prompt");
                let ThreadItem::UserMessage {
                    client_id, content, ..
                } = canonical_item
                else {
                    unreachable!()
                };
                let mut optimistic = crate::history_cell::new_user_prompt(
                    replayed.message.clone(),
                    replayed.text_elements.clone(),
                    replayed.local_image_paths.clone(),
                    replayed.remote_image_urls.clone(),
                );
                optimistic.client_id = client_id.clone();
                let selected_cell: Arc<dyn crate::history_cell::HistoryCell> = Arc::new(optimistic);
                app.transcript_cells[selected_index] = Arc::clone(&selected_cell);
                let live_item_id = ThreadId::new().to_string();
                // Exercise the live receipt binding, not a manually assigned canonical identity.
                let receipt = codex_app_server_protocol::ItemCompletedNotification {
                    thread_id: source_id.to_string(),
                    turn_id: canonical_identity.turn_id.clone(),
                    item: ThreadItem::UserMessage {
                        id: live_item_id.clone(),
                        client_id,
                        content,
                    },
                    completed_at_ms: 0,
                };
                app.chat_widget.handle_server_notification(
                    ServerNotification::ItemCompleted(receipt.clone()),
                    /*replay_kind*/ None,
                );
                drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
                let live_identity = UserMessageIdentity {
                    turn_id: canonical_identity.turn_id.clone(),
                    item_id: live_item_id,
                };
                assert_eq!(
                    selected_cell
                        .as_any()
                        .downcast_ref::<UserHistoryCell>()
                        .expect("live user cell")
                        .identity
                        .get(),
                    Some(&live_identity),
                );
                // A receipt may render another copy; backtracking selects its newest
                // visible cell instead of the optimistic Arc hidden by deduplication.
                let selected_cell =
                    crate::app_backtrack::user_positions_iter(&app.transcript_cells)
                        .find_map(|index| {
                            let cell = &app.transcript_cells[index];
                            cell.as_any()
                                .downcast_ref::<UserHistoryCell>()
                                .is_some_and(|user| user.identity.get() == Some(&live_identity))
                                .then(|| Arc::clone(cell))
                        })
                        .expect("visible live receipt");
                {
                    let mut store = app
                        .thread_event_channels
                        .get(&source_id)
                        .expect("source channel")
                        .store
                        .lock()
                        .await;
                    store.turn_history_complete = cached;
                    if cached {
                        store.push_notification(ServerNotification::ItemCompleted(receipt));
                    }
                }
                Box::pin(app.handle_event(
                    &mut tui,
                    &mut server,
                    AppEvent::RevertSessionForPromptEdit {
                        thread_id: source_id,
                        selected_cell,
                        prompt: crate::chatwidget::UserMessage::from(SELECTED),
                    },
                ))
                .await?;
                if fork {
                    drain_fork_attachment(&mut app, &mut tui, &mut server, &mut events).await?;
                } else {
                    drain_attach_events(&mut app, &mut tui, &mut server, &mut events).await?;
                }
                let method = if fork {
                    "thread/fork"
                } else {
                    "thread/rollback"
                };
                let mutations = recorded_params(&requests, method);
                assert_eq!(
                    mutations.len(),
                    1,
                    "the visible live prompt must issue one edit: fork={fork}, cached={cached}, selected={selected_number}",
                );
                let edited_id = app.chat_widget.thread_id().expect("edited thread");
                assert_eq!(edited_id == source_id, !fork);
                let edited = server
                    .thread_read(edited_id, /*include_turns*/ true)
                    .await?;
                let mut expected: Vec<_> = original
                    .turns
                    .iter()
                    .flat_map(|turn| turn.items.clone())
                    .collect();
                let selected = expected
                    .iter()
                    .position(|item| item.id() == canonical_identity.item_id)
                    .expect("selected canonical prompt");
                expected.truncate(selected);
                expected.retain(|item| !matches!(
                    item, ThreadItem::AgentMessage { text, .. } if text == "Output after steer admission"
                ));
                assert_eq!(
                    edited
                        .turns
                        .iter()
                        .flat_map(|turn| turn.items.clone())
                        .collect::<Vec<_>>(),
                    expected,
                );
                assert_eq!(app.chat_widget.composer_text_with_pending(), SELECTED);
                assert_eq!(app.pending_thread_switch_resets, 0);
                assert!(!app.thread_unavailable(edited_id));
                let expected_boundary =
                    (history == PromptHistory::MidTurnSteer).then_some(canonical_identity.item_id);
                let expected_params = if fork {
                    serde_json::json!({
                        "beforeTurnId": canonical_identity.turn_id,
                        "beforeItemId": expected_boundary,
                    })
                } else {
                    serde_json::json!({
                        "expectedStartTurnId": canonical_identity.turn_id,
                        "expectedStartItemId": expected_boundary,
                        "expectedTurnCount": original.turns.len(),
                        "numTurns": original.turns.len() - canonical_turn_index,
                    })
                };
                let observed: serde_json::Value = expected_params
                    .as_object()
                    .expect("expected request")
                    .keys()
                    .map(|key| (key.clone(), mutations[0][key].clone()))
                    .collect();
                assert_eq!(observed, expected_params);
                assert!(recorded_params(&requests, "turn/start").is_empty());
                if fork {
                    assert_eq!(std::fs::read(&path)?, original_bytes);
                }
                let rendered = app
                    .transcript_cells
                    .iter()
                    .filter(|cell| cell.as_any().is::<UserHistoryCell>())
                    .flat_map(|cell| cell.display_lines(/*width*/ 80))
                    .collect::<Vec<_>>();
                let rendered = format!(
                    "{}\nDraft: {}",
                    lines_to_single_string(&rendered).trim(),
                    app.chat_widget.composer_text_with_pending(),
                );
                insta::allow_duplicates! {
                    match history {
                        PromptHistory::SeparateTurns => insta::assert_snapshot!(rendered, @"
                    › older prompt


                    › retained prompt
                    Draft: use $skill @sample $google-calendar
                    "),
                        PromptHistory::MidTurnSteer => insta::assert_snapshot!(rendered, @"
                    › older prompt


                    › retained prompt


                    › use $skill @sample $google-calendar
                    Draft: use $skill @sample $google-calendar
                    "),
                    }
                }
                server.shutdown().await?;
                proxy.await??;
            }
        }
    }
    Ok(())
}
