//! Account notifications omit email; fetch it from the server, never from stale local auth.

use super::*;
use crate::app_server_session::ThreadParamsMode;
use crate::status::StatusAccountDisplay;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::AccountUpdatedNotification;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::JSONRPCMessage;
use codex_protocol::account::PlanType;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn account_updates_refresh_email_without_restoring_a_previous_identity() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let initialized = InitializeResponse {
        user_agent: "account-test/1.0.0".into(),
        codex_home: app.config.codex_home.clone(),
        platform_family: std::env::consts::FAMILY.into(),
        platform_os: std::env::consts::OS.into(),
    };
    let daemon = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut replies = [
            json!({"result": {"account": {
                "type": "chatgpt", "email": "old@example.com", "planType": "pro"
            }, "requiresOpenaiAuth": true}}),
            json!({"result": {"account": {
                "type": "chatgpt", "email": "current@example.com", "planType": "pro"
            }, "requiresOpenaiAuth": true}}),
            json!({"result": {"account": {
                "type": "chatgpt", "email": null, "planType": "pro"
            }, "requiresOpenaiAuth": true}}),
            json!({"result": {"account": {
                "type": "gcp"
            }, "requiresOpenaiAuth": false}}),
            json!({"error": {"code": -32000, "message": "account unavailable"}}),
        ]
        .into_iter();
        let mut reads = 0;
        while let Some(Ok(frame)) = socket.next().await {
            let Message::Text(text) = frame else {
                continue;
            };
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let mut reply = match request.method.as_str() {
                "initialize" => json!({"result": initialized}),
                "account/read" => {
                    // The default false flag is omitted on the wire.
                    assert_eq!(request.params, Some(json!({})));
                    reads += 1;
                    replies
                        .next()
                        .ok_or_else(|| color_eyre::eyre::eyre!("unexpected extra account read"))?
                }
                other => {
                    color_eyre::eyre::bail!("unexpected method: {other}");
                }
            };
            reply["id"] = json!(request.id);
            socket.send(Message::Text(reply.to_string().into())).await?;
        }
        Ok::<_, color_eyre::Report>(reads)
    });
    let mut session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Remote,
    );
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut superseded = None;
    let mut account_lines = Vec::new();
    for (index, email) in [
        Some("old@example.com"),
        Some("current@example.com"),
        None,
        None,
        None,
    ]
    .into_iter()
    .enumerate()
    {
        app.handle_app_server_event(
            &session,
            AppServerEvent::ServerNotification(Box::new(ServerNotification::AccountUpdated(
                AccountUpdatedNotification {
                    auth_mode: Some(AuthMode::Chatgpt),
                    plan_type: Some(PlanType::Pro),
                },
            ))),
        )
        .await;
        let pending = Some(StatusAccountDisplay::ChatGpt {
            email: None,
            plan: Some("Pro 200".into()),
        });
        assert_eq!(app.chat_widget.status_account_display().cloned(), pending);
        let loaded = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
            loop {
                let event = events
                    .recv()
                    .await
                    .ok_or_else(|| color_eyre::eyre::eyre!("account event channel closed"))?;
                if matches!(event, AppEvent::AccountEmailLoaded { .. }) {
                    return Ok::<_, color_eyre::Report>(event);
                }
            }
        })
        .await??;
        if index == 0 {
            superseded = Some(loaded);
            continue;
        }
        if let Some(old) = superseded.take() {
            app.handle_event(&mut tui, &mut session, old).await?;
            assert_eq!(app.chat_widget.status_account_display().cloned(), pending);
        }
        app.handle_event(&mut tui, &mut session, loaded).await?;
        assert_eq!(
            app.chat_widget.status_account_display().cloned(),
            Some(StatusAccountDisplay::ChatGpt {
                email: email.map(str::to_owned),
                plan: Some("Pro 200".into()),
            }),
        );
        app.chat_widget.add_status_output(
            /*refreshing_rate_limits*/ false, /*request_id*/ None,
        );
        while let Ok(event) = events.try_recv() {
            if let AppEvent::InsertHistoryCell(cell) = event {
                for line in cell.display_lines(/*width*/ 100) {
                    let text = line.to_string();
                    if let Some((_, account)) = text.split_once("Account:") {
                        account_lines.push(account.trim().to_string());
                    }
                }
            }
        }
    }
    insta::assert_snapshot!(account_lines.join("\n"), @"
    current@example.com (Pro 200)
    Pro 200
    Pro 200
    Pro 200
    ");
    // A response queued before logout must not restore email after switching to API-key auth.
    let stale_id = Uuid::new_v4();
    app.account_email_request_id = Some(stale_id);
    app.handle_app_server_event(
        &session,
        AppServerEvent::ServerNotification(Box::new(ServerNotification::AccountUpdated(
            AccountUpdatedNotification {
                auth_mode: Some(AuthMode::ApiKey),
                plan_type: None,
            },
        ))),
    )
    .await;
    app.handle_event(
        &mut tui,
        &mut session,
        AppEvent::AccountEmailLoaded {
            request_id: stale_id,
            email: Some("old@example.com".into()),
        },
    )
    .await?;
    assert_eq!(
        app.chat_widget.status_account_display(),
        Some(&StatusAccountDisplay::ApiKey),
    );
    session.shutdown().await?;
    assert_eq!(daemon.await??, 5);
    Ok(())
}
