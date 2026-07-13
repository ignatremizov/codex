//! Local durability and asynchronous host acceptance are distinct reply boundaries.

use super::reply_after_host_acceptance;
use codex_protocol::error::CodexErr;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::oneshot;

#[test_case::test_case(false; "publication_first")]
#[test_case::test_case(true; "host_first")]
#[tokio::test]
async fn settings_reply_requires_both_acceptances(host_first: bool) {
    let (host_sender, host_receiver) = oneshot::channel();
    let acceptance = async move { host_receiver.await.expect("host result") }
        .boxed()
        .shared();
    let (reply, mut response) = oneshot::channel();
    let publication = reply_after_host_acceptance(reply, acceptance);
    let mut host_sender = Some(host_sender);
    let mut publication = Some(publication);
    if host_first {
        host_sender
            .take()
            .expect("host sender")
            .send(Ok(()))
            .expect("host live");
    } else {
        publication
            .take()
            .expect("publication sender")
            .send(Ok(()))
            .expect("publication live");
    }
    tokio::task::yield_now().await;
    assert!(matches!(
        response.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    if let Some(host) = host_sender {
        host.send(Ok(())).expect("host live");
    }
    if let Some(publication) = publication {
        publication.send(Ok(())).expect("publication live");
    }
    tokio::time::timeout(Duration::from_secs(2), response)
        .await
        .expect("reply completes")
        .expect("reply present")
        .expect("both accepted");
}

#[tokio::test]
async fn failed_publication_replies_without_waiting_for_host() {
    let polls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&polls);
    let acceptance = async move {
        observed.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<Result<(), String>>().await
    }
    .boxed()
    .shared();
    let (reply, response) = oneshot::channel();
    let publication = reply_after_host_acceptance(reply, acceptance);
    publication
        .send(Err(CodexErr::Fatal("flush failed".into())))
        .expect("publication live");
    let result = tokio::time::timeout(Duration::from_secs(2), response)
        .await
        .expect("failure must not await host")
        .expect("failure reply");
    assert!(result.is_err_and(|error| error.to_string().contains("flush failed")));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_host_acceptance_does_not_claim_local_rollback() {
    let acceptance = async { Err("host rejected tier".to_string()) }
        .boxed()
        .shared();
    let (reply, response) = oneshot::channel();
    let publication = reply_after_host_acceptance(reply, acceptance);
    publication.send(Ok(())).expect("publication live");
    let result = tokio::time::timeout(Duration::from_secs(2), response)
        .await
        .expect("host failure reply")
        .expect("reply present");
    assert!(result.is_err_and(|error| {
        let message = error.to_string();
        message.contains("committed locally") && message.contains("host rejected tier")
    }));
}
