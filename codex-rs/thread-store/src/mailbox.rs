//! Typed mailbox persistence, separate from runtime grants and input admission.
//!
//! Claims reserve a fixed batch; they do not prove delivery or consume messages.
//! SQLite claims and canonical receiver history have separate commit boundaries.

use codex_protocol::AgentInputAttribution;
use codex_protocol::ThreadId;
use codex_protocol::user_input::UserInput;
use codex_rollout::ResponseItemEnvelope;
use serde::Deserialize;
use serde::Serialize;

use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub use codex_state::MailboxInvocation;
pub use codex_state::MailboxMessageState;

/// Canonical author identity, never a display name or a runtime grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailboxSender {
    User,
    Agent(ThreadId),
}

impl MailboxSender {
    pub(crate) fn key(&self) -> String {
        match self {
            Self::User => "user".to_string(),
            Self::Agent(thread_id) => format!("agent:{thread_id}"),
        }
    }
}

/// Structured input with distinct user and trusted agent authorship.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MailboxPayload {
    User {
        input: Vec<UserInput>,
        client_id: Option<String>,
    },
    Agent {
        input: Vec<UserInput>,
        attribution: Box<AgentInputAttribution>,
    },
}

impl MailboxPayload {
    pub(crate) fn sender(&self) -> MailboxSender {
        match self {
            Self::User { .. } => MailboxSender::User,
            Self::Agent { attribution, .. } => MailboxSender::Agent(attribution.sender.thread_id),
        }
    }

    pub(crate) fn validate_receiver(&self, receiver: ThreadId) -> ThreadStoreResult<()> {
        if let Self::Agent { attribution, .. } = self
            && attribution.recipient.thread_id != receiver
        {
            return Err(invalid_request(
                "mailbox attribution recipient does not match receiver",
            ));
        }
        Ok(())
    }
}

/// Idempotent submission; callers preserve the key on acceptance retries.
///
/// Trusted attribution must be constructed by the caller's authority boundary.
/// The store checks identity consistency, not permission to send.
#[derive(Clone, Debug, PartialEq)]
pub struct AcceptMailboxInputParams {
    pub receiver_thread_id: ThreadId,
    pub submission_key: String,
    pub payload: MailboxPayload,
}

/// Sender filters are canonicalized as sets, including an explicitly empty set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailboxSelection {
    All,
    Senders(Vec<MailboxSender>),
}

impl MailboxSelection {
    pub(crate) fn to_state(&self) -> codex_state::MailboxSelection {
        match self {
            Self::All => codex_state::MailboxSelection::All,
            Self::Senders(senders) => {
                let mut keys: Vec<_> = senders.iter().map(MailboxSender::key).collect();
                keys.sort();
                keys.dedup();
                codex_state::MailboxSelection::Senders(keys)
            }
        }
    }
}

/// Retrying this invocation must use the same canonical sender selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimMailboxInputParams {
    pub invocation: MailboxInvocation,
    pub selection: MailboxSelection,
}

/// Accepted content and its current state; acceptance is not model visibility.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredMailboxInput {
    pub id: String,
    pub receiver_thread_id: ThreadId,
    pub submission_key: String,
    pub sender: MailboxSender,
    pub payload: MailboxPayload,
    pub acceptance_sequence: i64,
    pub state: MailboxMessageState,
    pub rejection_reason: Option<String>,
}

/// A fixed batch member with a stable identity reserved for receiver history.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaimedMailboxInput {
    pub message: StoredMailboxInput,
    pub delivery_id: String,
}

/// Fixed membership with current states, including terminal members on retries.
///
/// Callers must not inject consumed or rejected members. Selection alone never
/// acknowledges consumption, and this API deliberately exposes no unchecked ack.
#[derive(Clone, Debug, PartialEq)]
pub struct MailboxClaim {
    pub invocation: MailboxInvocation,
    pub selection: MailboxSelection,
    pub messages: Vec<ClaimedMailboxInput>,
}

/// Expected prepared context, never proof of delivery without canonical history.
///
/// Core may transform attachments while preparing this envelope. The original
/// structured payload is verified separately against its presentation artifact.
#[derive(Clone, Debug, PartialEq)]
pub struct MailboxDeliveryEvidence {
    pub message_id: String,
    pub prepared_context: ResponseItemEnvelope,
}

/// Reconciles a fixed claim after Core's serialized canonical delivery boundary.
///
/// Core must serialize same-invocation retries, live insertion, and rejection
/// arbitration. SQL acknowledgement is not atomic with history append.
#[derive(Clone, Debug, PartialEq)]
pub struct ReconcileMailboxDeliveryParams {
    pub claim: ClaimMailboxInputParams,
    pub deliveries: Vec<MailboxDeliveryEvidence>,
}

/// Exact canonical artifacts, not authorization to repair or resend partial delivery.
// Keep the paired artifacts in the agreed storage-neutral consumer shape, like
// protocol TurnItem, rather than introducing variant-specific indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum MailboxDeliveryArtifacts {
    /// No artifacts recovered. Terminal members are deliberately not inspected;
    /// consult their authoritative state rather than interpreting this as absence.
    NotRecorded,
    ContextOnly {
        prepared_context: ResponseItemEnvelope,
    },
    PresentationOnly {
        completion: codex_protocol::protocol::ItemCompletedEvent,
    },
    Complete {
        prepared_context: ResponseItemEnvelope,
        completion: codex_protocol::protocol::ItemCompletedEvent,
    },
}

#[derive(Clone, Debug)]
pub struct RecoveredMailboxDelivery {
    pub message_id: String,
    pub artifacts: MailboxDeliveryArtifacts,
}

/// One artifact classification per fixed member, in claim order.
///
/// Terminal states in `claim` are authoritative even if artifacts are no longer
/// reachable. Recovery may establish a new fixed claim, including an empty one.
#[derive(Clone, Debug)]
pub struct RecoveredMailboxClaim {
    pub claim: MailboxClaim,
    pub deliveries: Vec<RecoveredMailboxDelivery>,
}

/// Called only under Core's permission/delivery arbitration after recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectMailboxInputParams {
    pub receiver_thread_id: ThreadId,
    pub message_id: String,
    pub reason: String,
}

pub(crate) fn invalid_request(message: &str) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: message.to_string(),
    }
}

pub(crate) fn storage_error(error: &(dyn std::error::Error + 'static)) -> ThreadStoreError {
    if let Some(error) = error.downcast_ref::<std::io::Error>()
        && error.kind() == std::io::ErrorKind::InvalidInput
    {
        return invalid_request(&error.to_string());
    }
    ThreadStoreError::Internal {
        message: format!("mailbox storage failed: {error}"),
    }
}

pub(crate) fn encode_payload(params: &AcceptMailboxInputParams) -> ThreadStoreResult<String> {
    if params.submission_key.is_empty() {
        return Err(invalid_request(
            "mailbox submission and sender keys must be nonempty",
        ));
    }
    params
        .payload
        .validate_receiver(params.receiver_thread_id)?;
    serde_json::to_string(&params.payload).map_err(|error| storage_error(&error))
}

pub(crate) fn sender_from_key(key: &str) -> ThreadStoreResult<MailboxSender> {
    if key == "user" {
        return Ok(MailboxSender::User);
    }
    let sender = key
        .strip_prefix("agent:")
        .and_then(|id| ThreadId::from_string(id).ok())
        .map(MailboxSender::Agent);
    sender.ok_or_else(|| ThreadStoreError::Internal {
        message: "invalid canonical mailbox sender key".to_string(),
    })
}

pub(crate) fn decode_message(
    message: codex_state::MailboxMessage,
) -> ThreadStoreResult<StoredMailboxInput> {
    let payload: MailboxPayload =
        serde_json::from_str(&message.payload_json).map_err(|error| storage_error(&error))?;
    let sender = sender_from_key(&message.sender_key)?;
    if sender != payload.sender()
        || payload
            .validate_receiver(message.receiver_thread_id)
            .is_err()
    {
        return Err(ThreadStoreError::Internal {
            message: "stored mailbox authorship does not match envelope".to_string(),
        });
    }
    Ok(StoredMailboxInput {
        id: message.id,
        receiver_thread_id: message.receiver_thread_id,
        submission_key: message.submission_key,
        sender,
        payload,
        acceptance_sequence: message.acceptance_sequence,
        state: message.state,
        rejection_reason: message.rejection_reason,
    })
}

pub(crate) fn decode_claim(claim: codex_state::MailboxClaim) -> ThreadStoreResult<MailboxClaim> {
    let selection = match claim.selection {
        codex_state::MailboxSelection::All => MailboxSelection::All,
        codex_state::MailboxSelection::Senders(keys) => MailboxSelection::Senders(
            keys.iter()
                .map(|key| sender_from_key(key))
                .collect::<ThreadStoreResult<_>>()?,
        ),
    };
    let messages = claim
        .messages
        .into_iter()
        .map(|member| {
            Ok(ClaimedMailboxInput {
                message: decode_message(member.message)?,
                delivery_id: member.delivery_id,
            })
        })
        .collect::<ThreadStoreResult<_>>()?;
    Ok(MailboxClaim {
        invocation: claim.invocation,
        selection,
        messages,
    })
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
