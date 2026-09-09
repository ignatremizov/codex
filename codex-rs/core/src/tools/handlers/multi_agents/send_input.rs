use super::*;
use crate::agent::agent_resolver::resolve_controlled_v1_agent_target;
use crate::agent::control::QueuedInputObservationParams;
use crate::agent::control::render_input_preview;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::tools::handlers::multi_agents_spec::create_send_input_tool_v1;
use codex_protocol::WakeEventFinalDelivery;
use codex_protocol::WakeEventFlags;
use codex_protocol::WakeEventSurface;
use codex_protocol::error::CodexErrorDetails;
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
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: SendInputArgs = parse_arguments(&arguments)?;
        let input_items = parse_collab_input(args.message, args.items)?;
        let mailbox = matches!(args.w, SendInputMode::Mailbox);
        if mailbox && args.interrupt {
            return Err(FunctionCallError::RespondToModel(
                "mailbox input cannot interrupt a receiver; omit interrupt when using w:z"
                    .to_string(),
            ));
        }
        if mailbox && turn.multi_agent_version != MultiAgentVersion::V1 {
            return Err(FunctionCallError::RespondToModel(
                "mailbox input is only supported by V1 send_input".to_string(),
            ));
        }
        let response_observation = match args.w {
            SendInputMode::Response(policy) => policy,
            SendInputMode::Mailbox => ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::None,
            ),
        };
        let receiver_thread_id = if mailbox {
            // Resolve identity only: mailbox acceptance must not load or adopt a runtime.
            match ThreadId::from_string(args.target.strip_prefix("id:").unwrap_or(&args.target)) {
                Ok(receiver) => receiver,
                Err(_) => resolve_controlled_v1_agent_target(&session, &args.target).await?,
            }
        } else {
            resolve_controlled_v1_agent_target(&session, &args.target).await?
        };
        if receiver_thread_id == session.thread_id {
            return Err(FunctionCallError::RespondToModel(
                "an agent cannot send input to itself; continue the current turn directly"
                    .to_string(),
            ));
        }
        let prompt = render_input_preview(&input_items);
        let receiver_agent = session
            .services
            .agent_control
            .get_agent_metadata(receiver_thread_id);
        if !mailbox
            && receiver_agent.is_some()
            && session.multi_agent_version() == Some(MultiAgentVersion::V2)
        {
            let resume_config = build_agent_resume_config(turn.as_ref())?;
            session
                .services
                .agent_control
                .ensure_v2_agent_loaded(resume_config, receiver_thread_id)
                .await
                .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
        }
        let receiver_agent = receiver_agent.unwrap_or_default();
        let agent_control = session.services.agent_control.clone();
        let sends_to_descendant = !mailbox
            && agent_control
                .is_live_agent_descendant(session.thread_id, receiver_thread_id)
                .await
                .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
        if args.interrupt && !sends_to_descendant {
            return Err(FunctionCallError::RespondToModel(
                "an agent reply route authorizes input, not interruption of its parent or peer"
                    .to_string(),
            ));
        }
        if args.interrupt {
            agent_control
                .interrupt_agent(receiver_thread_id)
                .await
                .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
        }
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
                    deadline_at_ms: None,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: vec![receiver_thread_id],
                    receiver_agents: Vec::new(),
                    prompt: Some(prompt.clone()),
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                    completion_presentation_agent_ids: None,
                }),
            )
            .await;
        let start_options = crate::TurnStartOptions {
            parent_turn_id: Some(turn.sub_id.clone()),
            root_turn_id: turn.turn_metadata_state.root_turn_id(),
            cyber_access_program: turn.cyber_access_program,
            ..Default::default()
        };
        let result = async {
            if mailbox {
                // Acceptance may hint a loaded receiver's inventory scheduler; this tool
                // does not admit a payload-bearing turn or deliver the payload to its model.
                agent_control
                    .accept_mailbox_agent_input(
                        session.presentation_id(),
                        &turn.sub_id,
                        &call_id,
                        receiver_thread_id,
                        input_items,
                    )
                    .await
                    .map(|accepted| SendInputResult {
                        submission_id: accepted.id,
                    })
            } else if sends_to_descendant {
                let input = agent_control
                    .attribute_model_input(
                        session.presentation_id(),
                        receiver_thread_id,
                        &turn.sub_id,
                        input_items,
                    )
                    .await?;
                if response_observation.queue_input() {
                    agent_control
                        .queue_input_observing_response(QueuedInputObservationParams {
                            agent_id: receiver_thread_id,
                            input,
                            start_options,
                            observer: session.presentation_id(),
                            response_observation,
                            task_preview: None,
                            authored_selector: None,
                        })
                        .await
                        .map(|submission| SendInputResult {
                            submission_id: submission.queue_id.to_string(),
                        })
                } else {
                    agent_control
                        .send_agent_input_observing_response(
                            receiver_thread_id,
                            input,
                            start_options,
                            session.presentation_id(),
                            response_observation,
                        )
                        .await
                        .map(|submission_id| SendInputResult { submission_id })
                }
            } else if response_observation.queue_input() {
                agent_control
                    .queue_scoped_agent_input_observing_response(
                        session.presentation_id(),
                        &turn.sub_id,
                        receiver_thread_id,
                        input_items,
                        start_options,
                        response_observation,
                    )
                    .await
                    .map(|submission| SendInputResult {
                        submission_id: submission.queue_id.to_string(),
                    })
            } else {
                agent_control
                    .send_scoped_agent_input_observing_response(
                        session.presentation_id(),
                        &turn.sub_id,
                        receiver_thread_id,
                        input_items,
                        start_options,
                        response_observation,
                    )
                    .await
                    .map(|submission_id| SendInputResult { submission_id })
            }
        }
        .await
        .map_err(|err| {
            // Mailbox admission reports actionable policy denials, not a missing manager.
            if mailbox && let CodexErrorDetails::UnsupportedOperation(message) = err.details() {
                FunctionCallError::RespondToModel(message.clone())
            } else {
                collab_agent_error(receiver_thread_id, err)
            }
        });
        let status = session
            .services
            .agent_control
            .get_status(receiver_thread_id)
            .await;
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
                    deadline_at_ms: None,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: vec![receiver_thread_id],
                    receiver_agents: vec![CollabAgentRef {
                        thread_id: receiver_thread_id,
                        agent_nickname: receiver_agent.agent_nickname,
                        agent_role: receiver_agent.agent_role,
                    }],
                    prompt: Some(prompt),
                    model: None,
                    reasoning_effort: None,
                    agents_states: if mailbox {
                        // Successful deposit says nothing about execution or model visibility.
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
    target: String,
    message: Option<String>,
    items: Option<Vec<UserInput>>,
    #[serde(default)]
    interrupt: bool,
    #[serde(default)]
    w: SendInputMode,
}

#[derive(Clone, Copy, Debug)]
enum SendInputMode {
    Response(ResponseObservationPolicy),
    Mailbox,
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
            return Ok(Self::Mailbox);
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
    submission_id: String,
}

impl ToolOutput for SendInputResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "send_input")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "send_input")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "send_input")
    }
}
