//! A wait's presentation claim remains owned through canonical and primary delivery.

use super::*;
use crate::agent::control::WaitAgentPresentationCommit;

struct WaitOutcome {
    session: Arc<Session>,
    finished: bool,
}

impl Drop for WaitOutcome {
    fn drop(&mut self) {
        if !self.finished {
            self.session
                .quarantine_history("wait presentation lost its canonical receipt".to_string());
        }
    }
}

impl Session {
    pub(crate) async fn emit_wait_item_completed(
        self: &Arc<Self>,
        turn: &TurnContext,
        item: TurnItem,
        presentation_commit: WaitAgentPresentationCommit,
    ) {
        let session = Arc::clone(self);
        let turn_id = turn.sub_id.clone();
        let worker = tokio::spawn(async move {
            let mut outcome = WaitOutcome {
                session: Arc::clone(&session),
                finished: false,
            };
            let result = async {
                let _transaction = session
                    .services
                    .agent_control
                    .acquire_response_observation_transaction(session.presentation_id())
                    .await;
                let permit = session.acquire_history_publication_barrier().await?;
                let claimed = presentation_commit.claimed_target_turns();
                let snapshots = session
                    .services
                    .agent_control
                    .wait_response_observation_committed_snapshots(
                        session.presentation_id(),
                        &claimed,
                    );
                let commits = claimed
                    .iter()
                    .filter_map(|target| {
                        let observation = snapshots.iter().find(|snapshot| {
                            snapshot.target_thread_id == target.child.thread_id
                                && snapshot.target_turn_id.as_deref()
                                    == Some(target.turn_id.as_str())
                        })?;
                        Some(crate::agent::control::ResponseObservationDeliveryCommit {
                            parent: session.presentation_id(),
                            child: target.child,
                            turn_id: target.turn_id.clone(),
                            response_item_id: observation
                                .final_delivery_response_item_id
                                .clone()?,
                            kind: crate::agent::control::ResponseObservationDeliveryKind::Final,
                            // Wait returns the consumed answer directly to the recipient model.
                            model_visibility: codex_protocol::protocol::SubAgentCompletionModelVisibility::Visible,
                        })
                    })
                    .collect::<Vec<_>>();
                let mut records = snapshots
                    .into_iter()
                    .map(RolloutItem::AgentResponseObservation)
                    .collect::<Vec<_>>();
                let event = Event {
                    id: turn_id.clone(),
                    msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id: session.thread_id,
                        turn_id,
                        item,
                        started_at_ms: None,
                        completed_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
                    }),
                };
                records.insert(0, RolloutItem::EventMsg(event.msg.clone()));
                let control = session.services.agent_control.clone();
                let receiver = session.dispatch_completion_publication(
                    permit,
                    records,
                    vec![event],
                    move |_| {
                        for commit in &commits {
                            control.commit_response_observation_delivery(commit);
                            control.finish_response_observation_turn(
                                commit.parent,
                                commit.child,
                                &commit.turn_id,
                            );
                        }
                    },
                    move || presentation_commit.commit(),
                )?;
                session.publication_result(receiver).await?;
                Ok::<(), CodexErr>(())
            }
            .await;
            if let Err(error) = result {
                session.quarantine_history(format!("wait publication failed: {error}"));
            }
            outcome.finished = true;
        });
        if let Err(error) = worker.await {
            self.quarantine_history(format!("wait publication worker failed: {error}"));
        }
    }
}
