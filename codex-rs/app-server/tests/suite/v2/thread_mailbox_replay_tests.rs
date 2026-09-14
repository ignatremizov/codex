//! Real public acceptance and consumption must survive a cold transcript read.

use super::READ_TIMEOUT;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ImageReference;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadMailboxAddParams;
use codex_app_server_protocol::ThreadMailboxAddResponse;
use codex_app_server_protocol::ThreadMailboxReadParams;
use codex_app_server_protocol::ThreadMailboxReadResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::sse;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use test_case::test_case;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test]
async fn consumed_user_mail_keeps_original_payload_and_client_id_after_restart(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    const CLIENT_ID: &str = "persisted-mailbox-client-id";
    const PAYLOAD: &str = "public mailbox content must survive cold replay";
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(server.uri())
        .enable_feature(Feature::Collab)
        .disable_feature(Feature::MultiAgentV2)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?
        .thread;
    let (release, gate) = oneshot::channel();
    let mut first = server
        .mount_response(
            |request| request.body_contains_text("receive public mailbox input in this turn"),
            vec![StreamingSseChunk {
                gate: Some(gate),
                body: sse(vec![
                    ev_function_call_with_namespace(
                        "public-consume-mail",
                        "multi_agent_v1",
                        "check_mail",
                        "{}",
                    ),
                    ev_completed("public-mail-first"),
                ]),
            }],
        )
        .await;
    let request_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "receive public mailbox input in this turn".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let started: TurnStartResponse = timeout(READ_TIMEOUT, app.read_response(request_id)).await??;
    timeout(READ_TIMEOUT, first.wait_for_request()).await?;
    let input = vec![
        UserInput::Text { text: PAYLOAD.into(), text_elements: Vec::new() },
        UserInput::Image {
            image: ImageReference::Inline {
                url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".into(),
            },
            detail: None,
        },
    ];
    let _: ThreadMailboxAddResponse = app
        .request(|request_id| ClientRequest::ThreadMailboxAdd {
            request_id,
            params: ThreadMailboxAddParams {
                thread_id: thread.id.clone(),
                input: input.clone(),
                client_user_message_id: CLIENT_ID.into(),
            },
        })
        .await?;
    let mut followup = server
        .mount_response(
            |request| {
                request.body_contains_text("public-consume-mail")
                    && request.body_contains_text(PAYLOAD)
            },
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_assistant_message("public-mail-done", "mail reviewed"),
                    ev_completed("public-mail-done"),
                ]),
            }],
        )
        .await;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("consuming response gate closed"))?;
    timeout(READ_TIMEOUT, followup.wait_for_request()).await?;
    let delivered = timeout(READ_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification = app.read_notification("item/completed").await?;
            if matches!(&completed.item, ThreadItem::UserMessage { client_id: Some(id), .. } if id == CLIENT_ID) {
                break Ok::<_, anyhow::Error>(completed);
            }
        }
    }).await??;
    assert_eq!(
        (&delivered.thread_id, &delivered.turn_id),
        (&thread.id, &started.turn.id)
    );
    let expected = ThreadItem::UserMessage {
        id: delivered.item.id().to_string(),
        client_id: Some(CLIENT_ID.into()),
        content: input,
    };
    assert_eq!(delivered.item, expected);
    timeout(READ_TIMEOUT, async {
        loop {
            let completed: TurnCompletedNotification =
                app.read_notification("turn/completed").await?;
            if completed.thread_id == thread.id && completed.turn.id == started.turn.id {
                assert_eq!(completed.turn.status, TurnStatus::Completed);
                break Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    let inventory: ThreadMailboxReadResponse = app
        .request(|request_id| ClientRequest::ThreadMailboxRead {
            request_id,
            params: ThreadMailboxReadParams {
                thread_id: thread.id.clone(),
            },
        })
        .await?;
    assert_eq!(inventory.pending_total, 0);
    timeout(READ_TIMEOUT, app.shutdown_gracefully()).await??;
    drop(app);

    let mut cold = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let read: ThreadReadResponse = cold
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread.id.clone(),
                include_turns: true,
            },
        })
        .await?;
    assert_eq!(read.thread.history_mode, history_mode);
    let projected = read
        .thread
        .turns
        .iter()
        .flat_map(|turn| {
            turn.items
                .iter()
                .filter(|item| item.id() == expected.id())
                .map(|item| (turn.id.clone(), item.clone()))
        })
        .collect::<Vec<_>>();
    assert_eq!(projected, vec![(started.turn.id, expected)]);
    timeout(READ_TIMEOUT, cold.shutdown_gracefully()).await??;
    server.shutdown().await;
    Ok(())
}
