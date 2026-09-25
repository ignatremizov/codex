use super::send_input_admission::RecipientInput;
use super::send_input_admission::admit_input;
use super::send_input_admission::prepare_receiver;
use super::send_input_admission::resolve_receiver;
use super::*;
use crate::agent::control::render_input_preview;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::tools::handlers::multi_agents_spec::create_send_input_tool_v1;
use codex_protocol::WakeEventFinalDelivery;
use codex_protocol::WakeEventFlags;
use codex_protocol::WakeEventMailboxSubscription;
use codex_protocol::WakeEventSurface;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_tools::ToolSpec;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "send_input")
    }

    fn spec(&self) -> ToolSpec {
        create_send_input_tool_v1()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "send_input send message existing agent subagent follow up interrupt redirect queue mailbox target",
            self.spec(),
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let arguments = function_arguments(invocation.payload.clone())?;
        let args: SendInputArgs = parse_arguments(&arguments)?;
        let input_items = parse_collab_input(args.message, args.items)?;
        let mailbox = matches!(args.w, SendInputMode::Mailbox(_));
        if mailbox && args.interrupt {
            return Err(FunctionCallError::RespondToModel(
                "mailbox input cannot interrupt a receiver; omit interrupt when using w:z"
                    .to_string(),
            ));
        }
        if mailbox && invocation.turn.multi_agent_version != MultiAgentVersion::V1 {
            return Err(FunctionCallError::RespondToModel(
                "mailbox input is only supported by V1 send_input".to_string(),
            ));
        }
        let target = match args.target {
            SendInputTarget::Single(target) => target,
            SendInputTarget::Batch(targets) => {
                if targets.is_empty() {
                    return Err(FunctionCallError::RespondToModel(
                        "target array must not be empty".to_string(),
                    ));
                }
                return super::send_input_batch::handle_batch(
                    invocation,
                    targets,
                    input_items,
                    args.w,
                    args.interrupt,
                )
                .await;
            }
        };
        let ToolInvocation {
            session,
            turn,
            call_id,
            ..
        } = invocation;
        let response_observation = args.w.response_observation();
        let receiver_thread_id = resolve_receiver(&session, &target, args.w).await?;
        let prompt = render_input_preview(&input_items);
        let prepared =
            prepare_receiver(&session, &turn, receiver_thread_id, args.w, args.interrupt).await?;
        let receiver_agent = prepared.agent.clone();
        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::SendInput,
                    status: CollabAgentToolCallStatus::InProgress,
                    observe_commentary: Some(response_observation.commentary()),
                    wake_on_completion: response_observation.wake_on_completion_item_value(),
                    target_messages: Some(response_observation.target_messages()),
                    queue_input: Some(response_observation.queue_input()),
                    input_batch: None,
                    mailbox_input: mailbox.then_some(true),
                    deadline_at_ms: None,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: vec![receiver_thread_id],
                    receiver_agents: vec![receiver_agent.clone()],
                    prompt: Some(prompt.clone()),
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                    completion_presentation_agent_ids: None,
                }),
            )
            .await;
        let mut result = admit_input(
            &session,
            &turn,
            RecipientInput {
                receiver: &prepared,
                call_id: &call_id,
                batch_id: None,
                items: input_items,
                mode: args.w,
            },
        )
        .await;
        let status = session
            .services
            .agent_control
            .get_status(receiver_thread_id)
            .await;
        if mailbox
            && matches!(&status, AgentStatus::NotFound)
            && let Ok(result) = &mut result
        {
            result.hint =
                Some("Mail saved; receiver not loaded. Use resume_agent first.".to_string());
        }
        let tool_call_status = if result.is_ok() && mailbox {
            CollabAgentToolCallStatus::Completed
        } else if result.is_ok() {
            collab_tool_call_status(&status, Some(receiver_thread_id))
        } else {
            CollabAgentToolCallStatus::Failed
        };
        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::SendInput,
                    status: tool_call_status,
                    observe_commentary: Some(response_observation.commentary()),
                    wake_on_completion: response_observation.wake_on_completion_item_value(),
                    target_messages: Some(response_observation.target_messages()),
                    queue_input: Some(response_observation.queue_input()),
                    input_batch: None,
                    mailbox_input: mailbox.then_some(true),
                    deadline_at_ms: None,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: vec![receiver_thread_id],
                    receiver_agents: vec![receiver_agent],
                    prompt: Some(prompt),
                    model: None,
                    reasoning_effort: None,
                    agents_states: if mailbox {
                        Default::default()
                    } else {
                        [(receiver_thread_id, status)].into_iter().collect()
                    },
                    completion_presentation_agent_ids: None,
                }),
            )
            .await;
        Ok(boxed_tool_output(result?))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
struct SendInputArgs {
    target: SendInputTarget,
    message: Option<String>,
    items: Option<Vec<UserInput>>,
    #[serde(default)]
    interrupt: bool,
    #[serde(default)]
    w: SendInputMode,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SendInputTarget {
    Single(String),
    Batch(Vec<String>),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum SendInputMode {
    Response(ResponseObservationPolicy),
    Mailbox(WakeEventMailboxSubscription),
}

impl SendInputMode {
    pub(super) fn response_observation(self) -> ResponseObservationPolicy {
        match self {
            Self::Response(policy) => policy,
            Self::Mailbox(_) => ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::None,
            ),
        }
    }

    pub(super) fn normalized_flags(self) -> String {
        match self {
            Self::Mailbox(WakeEventMailboxSubscription::None) => "z".to_string(),
            Self::Mailbox(WakeEventMailboxSubscription::Wake) => "zf".to_string(),
            Self::Response(policy) => {
                let mut flags = String::new();
                if policy.commentary() {
                    flags.push('c');
                }
                match policy.final_response() {
                    FinalResponseObservation::None | FinalResponseObservation::Passive => {}
                    FinalResponseObservation::Wake => flags.push('f'),
                    FinalResponseObservation::PresentationOnly => flags.push('x'),
                }
                if policy.target_messages() {
                    flags.push('m');
                }
                if policy.queue_input() {
                    flags.push('q');
                }
                flags
            }
        }
    }
}

impl Default for SendInputMode {
    fn default() -> Self {
        Self::Response(ResponseObservationPolicy::default())
    }
}

impl<'de> Deserialize<'de> for SendInputMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let flags =
            WakeEventFlags::parse(&value, WakeEventSurface::AgentMailbox).map_err(|error| {
                serde::de::Error::custom(format!("invalid wake/event state `{value}`; {error}"))
            })?;
        if flags.mailbox_input {
            return Ok(Self::Mailbox(flags.mailbox_subscription));
        }
        let final_response = match flags.final_delivery {
            WakeEventFinalDelivery::Passive => FinalResponseObservation::Passive,
            WakeEventFinalDelivery::Wake => FinalResponseObservation::Wake,
            WakeEventFinalDelivery::PresentationOnly => FinalResponseObservation::PresentationOnly,
        };
        Ok(Self::Response(ResponseObservationPolicy::from_turn_parts(
            flags.commentary,
            final_response,
            flags.target_messages,
            flags.queue_input,
        )))
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct SendInputResult {
    pub(super) submission_id: String,
    pub(super) status: SendInputAdmissionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) hint: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum SendInputAdmissionStatus {
    Submitted,
    Queued,
    MailboxAccepted,
}

#[derive(Serialize)]
struct SendInputModelResult<'a> {
    status: &'a SendInputAdmissionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
}

#[cfg(test)]
#[path = "send_input_tests.rs"]
mod tests;

impl ToolOutput for SendInputResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "send_input")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(
            call_id,
            payload,
            &SendInputModelResult {
                status: &self.status,
                hint: self.hint.as_deref(),
            },
            Some(true),
            "send_input",
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(
            &SendInputModelResult {
                status: &self.status,
                hint: self.hint.as_deref(),
            },
            "send_input",
        )
    }
}
