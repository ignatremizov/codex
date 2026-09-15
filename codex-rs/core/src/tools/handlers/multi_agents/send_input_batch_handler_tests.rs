use super::*;

#[tokio::test]
async fn invalid_shared_batch_input_fails_before_admission() {
    for arguments in [
        json!({"target": [], "message": "hello"}),
        json!({"target": ["43"], "message": "hello", "w": "z", "interrupt": true}),
        json!({"target": ["43"], "message": "hello", "w": "zc"}),
        json!({"target": ["43"], "message": "hello", "w": "zm"}),
        json!({"target": ["43"], "message": "hello", "w": "zq"}),
        json!({"target": ["43"], "message": "hello", "items": [{"type":"text","text":"other"}]}),
    ] {
        let (session, turn) = make_session_and_context().await;
        assert!(
            SendInputHandler
                .handle(invocation(
                    Arc::new(session),
                    Arc::new(turn),
                    "send_input",
                    function_payload(arguments),
                ))
                .await
                .is_err()
        );
    }
}

#[test_case::test_case(None, "submitted"; "default")]
#[test_case::test_case(Some("cf"), "submitted"; "commentary_final")]
#[test_case::test_case(Some("m"), "submitted"; "reply_route")]
#[test_case::test_case(Some("q"), "queued"; "queue")]
#[test_case::test_case(Some("x"), "submitted"; "presentation_only")]
#[tokio::test]
async fn live_batch_reuses_admission_and_deduplicates_aliases(flags: Option<&str>, expected: &str) {
    let (_session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    let config = turn.config.as_ref().clone();
    let parent = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .unwrap();
    let first = spawn_idle_v1_child(&parent.thread, config.clone()).await;
    let second = spawn_idle_v1_child(&parent.thread, config).await;
    let first_ref = parent
        .thread
        .session
        .services
        .agent_control
        .get_agent_presentation_ref(first)
        .await
        .unwrap();
    let second_ref = parent
        .thread
        .session
        .services
        .agent_control
        .get_agent_presentation_ref(second)
        .await
        .unwrap();
    let mut arguments = json!({
        "target": [first.to_string(), first_ref.agent_ref.clone().unwrap_or_else(|| first.to_string()), "unresolved-selector", second.to_string()],
        "items": [{"type":"text","text":"shared input"}],
    });
    if let Some(flags) = flags {
        arguments["w"] = json!(flags);
    }
    let output = SendInputHandler
        .handle(invocation(
            Arc::clone(&parent.thread.session),
            parent.thread.session.new_default_turn().await,
            "send_input",
            function_payload(arguments),
        ))
        .await
        .unwrap();
    let (text, success) = expect_text_output(output);
    let result: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(success, Some(false));
    let entries = result["results"].as_array().unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| json!({"target":entry["target"],"status":entry["status"]}))
            .collect::<Vec<_>>(),
        vec![
            json!({"target":first_ref.agent_ref.unwrap_or_else(|| first.to_string()),"status":expected}),
            json!({"target":"unresolved-selector","status":"error"}),
            json!({"target":second_ref.agent_ref.unwrap_or_else(|| second.to_string()),"status":expected}),
        ],
    );
    assert!(entries[1]["error"].is_string());
    for receiver in [first, second] {
        let child = manager.get_thread(receiver).await.unwrap();
        wait_for_recorded_agent_input(
            child.as_ref(),
            parent.thread_id,
            &[UserInput::Text {
                text: "shared input".into(),
                text_elements: Vec::new(),
            }],
        )
        .await;
        let inputs = manager
            .captured_ops()
            .into_iter()
            .filter(|(id, op)| *id == receiver && matches!(op, Op::AgentInput { .. }))
            .count();
        assert_eq!(inputs, 1);
        child.submit(Op::Shutdown {}).await.unwrap();
    }
    parent.thread.submit(Op::Shutdown {}).await.unwrap();
}

#[tokio::test]
async fn array_of_one_has_batch_result_and_self_send_is_an_attributed_error() {
    let (session, turn) = make_session_and_context().await;
    let target = session.thread_id.to_string();
    let output = SendInputHandler
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "send_input",
            function_payload(json!({"target":[target], "message":"hello"})),
        ))
        .await
        .unwrap();
    let (text, success) = expect_text_output(output);
    let result: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(success, Some(false));
    assert_eq!(result["results"].as_array().unwrap().len(), 1);
    assert_eq!(result["results"][0]["target"], target);
    assert_eq!(result["results"][0]["status"], "error");
}

#[tokio::test]
async fn closed_and_foreign_targets_do_not_prevent_later_descendant_admission() {
    let (_session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    let config = turn.config.as_ref().clone();
    let parent = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .unwrap();
    let foreign = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .unwrap();
    let closed = spawn_idle_v1_child(&parent.thread, config.clone()).await;
    let live = spawn_idle_v1_child(&parent.thread, config).await;
    manager
        .get_thread(closed)
        .await
        .unwrap()
        .shutdown_and_wait()
        .await
        .unwrap();
    manager.remove_thread(&closed).await;
    let output = SendInputHandler
        .handle(invocation(
            Arc::clone(&parent.thread.session),
            parent.thread.session.new_default_turn().await,
            "send_input",
            function_payload(json!({
                "target": [closed.to_string(), foreign.thread_id.to_string(), live.to_string()],
                "message": "redirect", "interrupt": true,
            })),
        ))
        .await
        .unwrap();
    let (text, success) = expect_text_output(output);
    let result: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(success, Some(false));
    assert_eq!(
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["status"].clone())
            .collect::<Vec<_>>(),
        vec![json!("error"), json!("error"), json!("submitted")],
    );
    let ops = manager.captured_ops();
    assert!(
        !ops.iter().any(|(id, op)| *id == foreign.thread_id
            && matches!(op, Op::Interrupt | Op::AgentInput { .. }))
    );
    assert_eq!(
        ops.iter()
            .filter_map(|(id, op)| (*id == live).then_some(op))
            .filter(|op| matches!(op, Op::Interrupt))
            .count(),
        1,
    );
    for child in [live, foreign.thread_id, parent.thread_id] {
        manager
            .get_thread(child)
            .await
            .unwrap()
            .submit(Op::Shutdown {})
            .await
            .unwrap();
    }
}
