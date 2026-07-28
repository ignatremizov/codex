use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_protocol::user_input::UserInput;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
    },
    ResponseItem(ResponseItem),
    InterAgentCommunication(InterAgentCommunication),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputQueueActivity {
    Mailbox,
    Steer,
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

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    mailbox: Mutex<MailboxState>,
    idle_pending_input: Mutex<Vec<TurnInput>>,
}

#[derive(Clone)]
struct PendingMailboxCommunication {
    communication: InterAgentCommunication,
    parent_turn_id: Option<String>,
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
                turn_state.pending_input.has_user_input(),
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
        parent_turn_id: Option<String>,
    ) {
        self.mailbox
            .lock()
            .await
            .pending_mails
            .push_back(PendingMailboxCommunication {
                communication,
                parent_turn_id,
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
                    TurnInput::UserInput { .. } | TurnInput::ResponseItem(_) => None,
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

    async fn drain_mailbox_input_items_with_parent_turn_id(
        &self,
    ) -> (Vec<TurnInput>, Option<String>) {
        let mut mailbox = self.mailbox.lock().await;
        let pending_mails = mailbox.pending_mails.drain(..).collect::<Vec<_>>();
        let parent_turn_id = pending_mails
            .iter()
            .filter(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.parent_turn_id.as_deref())
            .reduce(|expected, candidate| expected.filter(|id| candidate == Some(*id)))
            .and_then(|id| id.filter(|id| !id.trim().is_empty()).map(str::to_string));
        Self::lease_completion_communications(&mut mailbox, &pending_mails);
        let items = pending_mails
            .into_iter()
            .map(|mail| TurnInput::InterAgentCommunication(mail.communication))
            .collect();
        (items, parent_turn_id)
    }

    pub(crate) async fn drain_mailbox_input_items(&self) -> Vec<TurnInput> {
        self.drain_mailbox_input_items_with_parent_turn_id().await.0
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
                            parent_turn_id: None,
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
                        parent_turn_id: None,
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
                    parent_turn_id: None,
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
            .map(TurnInput::ResponseItem)
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

    pub(crate) async fn queued_response_items_for_next_turn(&self) -> Vec<ResponseItem> {
        self.idle_pending_input
            .lock()
            .await
            .iter()
            .filter_map(|item| match item {
                TurnInput::ResponseItem(item) => Some(item.clone()),
                TurnInput::UserInput { .. } | TurnInput::InterAgentCommunication(_) => None,
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) async fn has_queued_response_items_for_next_turn(&self) -> bool {
        !self.idle_pending_input.lock().await.is_empty()
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
    ) -> (Vec<TurnInput>, Option<String>) {
        let (pending_input, accepts_mailbox_delivery) = {
            let mut active = active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let mut turn_state = active_turn.turn_state.lock().await;
                    let accepts_mailbox_delivery =
                        turn_state.accepts_mailbox_delivery_for_current_turn();
                    let pending_input = if accepts_mailbox_delivery {
                        turn_state.pending_input.items.split_off(0)
                    } else {
                        Vec::new()
                    };
                    (pending_input, accepts_mailbox_delivery)
                }
                None => (Vec::new(), true),
            }
        };
        if !accepts_mailbox_delivery {
            return (pending_input, None);
        }
        let (mailbox_items, parent_turn_id) =
            self.drain_mailbox_input_items_with_parent_turn_id().await;
        if pending_input.is_empty() {
            (mailbox_items, parent_turn_id)
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            (pending_input, parent_turn_id)
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
    fn has_user_input(&self) -> bool {
        self.items
            .iter()
            .any(|input| matches!(input, TurnInput::UserInput { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::AgentPath;
    use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
    use pretty_assertions::assert_eq;

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
            .enqueue_mailbox_communication(mail_one, /*parent_turn_id*/ None)
            .await;
        let mail_two = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_two, /*parent_turn_id*/ None)
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
            .enqueue_mailbox_communication(mail_one.clone(), /*parent_turn_id*/ None)
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone(), /*parent_turn_id*/ None)
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two)
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_requires_one_unambiguous_trigger_parent() {
        for (pending_mails, expected_parent_turn_id) in [
            (Vec::new(), None),
            (vec![(false, Some("q"))], None),
            (vec![(true, Some(""))], None),
            (vec![(true, Some("   "))], None),
            (vec![(true, None)], None),
            (vec![(true, Some("a")), (true, Some("b"))], None),
            (vec![(true, Some("a")), (true, None)], None),
            (vec![(true, Some("a")), (true, Some("a"))], Some("a")),
            (vec![(false, Some("q")), (true, Some("a"))], Some("a")),
        ] {
            let input_queue = InputQueue::new();
            for (trigger_turn, parent_turn_id) in pending_mails {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        parent_turn_id.map(str::to_string),
                    )
                    .await;
            }
            let (_, parent_turn_id) = input_queue
                .drain_mailbox_input_items_with_parent_turn_id()
                .await;
            assert_eq!(parent_turn_id.as_deref(), expected_parent_turn_id);
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
            .enqueue_mailbox_communication(identified_mail, /*parent_turn_id*/ None)
            .await;
        input_queue
            .enqueue_mailbox_communication(
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    "unrelated",
                    /*trigger_turn*/ false,
                ),
                /*parent_turn_id*/ None,
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
            .enqueue_mailbox_communication(raw_mail, /*parent_turn_id*/ None)
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
            vec![TurnInput::InterAgentCommunication(completion)]
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
            .enqueue_mailbox_communication(completion.clone(), /*parent_turn_id*/ None)
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![TurnInput::InterAgentCommunication(completion.clone())]
        );
        input_queue.clear_pending(&active_turn).await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![TurnInput::InterAgentCommunication(completion)]
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
            .enqueue_mailbox_communication(completion.clone(), /*parent_turn_id*/ None)
            .await;
        let drained = input_queue.drain_mailbox_input_items().await;
        input_queue
            .extend_pending_input_for_turn_state(active_turn.turn_state.as_ref(), drained)
            .await;

        input_queue.clear_pending(&active_turn).await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![TurnInput::InterAgentCommunication(completion)]
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
            .enqueue_mailbox_communication(first.clone(), /*parent_turn_id*/ None)
            .await;
        input_queue
            .enqueue_mailbox_communication(second.clone(), /*parent_turn_id*/ None)
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
            .enqueue_mailbox_communication(ordinary.clone(), /*parent_turn_id*/ None)
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
            vec![TurnInput::InterAgentCommunication(ordinary)]
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
            .enqueue_mailbox_communication(completion.clone(), /*parent_turn_id*/ None)
            .await;
        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![TurnInput::InterAgentCommunication(completion.clone())]
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
            .enqueue_mailbox_communication(completion.clone(), /*parent_turn_id*/ None)
            .await;
        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![TurnInput::InterAgentCommunication(completion.clone())]
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
            .enqueue_mailbox_communication(completion.clone(), /*parent_turn_id*/ None)
            .await;
        let _ = input_queue.drain_mailbox_input_items().await;
        let CompletionCommunicationCommit::Started(response_item_id) = input_queue
            .begin_completion_communication_commit(&completion)
            .await
        else {
            panic!("completion commit should start");
        };

        input_queue.clear_pending(&active_turn).await;
        assert!(input_queue.drain_mailbox_input_items().await.is_empty());

        input_queue
            .retry_completion_communication(&response_item_id)
            .await;
        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![TurnInput::InterAgentCommunication(completion)]
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
            .enqueue_mailbox_communication(queued_mail, /*parent_turn_id*/ None)
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        let trigger_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(trigger_mail, /*parent_turn_id*/ None)
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }
}
