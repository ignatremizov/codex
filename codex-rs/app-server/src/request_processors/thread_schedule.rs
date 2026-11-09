//! Cold loading for due queue entries. Loaded shared/embedded owners dispatch normally.

use super::ThreadRequestProcessor;
use super::ThreadResumeTarget;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::turn_admission::TurnAdmission;
use codex_app_server_protocol::ThreadResumeParams;
use codex_protocol::ThreadId;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_queue_extension::QueuedItemService;
use codex_thread_store::ReadThreadParams;
use std::collections::HashMap;
use std::sync::Weak;
use std::time::Duration;

const MAX_CONCURRENT_RESUMES: usize = 4;

pub(crate) fn spawn(
    processor: Weak<ThreadRequestProcessor>,
    service: Weak<QueuedItemService>,
    admission: TurnAdmission,
) {
    tokio::spawn(async move {
        let mut pending: HashMap<ThreadId, tokio::task::JoinHandle<()>> = HashMap::new();
        let mut interval = tokio::time::interval(Duration::from_secs(/*secs*/ 10));
        let mut after = None;
        loop {
            interval.tick().await;
            let (Some(processor), Some(service)) = (processor.upgrade(), service.upgrade()) else {
                return;
            };
            pending.retain(|_, task| !task.is_finished());
            let available = MAX_CONCURRENT_RESUMES.saturating_sub(pending.len());
            if available == 0 {
                continue;
            }
            let profile = processor.config.auth_file_selection.profile_identity(
                &processor.config.codex_home,
                processor.config.cli_auth_credentials_store_mode,
            );
            let targets = match service
                .scheduled_threads(profile.profile_opaque_id.clone(), after.take(), available)
                .await
            {
                Ok(targets) => targets,
                Err(error) => {
                    tracing::warn!(%error, "failed to discover due input");
                    continue;
                }
            };
            after = targets.last().map(ToString::to_string);
            for thread_id in targets {
                if pending.contains_key(&thread_id)
                    || processor.thread_manager.get_thread(thread_id).await.is_ok()
                {
                    continue;
                }
                let processor = processor.clone();
                let service = service.clone();
                let profile = profile.profile_opaque_id.clone();
                let Ok(permit) = admission.admit() else {
                    return;
                };
                let task = tokio::spawn(async move {
                    let _permit = permit;
                    let interrupted_turn = None;
                    let result = async {
                        let stored = processor
                            .thread_store
                            .read_thread(ReadThreadParams {
                                thread_id,
                                include_archived: true,
                                include_history: false,
                            })
                            .await
                            .map_err(|error| internal_error(error.to_string()))?;
                        if stored.archived_at.is_some() {
                            return Err(invalid_request(format!("session {thread_id} is archived")));
                        }
                        if matches!(
                            stored.source,
                            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                        ) {
                            return Err(invalid_request(
                                "scheduled input cannot restore an unloaded spawned sub-agent; resume its owning tree first",
                            ));
                        }
                        processor
                            .thread_resume(
                                ThreadResumeTarget::DaemonRecovery(interrupted_turn),
                                ThreadResumeParams {
                                    thread_id: thread_id.to_string(),
                                    exclude_turns: true,
                                    ..Default::default()
                                },
                                /*app_server_client_name*/ None,
                                /*app_server_client_version*/ None,
                                ClientMcpExtensions::default(),
                            )
                            .await
                    }
                    .await;
                    if let Err(error) = result
                        && let Err(storage_error) = service
                            .note_dispatch_error(thread_id, &profile, error.message)
                            .await
                    {
                        tracing::warn!(%storage_error, "failed to record scheduled-input error");
                    }
                });
                pending.insert(thread_id, task);
            }
        }
    });
}
