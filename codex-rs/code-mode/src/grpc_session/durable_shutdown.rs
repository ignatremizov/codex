//! Durable closure retains transport identity and retries unacknowledged closes.

use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::grpc;
use tokio::sync::watch;

use super::SessionInner;
use super::deadline;
use crate::remote_session::ShutdownResultReceiver;

impl SessionInner {
    pub(super) fn request_durable_shutdown(self: &Arc<Self>) -> ShutdownResultReceiver {
        self.durable_requested.store(true, Ordering::Release);
        self.shutdown_requested.store(true, Ordering::Release);
        let mut attempt = self
            .durable_result
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(receiver) = attempt.as_ref()
            && !matches!(&*receiver.borrow(), Some(Err(_)))
        {
            return receiver.clone();
        }
        let (sender, receiver) = watch::channel(None);
        *attempt = Some(receiver.clone());
        let inner = Arc::clone(self);
        self.runtime.spawn(async move {
            let result = if inner.close_confirmed.load(Ordering::Acquire) {
                Ok(())
            } else {
                let mut client = inner.client();
                let result = deadline::startup(
                    "durable session shutdown",
                    client.close_session(grpc::CloseSessionRequest {
                        session_id: inner.id.clone(),
                    }),
                )
                .await
                .map(|_| ());
                if result.is_ok() {
                    inner.close_confirmed.store(true, Ordering::Release);
                }
                result
            };
            inner.close_state(/*failure*/ None);
            // Stream tasks own admission; no callback can be registered after they end.
            inner.stream_tasks.wait().await;
            inner.callback_tasks.close();
            inner.callback_tasks.wait().await;
            // An operation admitted before close may register its execution stream while
            // the first stream drain is completing. Its callback token fences this pass.
            inner.stream_tasks.wait().await;
            let result = result.and_then(|()| {
                if inner.callback_panicked.load(Ordering::Acquire) {
                    Err("accepted gRPC code-mode callback panicked".to_string())
                } else {
                    Ok(())
                }
            });
            sender.send_replace(Some(result));
        });
        receiver
    }
}
