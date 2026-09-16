//! Default TUI discovery shares the selected home; explicit account filtering is server-owned.

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use codex_app_server_client::AppServerClient;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStatusChangedNotification;
use codex_features::Features;
use codex_protocol::ThreadId;
use color_eyre::Result;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

fn saved_thread(id: &str, title: &str) -> Thread {
    serde_json::from_value(json!({
        "id": id, "sessionId": id, "name": title, "preview": title,
        "ephemeral": false, "modelProvider": "openai", "createdAt": 0,
        "updatedAt": 1, "status": {"type": "notLoaded"},
        "cwd": std::env::temp_dir(), "cliVersion": "test", "source": "cli", "turns": [],
    }))
    .expect("saved local thread")
}

type HistoryRequests = Vec<(String, Value)>;

struct HistoryServer {
    remote: AppServerClient,
    server: JoinHandle<Result<HistoryRequests>>,
}

async fn history_server(first: Thread, second: Thread) -> Result<HistoryServer> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut requests = Vec::new();
        while let Some(message) = socket.next().await {
            let Message::Text(text) = message? else {
                continue;
            };
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let params = request.params.unwrap_or_default();
            let result = match request.method.as_str() {
                "initialize" => json!({"userAgent": "shared-history-test", "codexHome": "/unused"}),
                "thread/list" | "thread/search" => {
                    assert!(
                        !params["currentAccountOnly"].as_bool().unwrap_or(false),
                        "ordinary discovery must not silently scope the shared home to one credential"
                    );
                    let data = if request.method == "thread/search" {
                        json!([{"thread": first, "snippet": "Matching text from the first saved profile"}])
                    } else {
                        json!([first, second])
                    };
                    requests.push((request.method.clone(), params));
                    json!({"data": data, "nextCursor": null, "backwardsCursor": null, "readStates": null})
                }
                method => panic!("unexpected mutation or account operation: {method}"),
            };
            socket
                .send(Message::Text(
                    json!({
                        "jsonrpc": "2.0", "id": request.id, "result": result,
                    })
                    .to_string()
                    .into(),
                ))
                .await?;
        }
        Ok(requests)
    });
    Ok(HistoryServer {
        remote: crate::connect_remote_app_server(endpoint).await?,
        server,
    })
}

#[tokio::test]
async fn task_mentions_and_dynamic_thread_lists_keep_shared_home_visibility() -> Result<()> {
    let first_id = ThreadId::new().to_string();
    let second_id = ThreadId::new().to_string();
    let HistoryServer { remote, server } = history_server(
        saved_thread(&first_id, "Task saved by profile one"),
        saved_thread(&second_id, "Task saved by profile two"),
    )
    .await?;
    let handle = remote.request_handle();
    let current = ThreadId::new();
    let (events, mut received) = mpsc::unbounded_channel();
    crate::task_mentions::spawn_search(
        handle.clone(),
        "Task".to_string(),
        current,
        std::env::temp_dir(),
        Arc::new(AtomicU64::new(0)),
        AppEventSender::new(events),
    );
    let matches = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        loop {
            let event = received
                .recv()
                .await
                .expect("search completes before its sender closes");
            if let AppEvent::TaskSearchResult {
                thread_id,
                query,
                matches,
            } = event
            {
                assert_eq!(thread_id, current);
                assert_eq!(query, "Task");
                break matches;
            }
        }
    })
    .await?;
    assert_eq!(
        matches
            .into_iter()
            .map(|row| row.thread_id)
            .collect::<Vec<_>>(),
        vec![first_id.clone(), second_id.clone()]
    );

    let (_status_sender, status) = broadcast::channel::<ThreadStatusChangedNotification>(8);
    for name in ["list_threads", "list_archived_threads"] {
        let response = crate::dynamic_tools::execute(
            handle.clone(),
            DynamicToolCallParams {
                thread_id: current.to_string(),
                turn_id: "current-turn".to_string(),
                call_id: name.to_string(),
                namespace: Some("codex_tui".to_string()),
                tool: name.to_string(),
                arguments: json!({}),
            },
            ThreadStartParams::default(),
            Features::with_defaults(),
            status.resubscribe(),
            /*app_event_tx*/ None,
        )
        .await;
        assert!(response.success, "{name}: {response:?}");
        let [DynamicToolCallOutputContentItem::InputText { text }] =
            response.content_items.as_slice()
        else {
            panic!("expected JSON thread list");
        };
        let data: Value = serde_json::from_str(text)?;
        let ids = data["threads"]
            .as_array()
            .expect("threads")
            .iter()
            .map(|thread| thread["id"].as_str().expect("thread ID"))
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![first_id.as_str(), second_id.as_str()]);
    }
    drop(handle);
    remote.shutdown().await?;
    let requests = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), server).await???;
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests
            .iter()
            .filter(|(method, _)| method == "thread/search")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(_, params)| params["archived"] == true)
            .count(),
        1
    );
    Ok(())
}
