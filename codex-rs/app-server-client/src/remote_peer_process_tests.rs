use std::io;

use futures::SinkExt;
use futures::StreamExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use super::RemoteAppServerClient;
use super::RemoteAppServerConnectArgs;
use super::RemoteAppServerEndpoint;
use crate::AppServerClient;

async fn serve_connection(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
) -> io::Result<()> {
    let mut stream = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(io::Error::other)?;
    let request = stream
        .next()
        .await
        .ok_or_else(|| io::Error::other("initialize missing"))?
        .map_err(io::Error::other)?;
    let request: serde_json::Value =
        serde_json::from_str(request.to_text().map_err(io::Error::other)?)?;
    stream
        .send(Message::Text(
            serde_json::json!({
                "id": request["id"],
                "result": {"userAgent": "codex/1.0"}
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(io::Error::other)?;
    while let Some(message) = stream.next().await {
        if matches!(message, Ok(Message::Close(_))) {
            break;
        }
    }
    Ok(())
}

#[tokio::test]
async fn loopback_websocket_never_has_a_local_process_monitor() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        serve_connection(stream).await
    });
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: format!("ws://{address}"),
            auth_token: None,
        },
        client_name: "peer-monitor-test".to_string(),
        client_version: "1.0".to_string(),
        experimental_api: false,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 8,
    })
    .await?;
    assert!(client.local_peer_process().is_none());
    let client = AppServerClient::Remote(client);
    assert!(client.local_peer_process().is_none());
    client.shutdown().await?;
    server.await.map_err(io::Error::other)??;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn local_client_retains_monitor_after_shutdown_without_claiming_death() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("peer.sock");
    let mut listener = codex_uds::UnixListener::bind(&socket).await?;
    let server = tokio::spawn(async move { serve_connection(listener.accept().await?).await });
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(socket)?,
        },
        client_name: "peer-monitor-test".to_string(),
        client_version: "1.0".to_string(),
        experimental_api: false,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 8,
    })
    .await?;
    let client = AppServerClient::Remote(client);
    let monitor = client
        .local_peer_process()
        .ok_or_else(|| io::Error::other("local peer monitor unavailable"))?;
    assert!(!monitor.has_exited()?);
    client.shutdown().await?;
    server.await.map_err(io::Error::other)??;
    assert!(!monitor.has_exited()?);
    assert!(!monitor.clone().has_exited()?);
    Ok(())
}
