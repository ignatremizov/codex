use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_diagnostics::Gauge;
use codex_diagnostics::GaugeGuard;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

static PENDING_MAILBOX_MESSAGES: Gauge = Gauge::new("core.mailbox.pending");

/// Input consumed by a regular turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
    },
    FunctionCallOutput(ResponseItem),
    // Preserve the existing serialized format while carrying injection API metadata
    // through the in-memory queue.
    ResponseItem(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    InterAgentCommunication(InterAgentCommunication),
}

mod turn_input_response_item {
    use super::ResponseItem;
    use super::ResponseItemEnvelope;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serialize;
    use serde::Serializer;
    use serde::ser::Error as _;

    pub(super) fn serialize<S>(
        item: &ResponseItemEnvelope,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if item.metadata.is_some() {
            return Err(S::Error::custom(
                "annotated response items cannot cross the turn-input serialization boundary",
            ));
        }
        item.item.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<ResponseItemEnvelope, D::Error>
    where
        D: Deserializer<'de>,
    {
        ResponseItem::deserialize(deserializer).map(ResponseItemEnvelope::new)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputQueueActivity {
    Mailbox,
    Steer,
}

/// Whether the current turn-local queue requires another model sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingInputFollowUp {
    Required,
    DeferredAtMcpBoundary,
    NoTurnLocalFollowUp { accepts_mailbox_delivery: bool },
}

pub(super) enum CompletionCommunicationCommit {
    Ordinary,
    Started(ResponseItemId),
    AlreadyStarted,
}

pub(super) struct ShutdownCompletionCommunications {
    pub(super) communications: Vec<InterAgentCommunication>,
    pub(super) has_committing: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionMailPhase {
    Leased,
    Committing,
}

struct InFlightCompletionMail {
    mail: PendingMailboxCommunication,
    sequence: u64,
    phase: CompletionMailPhase,
}

#[derive(Default)]
struct MailboxState {
    pending_mails: VecDeque<PendingMailboxCommunication>,
    in_flight_completion_mails: HashMap<ResponseItemId, InFlightCompletionMail>,
    next_sequence: u64,
}

/// Turn-local pending input storage owned by the input queue flow.
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    items: Vec<TurnInput>,
}

impl TurnInputQueue {
    pub(crate) fn append_to_front(&mut self, mut items: Vec<TurnInput>) {
        if items.is_empty() {
            return;
        }
        items.append(&mut self.items);
        self.items = items;
    }

    pub(crate) fn as_slice(&self) -> &[TurnInput] {
        &self.items
    }

    pub(crate) fn take(&mut self) -> Vec<TurnInput> {
        std::mem::take(&mut self.items)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

pub(crate) fn classify_pending_input_follow_up(turn_state: &TurnState) -> PendingInputFollowUp {
    let accepts_mailbox_delivery = turn_state.accepts_mailbox_delivery_for_current_turn();
    for item in turn_state.pending_input() {
        if is_mcp_server_use_context_input_item(item) {
            return PendingInputFollowUp::DeferredAtMcpBoundary;
        }
        if !matches!(
            item,
            TurnInput::InterAgentCommunication(communication)
                if !communication.trigger_turn && !accepts_mailbox_delivery
        ) {
            return PendingInputFollowUp::Required;
        }
    }
    PendingInputFollowUp::NoTurnLocalFollowUp {
        accepts_mailbox_delivery,
    }
}

pub(super) fn is_mcp_server_use_context_input_item(item: &TurnInput) -> bool {
    let TurnInput::ResponseItem(item) = item else {
        return false;
    };
    crate::context::McpServerUseInstructions::matches_response_item(item)
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    mailbox: Mutex<MailboxState>,
    idle_pending_input: Mutex<Vec<TurnInput>>,
}

struct PendingMailboxCommunication {
    communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    _diagnostics_guard: GaugeGuard,
}

impl Clone for PendingMailboxCommunication {
    fn clone(&self) -> Self {
        Self {
            communication: self.communication.clone(),
            start_options: self.start_options.clone(),
            _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
        }
    }
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            mailbox: Mutex::new(MailboxState::default()),
            idle_pending_input: Mutex::new(Vec::new()),
        }
    }

    pub(crate) async fn subscribe_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let activity_rx = self.activity_tx.subscribe();
        let (has_pending_steer, has_staged_mailbox_input) = if let Some(turn_state) = turn_state {
            let turn_state = turn_state.lock().await;
            (
                turn_state.pending_input.has_pending_input(),
                turn_state
                    .pending_input
                    .as_slice()
                    .iter()
                    .any(|input| matches!(input, TurnInput::InterAgentCommunication(_))),
            )
        } else {
            (false, false)
        };
        let pending_activity = if has_pending_steer {
            Some(InputQueueActivity::Steer)
        } else if has_staged_mailbox_input || self.has_pending_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        (activity_rx, pending_activity)
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) {
        self.mailbox
            .lock()
            .await
            .pending_mails
            .push_back(PendingMailboxCommunication {
                communication,
                start_options,
                _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
            });
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        !self.mailbox.lock().await.pending_mails.is_empty()
    }

    pub(crate) async fn pending_mailbox_response_item_ids(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> Vec<ResponseItemId> {
        let mut response_item_ids = if let Some(turn_state) = turn_state {
            turn_state
                .lock()
                .await
                .pending_input
                .as_slice()
                .iter()
                .filter_map(|input| match input {
                    TurnInput::InterAgentCommunication(communication) => communication.id.clone(),
                    TurnInput::UserInput { .. }
                    | TurnInput::FunctionCallOutput(_)
                    | TurnInput::ResponseItem(_) => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        response_item_ids.extend(
            self.mailbox
                .lock()
                .await
                .pending_mails
                .iter()
                .filter_map(|mail| mail.communication.id.clone()),
        );
        response_item_ids
    }

    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        self.mailbox
            .lock()
            .await
            .pending_mails
            .iter()
            .any(|mail| mail.communication.trigger_turn)
    }

    pub(crate) async fn drain_mailbox_input_items(&self) -> (Vec<TurnInput>, TurnStartOptions) {
        let mut mailbox = self.mailbox.lock().await;
        let pending_mails = mailbox.pending_mails.drain(..).collect::<Vec<_>>();
        // A later follow-up supersedes the earlier choice, including an omitted choice.
        let mut start_options = pending_mails
            .iter()
            .rev()
            .find(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.clone())
            .unwrap_or_default();
        start_options.parent_turn_id = pending_mails
            .iter()
            .filter(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.parent_turn_id.as_deref())
            .reduce(|expected, candidate| expected.filter(|id| candidate == Some(*id)))
            .and_then(|id| id.filter(|id| !id.trim().is_empty()).map(str::to_string));
        start_options.root_turn_id = pending_mails
            .iter()
            .find(|mail| mail.communication.trigger_turn)
            .and_then(|mail| {
                mail.start_options
                    .parent_turn_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .and(mail.start_options.root_turn_id.as_deref())
                    .filter(|id| !id.trim().is_empty())
            })
            .map(str::to_string);
        Self::lease_completion_communications(&mut mailbox, &pending_mails);
        let items = pending_mails
            .into_iter()
            .map(|mail| TurnInput::InterAgentCommunication(mail.communication))
            .collect();
        (items, start_options)
    }

    pub(super) async fn drain_completion_communications_for_shutdown(
        &self,
    ) -> ShutdownCompletionCommunications {
        let mut completion_mails = {
            let mut queued_input = self.idle_pending_input.lock().await;
            let mut completion_mails = Vec::new();
            let mut retained_input = Vec::new();
            for input in std::mem::take(&mut *queued_input) {
                match input {
                    TurnInput::InterAgentCommunication(communication)
                        if completion_context_response_item_id(&communication).is_some() =>
                    {
                        completion_mails.push(PendingMailboxCommunication {
                            communication,
                            start_options: TurnStartOptions::default(),
                            _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
                        });
                    }
                    input => retained_input.push(input),
                }
            }
            *queued_input = retained_input;
            completion_mails
        };
        let mut mailbox = self.mailbox.lock().await;
        Self::restore_completion_communications(&mut mailbox, Vec::new());
        let (mailbox_completions, ordinary_mails): (Vec<_>, Vec<_>) = mailbox
            .pending_mails
            .drain(..)
            .partition(|mail| completion_context_response_item_id(&mail.communication).is_some());
        mailbox.pending_mails.extend(ordinary_mails);
        completion_mails.extend(mailbox_completions);
        let mut response_item_ids = HashSet::new();
        completion_mails.retain(|mail| {
            completion_context_response_item_id(&mail.communication)
                .is_some_and(|response_item_id| response_item_ids.insert(response_item_id))
        });
        Self::lease_completion_communications(&mut mailbox, &completion_mails);
        let has_committing = mailbox
            .in_flight_completion_mails
            .values()
            .any(|in_flight| in_flight.phase == CompletionMailPhase::Committing);
        ShutdownCompletionCommunications {
            communications: completion_mails
                .into_iter()
                .map(|mail| mail.communication)
                .collect(),
            has_committing,
        }
    }

    fn lease_completion_communications(
        mailbox: &mut MailboxState,
        mails: &[PendingMailboxCommunication],
    ) {
        for mail in mails {
            let Some(response_item_id) = completion_context_response_item_id(&mail.communication)
            else {
                continue;
            };
            if mailbox
                .in_flight_completion_mails
                .contains_key(&response_item_id)
            {
                continue;
            }
            let sequence = mailbox.next_sequence;
            mailbox.next_sequence = mailbox.next_sequence.wrapping_add(1);
            mailbox.in_flight_completion_mails.insert(
                response_item_id,
                InFlightCompletionMail {
                    mail: mail.clone(),
                    sequence,
                    phase: CompletionMailPhase::Leased,
                },
            );
        }
    }

    pub(super) async fn begin_completion_communication_commit(
        &self,
        communication: &InterAgentCommunication,
    ) -> CompletionCommunicationCommit {
        let Some(response_item_id) = completion_context_response_item_id(communication) else {
            return CompletionCommunicationCommit::Ordinary;
        };
        let mut mailbox = self.mailbox.lock().await;
        if mailbox.pending_mails.iter().any(|pending| {
            completion_context_response_item_id(&pending.communication).as_ref()
                == Some(&response_item_id)
        }) {
            return CompletionCommunicationCommit::AlreadyStarted;
        }
        if let Some(in_flight) = mailbox
            .in_flight_completion_mails
            .get_mut(&response_item_id)
        {
            if in_flight.phase == CompletionMailPhase::Committing {
                return CompletionCommunicationCommit::AlreadyStarted;
            }
            in_flight.phase = CompletionMailPhase::Committing;
        } else {
            let sequence = mailbox.next_sequence;
            mailbox.next_sequence = mailbox.next_sequence.wrapping_add(1);
            mailbox.in_flight_completion_mails.insert(
                response_item_id.clone(),
                InFlightCompletionMail {
                    mail: PendingMailboxCommunication {
                        communication: communication.clone(),
                        start_options: TurnStartOptions::default(),
                        _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
                    },
                    sequence,
                    phase: CompletionMailPhase::Committing,
                },
            );
        }
        CompletionCommunicationCommit::Started(response_item_id)
    }

    pub(super) async fn acknowledge_completion_communication(
        &self,
        response_item_id: &ResponseItemId,
    ) {
        self.mailbox
            .lock()
            .await
            .in_flight_completion_mails
            .remove(response_item_id);
    }

    pub(super) async fn retry_completion_communication(&self, response_item_id: &ResponseItemId) {
        let mut mailbox = self.mailbox.lock().await;
        let Some(in_flight) = mailbox.in_flight_completion_mails.get_mut(response_item_id) else {
            return;
        };
        in_flight.phase = CompletionMailPhase::Leased;
        let restored = Self::restore_completion_communications(&mut mailbox, Vec::new());
        drop(mailbox);
        if restored {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
    }

    fn restore_completion_communications(
        mailbox: &mut MailboxState,
        staged: Vec<InterAgentCommunication>,
    ) -> bool {
        let mut restored = mailbox
            .in_flight_completion_mails
            .iter()
            .filter_map(|(response_item_id, in_flight)| {
                (in_flight.phase == CompletionMailPhase::Leased).then_some((
                    response_item_id.clone(),
                    in_flight.sequence,
                    in_flight.mail.clone(),
                ))
            })
            .collect::<Vec<_>>();
        let mut restored_ids = restored
            .iter()
            .map(|(response_item_id, _, _)| response_item_id.clone())
            .collect::<HashSet<_>>();
        for communication in staged {
            let Some(response_item_id) = completion_context_response_item_id(&communication) else {
                continue;
            };
            if restored_ids.contains(&response_item_id)
                || mailbox
                    .in_flight_completion_mails
                    .get(&response_item_id)
                    .is_some_and(|in_flight| in_flight.phase == CompletionMailPhase::Committing)
            {
                continue;
            }
            let sequence = mailbox.next_sequence;
            mailbox.next_sequence = mailbox.next_sequence.wrapping_add(1);
            restored_ids.insert(response_item_id.clone());
            restored.push((
                response_item_id,
                sequence,
                PendingMailboxCommunication {
                    communication,
                    start_options: TurnStartOptions::default(),
                    _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
                },
            ));
        }
        if restored.is_empty() {
            return false;
        }
        restored.sort_by_key(|(_, sequence, _)| *sequence);
        for (response_item_id, _, _) in &restored {
            mailbox.in_flight_completion_mails.remove(response_item_id);
        }
        let mut pending_ids = mailbox
            .pending_mails
            .iter()
            .filter_map(|mail| completion_context_response_item_id(&mail.communication))
            .collect::<HashSet<_>>();
        let mut restored_any = false;
        for (response_item_id, _, mail) in restored.into_iter().rev() {
            if pending_ids.insert(response_item_id) {
                mailbox.pending_mails.push_front(mail);
                restored_any = true;
            }
        }
        restored_any
    }

    #[cfg(test)]
    pub(crate) async fn queue_response_items_for_next_turn(&self, items: Vec<ResponseItem>) {
        let items = items
            .into_iter()
            .map(|item| TurnInput::ResponseItem(item.into()))
            .collect::<Vec<_>>();
        self.queue_turn_inputs_for_next_turn(items).await;
    }

    pub(crate) async fn queue_turn_inputs_for_next_turn(&self, items: Vec<TurnInput>) {
        if items.is_empty() {
            return;
        }

        self.idle_pending_input.lock().await.extend(items);
    }

    pub(crate) async fn take_queued_items_for_next_turn(&self) -> Vec<TurnInput> {
        std::mem::take(&mut *self.idle_pending_input.lock().await)
    }

    pub(crate) async fn has_queued_turn_inputs(&self) -> bool {
        !self.idle_pending_input.lock().await.is_empty()
    }

    pub(crate) async fn queued_response_items_for_next_turn(&self) -> Vec<ResponseItem> {
        self.idle_pending_input
            .lock()
            .await
            .iter()
            .filter_map(|item| match item {
                TurnInput::FunctionCallOutput(item) => Some(item.clone()),
                TurnInput::ResponseItem(item) => Some(item.item.clone()),
                TurnInput::UserInput { .. } | TurnInput::InterAgentCommunication(_) => None,
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) async fn has_queued_response_items_for_next_turn(&self) -> bool {
        self.idle_pending_input
            .lock()
            .await
            .iter()
            .any(|item| {
                matches!(
                    item,
                    TurnInput::FunctionCallOutput(_) | TurnInput::ResponseItem(_)
                )
            })
    }

    pub(crate) async fn turn_state_for_sub_id(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) -> Option<Arc<Mutex<TurnState>>> {
        let active = active_turn.lock().await;
        active.as_ref().and_then(|active_turn| {
            active_turn
                .task
                .as_ref()
                .is_some_and(|task| task.turn_context.sub_id == sub_id)
                .then(|| Arc::clone(&active_turn.turn_state))
        })
    }

    /// Clear any pending waiters and input buffered for the current turn.
    pub(crate) async fn clear_pending(&self, active_turn: &ActiveTurn) {
        let completion_communications = {
            let mut turn_state = active_turn.turn_state.lock().await;
            turn_state.clear_pending_waiters();
            turn_state
                .pending_input
                .take()
                .into_iter()
                .filter_map(|input| match input {
                    TurnInput::InterAgentCommunication(communication)
                        if communication.id.as_ref().is_some_and(|id| {
                            is_sub_agent_completion_context_response_item_id(id.as_str())
                        }) =>
                    {
                        Some(communication)
                    }
                    TurnInput::UserInput { .. }
                    | TurnInput::FunctionCallOutput(_)
                    | TurnInput::ResponseItem(_)
                    | TurnInput::InterAgentCommunication(_) => None,
                })
                .collect::<Vec<_>>()
        };
        let restored = {
            let mut mailbox = self.mailbox.lock().await;
            Self::restore_completion_communications(&mut mailbox, completion_communications)
        };
        if restored {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
    }

    pub(crate) async fn defer_mailbox_delivery_to_next_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        let mut turn_state = turn_state.lock().await;
        // Explicit same-turn work still needs a follow-up. Queue-only child mail and MCP-use
        // context do not: task completion persists them for the next turn without sampling again.
        if turn_state
            .pending_input
            .items
            .iter()
            .any(|input| match input {
                TurnInput::InterAgentCommunication(communication) => communication.trigger_turn,
                TurnInput::ResponseItem(item) => {
                    !crate::context::McpServerUseInstructions::matches_response_item(item)
                }
                TurnInput::FunctionCallOutput(_) => true,
                TurnInput::UserInput { .. } => true,
            })
        {
            return;
        }
        turn_state.set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);
    }

    pub(crate) async fn accept_mailbox_delivery_for_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        self.accept_mailbox_delivery_for_turn_state(turn_state.as_ref())
            .await;
    }

    pub(super) async fn accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) {
        turn_state
            .lock()
            .await
            .accept_mailbox_delivery_for_current_turn();
    }

    pub(super) async fn extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        {
            let mut turn_state = turn_state.lock().await;
            turn_state.pending_input.items.extend(input);
            turn_state.accept_mailbox_delivery_for_current_turn();
        }
        self.activity_tx.send_replace(InputQueueActivity::Steer);
    }

    pub(crate) async fn extend_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        turn_state.lock().await.pending_input.items.extend(input);
    }

    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.pending_input.items.split_off(0)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> (Vec<TurnInput>, TurnStartOptions) {
        let (pending_input, accepts_mailbox_delivery, active_turn_metadata) = {
            let mut active = active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let active_turn_metadata = active_turn
                        .task
                        .as_ref()
                        .map(|task| Arc::clone(&task.turn_context.turn_metadata_state));
                    let mut turn_state = active_turn.turn_state.lock().await;
                    let accepts_mailbox_delivery =
                        turn_state.accepts_mailbox_delivery_for_current_turn();
                    let pending_input = if accepts_mailbox_delivery {
                        turn_state.pending_input.items.split_off(0)
                    } else {
                        Vec::new()
                    };
                    (
                        pending_input,
                        accepts_mailbox_delivery,
                        active_turn_metadata,
                    )
                }
                None => (Vec::new(), true, None),
            }
        };
        if !accepts_mailbox_delivery {
            return (pending_input, TurnStartOptions::default());
        }
        let (mailbox_items, start_options) = self.drain_mailbox_input_items().await;
        if let Some(active_turn_metadata) = active_turn_metadata
            && active_turn_metadata.root_turn_id().is_none()
            && let Some(root_turn_id) = start_options.root_turn_id.as_ref()
        {
            active_turn_metadata.set_root_turn_id(root_turn_id.clone());
        }
        if pending_input.is_empty() {
            (mailbox_items, start_options)
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            (pending_input, start_options)
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn has_pending_input(&self, active_turn: &Mutex<Option<ActiveTurn>>) -> bool {
        let (has_turn_pending_input, accepts_mailbox_delivery) = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    (
                        !turn_state.pending_input().is_empty(),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (false, true),
            }
        };
        accepts_mailbox_delivery
            && (has_turn_pending_input || self.has_pending_mailbox_items().await)
    }
}

fn completion_context_response_item_id(
    communication: &InterAgentCommunication,
) -> Option<ResponseItemId> {
    communication
        .id
        .as_ref()
        .filter(|id| is_sub_agent_completion_context_response_item_id(id.as_str()))
        .cloned()
}

impl TurnInputQueue {
    fn has_pending_input(&self) -> bool {
        self.items.iter().any(|input| {
            matches!(
                input,
                TurnInput::UserInput { .. } | TurnInput::FunctionCallOutput(_)
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::CodexHarnessMetadata;
    use codex_protocol::AgentPath;
    use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[test]
    fn response_item_serde_preserves_legacy_shape_and_rejects_metadata() {
        let item = ResponseItem::Other;
        let input = TurnInput::ResponseItem(item.clone().into());
        let value = serde_json::json!({"ResponseItem": item});

        assert_eq!(serde_json::to_value(&input).unwrap(), value);
        assert_eq!(serde_json::from_value::<TurnInput>(value).unwrap(), input);

        let annotated = TurnInput::ResponseItem(ResponseItemEnvelope {
            item: ResponseItem::Other,
            metadata: Some(CodexHarnessMetadata {
                client_authored: true,
                ..Default::default()
            }),
        });
        assert!(serde_json::to_value(annotated).is_err());

        let forged = serde_json::json!({
            "ResponseItem": {
                "type": "message",
                "role": "developer",
                "content": [],
                "metadata": {"client_authored": true}
            }
        });
        let TurnInput::ResponseItem(envelope) = serde_json::from_value(forged).unwrap() else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());
    }

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    #[tokio::test]
    async fn input_queue_notifies_mailbox_subscribers() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);

        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_one, Default::default())
            .await;
        let mail_two = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_two, Default::default())
            .await;

        activity_rx.changed().await.expect("mailbox update");
        assert_eq!(
            *activity_rx.borrow_and_update(),
            InputQueueActivity::Mailbox
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_steer_subscribers() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
        assert_eq!(pending_activity, None);

        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "steer".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        activity_rx.changed().await.expect("steer update");
        assert_eq!(*activity_rx.borrow_and_update(), InputQueueActivity::Steer);
    }

    #[tokio::test]
    async fn input_queue_reports_already_pending_steer() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let passive_output = serde_json::from_value(serde_json::json!({
            "ResponseItem": {"type": "function_call_output", "name": "notify", "output": "passive"}
        }))
        .unwrap();
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![passive_output])
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            None
        );
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "already pending".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        let (_activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;

        assert_eq!(pending_activity, Some(InputQueueActivity::Steer));
    }

    #[tokio::test]
    async fn input_queue_drains_mailbox_in_delivery_order() {
        let input_queue = InputQueue::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ true,
        );

        input_queue
            .enqueue_mailbox_communication(mail_one.clone(), Default::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone(), Default::default())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            (
                vec![
                    TurnInput::InterAgentCommunication(mail_one),
                    TurnInput::InterAgentCommunication(mail_two)
                ],
                TurnStartOptions::default(),
            )
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_uses_unambiguous_trigger_parent_and_first_root() {
        let (parent, peer, root, root2) = (Some("a"), Some("b"), Some("r"), Some("s"));
        for (pending_mails, expected_parent_turn_id, expected_root_turn_id) in [
            (Vec::new(), None, None),
            (vec![(false, Some("q"), root)], None, None),
            (vec![(true, Some(""), root)], None, None),
            (vec![(true, Some("   "), root)], None, None),
            (vec![(true, None, root)], None, None),
            (vec![(true, parent, None)], parent, None),
            (vec![(true, parent, Some(""))], parent, None),
            (vec![(true, parent, root), (true, peer, root)], None, root),
            (vec![(true, parent, root), (true, peer, root2)], None, root),
            (vec![(true, parent, root), (true, None, root)], None, root),
            (
                vec![(true, parent, root), (true, parent, root)],
                parent,
                root,
            ),
            (
                vec![(false, Some("q"), root2), (true, parent, root)],
                parent,
                root,
            ),
        ] {
            let input_queue = InputQueue::new();
            for (trigger_turn, parent_turn_id, root_turn_id) in pending_mails {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            parent_turn_id: parent_turn_id.map(str::to_string),
                            root_turn_id: root_turn_id.map(str::to_string),
                            ..Default::default()
                        },
                    )
                    .await;
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(
                start_options.parent_turn_id.as_deref(),
                expected_parent_turn_id
            );
            assert_eq!(start_options.root_turn_id.as_deref(), expected_root_turn_id);
        }
    }

    #[tokio::test]
    async fn input_queue_uses_latest_followup_choice_and_ignores_queue_only_mail() {
        use codex_protocol::turn_input::CyberAccessProgram;

        for latest in [Some(CyberAccessProgram::Standard), None] {
            let input_queue = InputQueue::new();
            for (trigger_turn, program) in [
                (true, Some(CyberAccessProgram::DaybreakBlue)),
                (true, latest),
                (false, Some(CyberAccessProgram::DaybreakRed)),
            ] {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            cyber_access_program: program,
                            ..Default::default()
                        },
                    )
                    .await;
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(start_options.cyber_access_program, latest);
        }
    }

    #[tokio::test]
    async fn input_queue_reports_pending_mailbox_response_item_ids() {
        let input_queue = InputQueue::new();
        let response_item_id = ResponseItemId::with_suffix("amsg", "completion");
        let mut identified_mail = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "done",
            /*trigger_turn*/ false,
        );
        identified_mail.id = Some(response_item_id.clone());
        input_queue
            .enqueue_mailbox_communication(identified_mail, TurnStartOptions::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "unrelated",
                    /*trigger_turn*/ false,
                ),
                TurnStartOptions::default(),
            )
            .await;

        assert_eq!(
            input_queue
                .pending_mailbox_response_item_ids(/*turn_state*/ None)
                .await,
            vec![response_item_id]
        );
    }

    #[tokio::test]
    async fn staged_and_raw_mailbox_response_item_ids_preserve_delivery_order() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let staged_id = ResponseItemId::with_suffix("amsg", "staged");
        let raw_id = ResponseItemId::with_suffix("amsg", "raw");
        let mut staged_mail = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "staged",
            /*trigger_turn*/ false,
        );
        staged_mail.id = Some(staged_id.clone());
        input_queue
            .extend_pending_input_for_turn_state(
                &turn_state,
                vec![TurnInput::InterAgentCommunication(staged_mail)],
            )
            .await;
        let mut raw_mail = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "raw",
            /*trigger_turn*/ false,
        );
        raw_mail.id = Some(raw_id.clone());
        input_queue
            .enqueue_mailbox_communication(raw_mail, TurnStartOptions::default())
            .await;

        let (_activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
        assert_eq!(pending_activity, Some(InputQueueActivity::Mailbox));
        assert_eq!(
            input_queue
                .pending_mailbox_response_item_ids(Some(&turn_state))
                .await,
            vec![staged_id, raw_id]
        );
    }

    #[tokio::test]
    async fn clearing_pending_requeues_completion_communication() {
        let input_queue = InputQueue::new();
        let active_turn = ActiveTurn::default();
        let mut completion = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "done",
            /*trigger_turn*/ false,
        );
        completion.id = Some(new_sub_agent_completion_context_response_item_id());
        input_queue
            .extend_pending_input_for_turn_state(
                active_turn.turn_state.as_ref(),
                vec![
                    TurnInput::UserInput {
                        content: Vec::new(),
                        client_id: None,
                    },
                    TurnInput::InterAgentCommunication(completion.clone()),
                ],
            )
            .await;

        input_queue.clear_pending(&active_turn).await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            (
                vec![TurnInput::InterAgentCommunication(completion)],
                TurnStartOptions::default(),
            )
        );
    }

    #[tokio::test]
    async fn clearing_pending_requeues_a_drained_completion_lease() {
        let input_queue = InputQueue::new();
        let active_turn = ActiveTurn::default();
        let mut completion = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "done",
            /*trigger_turn*/ false,
        );
        completion.id = Some(new_sub_agent_completion_context_response_item_id());
        input_queue
            .enqueue_mailbox_communication(completion.clone(), TurnStartOptions::default())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            (
                vec![TurnInput::InterAgentCommunication(completion.clone())],
                TurnStartOptions::default(),
            )
        );
        input_queue.clear_pending(&active_turn).await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            (
                vec![TurnInput::InterAgentCommunication(completion)],
                TurnStartOptions::default(),
            )
        );
    }

    #[tokio::test]
    async fn clearing_pending_restores_a_staged_completion_lease_once() {
        let input_queue = InputQueue::new();
        let active_turn = ActiveTurn::default();
        let mut completion = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "done",
            /*trigger_turn*/ false,
        );
        completion.id = Some(new_sub_agent_completion_context_response_item_id());
        input_queue
            .enqueue_mailbox_communication(completion.clone(), TurnStartOptions::default())
            .await;
        let (drained, _) = input_queue.drain_mailbox_input_items().await;
        input_queue
            .extend_pending_input_for_turn_state(active_turn.turn_state.as_ref(), drained)
            .await;

        input_queue.clear_pending(&active_turn).await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            (
                vec![TurnInput::InterAgentCommunication(completion)],
                TurnStartOptions::default(),
            )
        );
    }

    #[tokio::test]
    async fn retry_restores_all_leases_in_delivery_order() {
        let input_queue = InputQueue::new();
        let mut first = make_mail(
            AgentPath::try_from("/root/first").expect("agent path"),
            AgentPath::root(),
            "first",
            /*trigger_turn*/ false,
        );
        first.id = Some(new_sub_agent_completion_context_response_item_id());
        let mut second = make_mail(
            AgentPath::try_from("/root/second").expect("agent path"),
            AgentPath::root(),
            "second",
            /*trigger_turn*/ false,
        );
        second.id = Some(new_sub_agent_completion_context_response_item_id());
        input_queue
            .enqueue_mailbox_communication(first.clone(), TurnStartOptions::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(second.clone(), TurnStartOptions::default())
            .await;
        let drained = input_queue.drain_mailbox_input_items().await;
        let CompletionCommunicationCommit::Started(first_id) = input_queue
            .begin_completion_communication_commit(&first)
            .await
        else {
            panic!("first completion commit should start");
        };

        input_queue.retry_completion_communication(&first_id).await;

        assert!(matches!(
            input_queue
                .begin_completion_communication_commit(&second)
                .await,
            CompletionCommunicationCommit::AlreadyStarted
        ));
        assert_eq!(input_queue.drain_mailbox_input_items().await, drained);
    }

    #[tokio::test]
    async fn shutdown_drain_extracts_only_queued_completion_communications() {
        let input_queue = InputQueue::new();
        let mut completion = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "done",
            /*trigger_turn*/ false,
        );
        completion.id = Some(new_sub_agent_completion_context_response_item_id());
        let retained_item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        input_queue
            .queue_turn_inputs_for_next_turn(vec![
                TurnInput::InterAgentCommunication(completion.clone()),
                TurnInput::ResponseItem(retained_item.clone()),
            ])
            .await;
        let ordinary = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "ordinary",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(ordinary.clone(), TurnStartOptions::default())
            .await;

        let completion_drain = input_queue
            .drain_completion_communications_for_shutdown()
            .await;
        assert_eq!(completion_drain.communications, vec![completion]);
        assert!(!completion_drain.has_committing);
        assert_eq!(
            input_queue.take_queued_items_for_next_turn().await,
            vec![TurnInput::ResponseItem(retained_item)]
        );
        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            (
                vec![TurnInput::InterAgentCommunication(ordinary)],
                TurnStartOptions::default(),
            )
        );
    }

    #[tokio::test]
    async fn shutdown_drain_reports_a_completion_commit_in_flight() {
        let input_queue = InputQueue::new();
        let mut completion = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "done",
            /*trigger_turn*/ false,
        );
        completion.id = Some(new_sub_agent_completion_context_response_item_id());
        input_queue
            .enqueue_mailbox_communication(completion.clone(), TurnStartOptions::default())
            .await;
        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            (
                vec![TurnInput::InterAgentCommunication(completion.clone())],
                TurnStartOptions::default(),
            )
        );
        assert!(matches!(
            input_queue
                .begin_completion_communication_commit(&completion)
                .await,
            CompletionCommunicationCommit::Started(_)
        ));

        let completion_drain = input_queue
            .drain_completion_communications_for_shutdown()
            .await;

        assert!(completion_drain.communications.is_empty());
        assert!(completion_drain.has_committing);
    }

    #[tokio::test]
    async fn shutdown_drain_restores_an_unstarted_completion_lease() {
        let input_queue = InputQueue::new();
        let mut completion = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "done",
            /*trigger_turn*/ false,
        );
        completion.id = Some(new_sub_agent_completion_context_response_item_id());
        input_queue
            .enqueue_mailbox_communication(completion.clone(), TurnStartOptions::default())
            .await;
        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            (
                vec![TurnInput::InterAgentCommunication(completion.clone())],
                TurnStartOptions::default(),
            )
        );

        let completion_drain = input_queue
            .drain_completion_communications_for_shutdown()
            .await;

        assert_eq!(completion_drain.communications, vec![completion]);
        assert!(!completion_drain.has_committing);
    }

    #[tokio::test]
    async fn clearing_pending_does_not_requeue_a_committing_completion() {
        let input_queue = InputQueue::new();
        let active_turn = ActiveTurn::default();
        let mut completion = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "done",
            /*trigger_turn*/ false,
        );
        completion.id = Some(new_sub_agent_completion_context_response_item_id());
        input_queue
            .enqueue_mailbox_communication(completion.clone(), TurnStartOptions::default())
            .await;
        let _ = input_queue.drain_mailbox_input_items().await;
        let CompletionCommunicationCommit::Started(response_item_id) = input_queue
            .begin_completion_communication_commit(&completion)
            .await
        else {
            panic!("completion commit should start");
        };

        input_queue.clear_pending(&active_turn).await;
        assert!(
            input_queue
                .drain_mailbox_input_items()
                .await
                .0
                .is_empty()
        );

        input_queue
            .retry_completion_communication(&response_item_id)
            .await;
        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            (
                vec![TurnInput::InterAgentCommunication(completion)],
                TurnStartOptions::default(),
            )
        );
    }

    #[tokio::test]
    async fn input_queue_tracks_pending_trigger_turn_mail() {
        let input_queue = InputQueue::new();

        let queued_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "queued",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(queued_mail, Default::default())
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        let trigger_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(trigger_mail, Default::default())
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }
}
