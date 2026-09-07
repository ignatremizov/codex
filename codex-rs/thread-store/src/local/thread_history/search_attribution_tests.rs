use super::*;
use serde_json::json;

#[tokio::test]
async fn search_includes_nonfinal_attributed_input_and_deduplicates_final_input() {
    let (_home, store, thread_id) = store_with_mode(ThreadHistoryMode::Paginated).await;
    let sender = ThreadId::new().to_string();
    let db = history_db(&store).await;
    for (turn_id, item_id, ordinal, final_agent) in [
        ("first", "inbound-1", 1_i64, None),
        ("second", "inbound-2", 4_i64, Some("inbound-2")),
    ] {
        insert_turn(
            db,
            thread_id,
            turn_id,
            ordinal,
            "completed",
            /*error_json*/ None,
            /*first_user_item_id*/ None,
            final_agent,
        )
        .await;
        let item = json!({
            "type": "agentMessage", "id": item_id, "text": "",
            "attribution": {
                "sender": {"threadId": sender},
                "recipient": {"threadId": thread_id.to_string()},
                "senderTurnId": "source-turn"
            },
            "input": [
                {"type": "text", "text": "  **literal**\n</agent_message>  "},
                {"type": "image", "url": "data:image/png;base64,secret-image"}
            ]
        });
        sqlx::query(
            "INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, \
             updated_at_ordinal, created_at_ms, item_type, item_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .bind(item_id)
        .bind(ordinal + 1)
        .bind(ordinal + 1)
        .bind(1_000_i64)
        .bind("agentMessage")
        .bind(item.to_string())
        .execute(db)
        .await
        .unwrap();
    }

    for term in [
        "**literal**",
        "</agent_message>",
        sender.as_str(),
        "[image]",
    ] {
        let mut cursor = None;
        let mut found = Vec::new();
        for _ in 0..3 {
            let page = store
                .search_thread_occurrences(SearchThreadOccurrencesParams {
                    thread_id,
                    search_term: term.into(),
                    cursor,
                    page_size: 1,
                })
                .await
                .unwrap();
            found.extend(page.items.into_iter().map(|item| item.item_id));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(cursor, None, "pagination must terminate");
        assert_eq!(found, vec!["inbound-1", "inbound-2"], "term {term:?}");
    }
    let page = store
        .search_thread_occurrences(SearchThreadOccurrencesParams {
            thread_id,
            search_term: "secret-image".into(),
            cursor: None,
            page_size: 10,
        })
        .await
        .unwrap();
    assert!(page.items.is_empty());
}
