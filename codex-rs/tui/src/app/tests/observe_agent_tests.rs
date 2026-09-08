use super::*;
use crate::app::agent_observation_display::AgentResponseObservationBinding;
use crate::chatwidget::agent_command::AgentSelector;
use crate::chatwidget::agent_command::AgentSelectorKind;
use codex_app_server_protocol::AgentObservationMode;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn explicit_observer_changes_its_binding_without_switching_issuer_or_starting_a_turn()
-> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let (mut app_server, requests, proxy) = session_lifecycle_requests::start_recording_app_server(
        &app.config,
        /*blocked_thread_list*/ None,
        /*failed_thread_name*/ None,
    )
    .await?;
    let issuer = app_server
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    let observer = ThreadId::from_string("019ff050-d466-73b0-b133-72ecc7c67270")?;
    session_lifecycle_requests::display_test_thread(&mut app, issuer);
    for (kind, authored, normalized) in [
        (
            AgentSelectorKind::Nickname("Peirce".to_string()),
            "Peirce",
            "nick:Peirce",
        ),
        (AgentSelectorKind::Ref(146), "ref:146", "146"),
        (
            AgentSelectorKind::Nickname("Charles Peirce".to_string()),
            "nick:\"Charles Peirce\"",
            "nick:Charles Peirce",
        ),
    ] {
        requests.lock().expect("request recorder lock").clear();

        app.observe_agent_from_selector(
            &mut app_server,
            issuer,
            AgentSelector {
                kind: AgentSelectorKind::Id(issuer),
                authored: "Main".to_string(),
            },
            Some(AgentSelector {
                kind,
                authored: authored.to_string(),
            }),
            AgentObservationMode::Passive,
        )
        .await;

        assert_eq!(app.active_thread_id, Some(issuer));
        assert_eq!(
            app.agent_navigation.response_observation(issuer, issuer),
            None
        );
        assert_eq!(
            app.agent_navigation.response_observation(observer, issuer),
            Some(
                crate::app::agent_observation_display::AgentResponseObservationDisplay {
                    binding: AgentResponseObservationBinding::Bound,
                    commentary: false,
                    target_messages: false,
                    queue_delivery: false,
                    final_response:
                        crate::app::agent_observation_display::AgentFinalResponseDisplay::Passive,
                }
            )
        );
        let recorded = requests.lock().expect("request recorder lock").clone();
        let controls = recorded
            .iter()
            .filter(|request| request.method == "agent/control")
            .collect::<Vec<_>>();
        assert_eq!(controls.len(), 1);
        assert_eq!(
            controls[0].params,
            Some(serde_json::json!({
                "sourceThreadId": issuer.to_string(),
                "authoredSelector": "Main",
                "action": {
                    "type": "observe",
                    "target": issuer.to_string(),
                    "observer": normalized,
                    "authoredObserverSelector": authored,
                    "responseHandling": "passive",
                },
            }))
        );
        assert!(
            !recorded
                .iter()
                .any(|request| matches!(request.method.as_str(), "thread/start" | "turn/start"))
        );
    }
    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}
