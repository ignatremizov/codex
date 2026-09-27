//! Exercise recording-proxy I/O with real in-memory WebSocket transports.

use super::recording_peer_io;
use color_eyre::eyre::Result;
use color_eyre::eyre::eyre;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::io::ErrorKind;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::io::duplex;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::Role;

const TIMEOUT: Duration = Duration::from_secs(/*secs*/ 5);

#[tokio::test]
async fn recording_peer_io_preserves_successful_notification_delivery() -> Result<()> {
    let (server, client) = duplex(/*max_buf_size*/ 1024);
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, /*config*/ None).await;
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, /*config*/ None).await;
    let notification = Message::Text(r#"{"method":"thread/settings/updated"}"#.into());
    let sent = timeout(TIMEOUT, server.send(notification.clone())).await?;
    assert_eq!(recording_peer_io(sent)?, Some(()));
    let received = timeout(TIMEOUT, client.next())
        .await?
        .ok_or_else(|| eyre!("notification stream closed"))?;
    assert_eq!(recording_peer_io(received)?, Some(notification));
    Ok(())
}

#[tokio::test]
async fn recording_peer_io_finishes_when_a_notification_write_loses_its_peer() -> Result<()> {
    let (server, client) = duplex(/*max_buf_size*/ 1024);
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, /*config*/ None).await;
    drop(client);
    let sent = timeout(
        TIMEOUT,
        server.send(Message::Text("queued notification".into())),
    )
    .await?;
    assert!(
        matches!(&sent, Err(WebSocketError::Io(error)) if error.kind() == ErrorKind::BrokenPipe)
    );
    assert_eq!(recording_peer_io(sent)?, None);
    Ok(())
}

#[tokio::test]
async fn recording_peer_io_finishes_when_a_read_loses_its_peer() -> Result<()> {
    let (server, client) = duplex(/*max_buf_size*/ 1024);
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, /*config*/ None).await;
    drop(client);
    let received = timeout(TIMEOUT, server.next())
        .await?
        .ok_or_else(|| eyre!("peer loss must produce the WebSocket close error"))?;
    assert!(matches!(
        &received,
        Err(WebSocketError::Protocol(
            ProtocolError::ResetWithoutClosingHandshake
        ))
    ));
    assert_eq!(recording_peer_io(received)?, None);
    Ok(())
}

#[tokio::test]
async fn recording_peer_io_preserves_invalid_frame_errors() -> Result<()> {
    let (server, mut client) = duplex(/*max_buf_size*/ 1024);
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, /*config*/ None).await;
    // Client frames must be masked. This is a live protocol failure, not peer shutdown.
    timeout(TIMEOUT, client.write_all(&[0x81, 0x01, b'x'])).await??;
    let received = timeout(TIMEOUT, server.next())
        .await?
        .ok_or_else(|| eyre!("malformed frame must produce a protocol error"))?;
    assert!(matches!(
        recording_peer_io(received),
        Err(WebSocketError::Protocol(
            ProtocolError::UnmaskedFrameFromClient
        ))
    ));
    Ok(())
}
