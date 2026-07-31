//! Deferred runtime publication must preserve host-tier acknowledgement ownership.

use super::*;
use crate::agent::host::AgentRuntimeOwnership;
use crate::agent::host::AgentServiceTier;
use crate::agent::host::AgentTurnAdmission;
use crate::agent::host::AgentTurnAdmissionRequest;
use crate::agent::types::AgentExecutionGuard;
use crate::session::SessionSettingsUpdate;
use crate::session::step_settings::StepSettingsUpdate;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

struct NoTurns;
impl AgentTurnAdmission for NoTurns {
    fn admit_turn(
        &self,
        _request: AgentTurnAdmissionRequest,
    ) -> BoxFuture<'_, CodexResult<AgentExecutionGuard>> {
        Box::pin(async {
            Err(CodexErr::InvalidRequest(
                "no execution in restoration test".into(),
            ))
        })
    }
}

struct GatedTier {
    updates: mpsc::UnboundedSender<(Option<String>, oneshot::Sender<()>)>,
}
impl AgentServiceTier for GatedTier {
    fn current(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { Some("priority".to_string()) })
    }
    fn update(&self, value: Option<String>) -> BoxFuture<'static, Result<(), String>> {
        let (accept, accepted) = oneshot::channel();
        let _ = self.updates.send((value, accept));
        Box::pin(async move {
            accepted
                .await
                .map_err(|_| "acceptance channel closed".to_string())
        })
    }
}

#[tokio::test]
async fn staged_host_tier_publishes_once_after_registration_and_waits_outside_locks() {
    for cancel_wait in [false, true] {
        let (_home, manager, seed, unused) = deferred_publication_fixture().await;
        unused
            .thread
            .shutdown_and_wait()
            .await
            .expect("close unused staged fixture");
        let (updates, mut handoffs) = mpsc::unbounded_channel();
        let mut config = seed.thread.config().await.as_ref().clone();
        config.service_tier = Some("priority".into());
        let mut options = StartThreadOptions {
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(config)
        };
        options
            .thread_extension_init
            .insert(AgentRuntimeOwnership::Host);
        options
            .thread_extension_init
            .insert(Arc::new(NoTurns) as Arc<dyn AgentTurnAdmission>);
        options
            .thread_extension_init
            .insert(Arc::new(GatedTier { updates }) as Arc<dyn AgentServiceTier>);
        let mut request = ThreadSpawnRequest::new(
            options,
            Arc::clone(&manager.state.auth_manager),
            seed.thread
                .session
                .services
                .local_agent_runtime
                .control(seed.thread.session.session_id()),
        );
        request.registration = ThreadRegistrationMode::Deferred;
        let prepared = tokio::time::timeout(
            std::time::Duration::from_secs(/*secs*/ 5),
            manager.state.spawn_thread(request),
        )
        .await
        .expect("preparation must not await a host receipt")
        .expect("prepared host root");
        assert!(manager.get_thread(prepared.thread_id).await.is_err());
        assert!(handoffs.try_recv().is_err());
        manager
            .state
            .publish_restored_thread(&prepared.thread, /*parent*/ None, || {
                Err(CodexErr::InvalidRequest("metadata refused".into()))
            })
            .await
            .expect_err("uncommitted metadata cannot publish settings");
        assert!(handoffs.try_recv().is_err());
        assert!(manager.get_thread(prepared.thread_id).await.is_err());
        let mut publication = Box::pin(manager.state.publish_restored_thread(
            &prepared.thread,
            /*parent*/ None,
            || Ok(()),
        ));
        let (value, accept) =
            tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), async {
                tokio::select! {
                    _ = &mut publication => panic!("publication returned before host acceptance"),
                    handoff = handoffs.recv() => handoff.expect("committed tier handoff"),
                }
            })
            .await
            .expect("bounded publication handoff");
        assert_eq!(value, Some("priority".into()));
        assert!(Arc::ptr_eq(
            &manager
                .get_thread(prepared.thread_id)
                .await
                .expect("registered"),
            &prepared.thread
        ));
        assert!(
            manager.state.threads.try_write().is_ok(),
            "host receipt cannot hold manager lock"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(/*secs*/ 5),
                prepared.thread.session.lock_pending_service_tier(),
            )
            .await
            .expect("host receipt must not retain the session-state lock")
            .is_none(),
            "committed publication must clear the pending tier",
        );
        if cancel_wait {
            drop(publication);
            // A later explicit same-value settings commit must recover the registered
            // runtime's original receipt without invoking the host a second time.
            let receipt = tokio::time::timeout(
                std::time::Duration::from_secs(/*secs*/ 5),
                prepared
                    .thread
                    .session
                    .update_settings(SessionSettingsUpdate {
                        step_settings: StepSettingsUpdate {
                            service_tier: Some(Some("priority".to_string())),
                            ..Default::default()
                        },
                        host_acceptance_in_reply: true,
                        ..Default::default()
                    }),
            )
            .await
            .expect("local commit does not wait for host acceptance")
            .expect("same captured tier")
            .host_acceptance
            .expect("registered runtime retained the original receipt");
            assert!(
                handoffs.try_recv().is_err(),
                "same-value commit must not republish"
            );
            accept
                .send(())
                .expect("registered runtime retains acceptance receipt");
            tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), receipt)
                .await
                .expect("bounded retained receipt")
                .expect("host accepted after waiter cancellation");
        } else {
            accept.send(()).expect("accept committed tier");
            publication.await.expect("publication completed");
        }
        manager
            .state
            .publish_restored_thread(&prepared.thread, /*parent*/ None, || Ok(()))
            .await
            .expect("warm checked publication");
        assert!(
            handoffs.try_recv().is_err(),
            "warm attach must not republish the same pending tier"
        );
        prepared
            .thread
            .shutdown_and_wait()
            .await
            .expect("shutdown prepared root");
        seed.thread
            .shutdown_and_wait()
            .await
            .expect("shutdown seed");
    }
}
