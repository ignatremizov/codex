//! Exercise the real tool runtime's abort path without introducing production cancellation hooks.

use super::*;
use crate::UserAgentSpawnOptions;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::multi_agents::SendInputHandler;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::openai_models::ToolMode;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::MailboxMessageState;
use codex_tools::ToolName;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

#[test_case::test_case("z"; "plain_mailbox")]
#[test_case::test_case("zf"; "conditional_mailbox")]
#[tokio::test]
async fn native_batch_abort_retains_accepted_prefix_without_a_completed_aggregate(flags: &str) {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let mut receivers = Vec::new();
    for _ in 0..3 {
        receivers.push(
            root.spawn_agent(UserAgentSpawnOptions::default())
                .await
                .expect("receiver")
                .target_thread_id,
        );
    }
    let control = root.session.services.agent_control.clone();
    let turn = root.session.new_default_turn().await;
    assert!(root.session.begin_agent_response_turn(&turn.sub_id));
    let call_id = "cancelled-batch";
    let key =
        serde_json::to_string(&("v1-send-input-mailbox", root_id, &turn.sub_id, call_id)).unwrap();
    let handler = Arc::new(SendInputHandler) as Arc<dyn CoreToolRuntime>;
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
        ToolMode::Direct,
        BTreeMap::new(),
        /*tool_namespaces_info*/ None,
        &[],
    ));
    let step = StepContext::for_test(turn).with_tool_router_for_test(router);
    let runtime = ToolCallRuntime::new(
        Arc::clone(&root.session),
        step,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let (first_reached_tx, first_reached_rx) = tokio::sync::oneshot::channel();
    let (first_release_tx, first_release_rx) = tokio::sync::oneshot::channel();
    *control
        .wait_agent_presentations
        .mailbox_acceptance_gate
        .lock()
        .unwrap() = Some((first_reached_tx, first_release_rx));
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(
        runtime.handle_tool_call(
            ToolCall {
                tool_name: ToolName::namespaced("multi_agent_v1", "send_input"),
                call_id: call_id.to_string(),
                payload: ToolPayload::Function {
                    arguments: json!({
                        "target": receivers.iter().map(ToString::to_string).collect::<Vec<_>>(),
                        "message": "Keep accepted mail even if the sender cancels.",
                        "w": flags,
                    })
                    .to_string(),
                },
                encrypted_function_args: None,
            },
            cancellation.clone(),
        ),
    );
    timeout(Duration::from_secs(/*secs*/ 15), first_reached_rx)
        .await
        .expect("first acceptance reaches its pre-persist gate")
        .expect("first gate");
    // Install the next gate while the first recipient is paused, before releasing it.
    let (second_reached_tx, second_reached_rx) = tokio::sync::oneshot::channel();
    let (second_release_tx, second_release_rx) = tokio::sync::oneshot::channel();
    *control
        .wait_agent_presentations
        .mailbox_acceptance_gate
        .lock()
        .unwrap() = Some((second_reached_tx, second_release_rx));
    first_release_tx.send(()).expect("release first admission");
    timeout(Duration::from_secs(/*secs*/ 15), second_reached_rx)
        .await
        .expect("second acceptance reaches its pre-persist gate")
        .expect("second gate");
    let store = &root.session.services.thread_store;
    let accepted = store
        .lookup_mailbox_input(receivers[0], &key)
        .await
        .unwrap()
        .expect("the first recipient was accepted before the second gate");
    assert_eq!(accepted.state, MailboxMessageState::Pending);

    cancellation.cancel();
    let response = timeout(Duration::from_secs(/*secs*/ 15), task)
        .await
        .expect("native runtime finishes abort and awaits the handler")
        .expect("runtime task")
        .expect("aborted tool response");
    let ResponseItem::FunctionCallOutput {
        call_id: returned_call_id,
        output,
        ..
    } = response.response.item
    else {
        panic!("expected tool output");
    };
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("expected abort text");
    };
    assert_eq!(returned_call_id.as_deref(), Some(call_id));
    assert!(text.contains("aborted by user"));
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_err(),
        "abort must not invent per-recipient outcomes"
    );
    // At this controlled pre-persist boundary the second admission has not committed.
    // This is not a general non-delivery guarantee for arbitrary cancellation boundaries:
    // notably, zf acceptance owns a cancellation-safe continuation after this gate.
    assert!(
        second_release_tx.send(()).is_err(),
        "native abort dropped the gated handler"
    );
    assert_eq!(
        store
            .lookup_mailbox_input(receivers[0], &key)
            .await
            .unwrap(),
        Some(accepted)
    );
    for receiver in &receivers[1..] {
        assert_eq!(
            store.lookup_mailbox_input(*receiver, &key).await.unwrap(),
            None
        );
    }
    root.flush_rollout().await.expect("flush lifecycle");
    let history = store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: root_id,
            include_archived: false,
        })
        .await
        .unwrap();
    // Started items are live-only; the rollout intentionally persists completed items.
    let mut started = 0;
    while let Some(event) = root.try_next_event().unwrap() {
        match event.msg {
            EventMsg::ItemStarted(event) if matches!(&event.item, TurnItem::CollabAgentToolCall(call) if call.id == call_id) =>
            {
                started += 1;
            }
            EventMsg::ItemCompleted(event) if matches!(&event.item, TurnItem::CollabAgentToolCall(call) if call.id == call_id) =>
            {
                panic!("cancelled batch must not emit a completed aggregate");
            }
            _ => {}
        }
    }
    assert_eq!(started, 1);
    assert!(
        !history.items.iter().any(|item| matches!(
            item,
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if matches!(&event.item, TurnItem::CollabAgentToolCall(call) if call.id == call_id)
        )),
        "cancelled batch must not fabricate a completed aggregate"
    );
    for receiver in receivers {
        harness
            .manager
            .get_thread(receiver)
            .await
            .unwrap()
            .shutdown_and_wait()
            .await
            .unwrap();
    }
    root.shutdown_and_wait().await.unwrap();
}
