use super::*;
use codex_app_server_protocol::AgentForkMode;

pub(in crate::request_processors::thread_processor) fn agent_control_error(
    err: CodexErr,
) -> JSONRPCErrorError {
    match err.details() {
        CodexErrorDetails::InvalidRequest(message)
        | CodexErrorDetails::UnsupportedOperation(message) => invalid_request(message.clone()),
        CodexErrorDetails::ThreadNotFound(thread_id) => {
            invalid_request(format!("agent not found: {thread_id}"))
        }
        _ => internal_error(format!("agent control failed: {err}")),
    }
}

pub(super) fn user_agent_control_item(
    action: &AgentControlAction,
    authored_selector: Option<&str>,
) -> UserAgentControlItem {
    let mut item = UserAgentControlItem::succeeded(match action {
        AgentControlAction::Spawn { .. } => CoreUserAgentControlAction::Spawn,
        AgentControlAction::Prompt { .. } | AgentControlAction::ReservedPrompt { .. } => {
            CoreUserAgentControlAction::Prompt
        }
        AgentControlAction::QueuedPrompt { .. } => CoreUserAgentControlAction::QueuedPrompt,
        AgentControlAction::Resume { .. } => CoreUserAgentControlAction::Resume,
        AgentControlAction::Interrupt { .. } => CoreUserAgentControlAction::Interrupt,
        AgentControlAction::Close { .. } => CoreUserAgentControlAction::Close,
        AgentControlAction::Observe { .. } => CoreUserAgentControlAction::Observe,
    });
    match action {
        AgentControlAction::Spawn {
            role,
            input,
            fork_mode,
            response_handling,
        } => {
            item.authored_selector = authored_selector.map(ToOwned::to_owned);
            item.role = role.clone();
            item.prompt_preview = input.as_deref().and_then(agent_control_prompt_preview);
            item.fork_mode = Some(match fork_mode {
                AgentForkMode::None => CoreUserAgentForkMode::None,
                AgentForkMode::All => CoreUserAgentForkMode::All,
                AgentForkMode::LastNTurns { turns } => {
                    CoreUserAgentForkMode::LastNTurns { turns: *turns }
                }
            });
            apply_agent_control_response_handling(
                &mut item,
                *response_handling,
                /*queued*/ false,
            );
        }
        AgentControlAction::Prompt {
            target,
            input,
            response_handling,
        }
        | AgentControlAction::QueuedPrompt {
            target,
            input,
            response_handling,
        } => {
            item.authored_selector = Some(authored_selector.unwrap_or(target).to_string());
            item.prompt_preview = agent_control_prompt_preview(input);
            apply_agent_control_response_handling(
                &mut item,
                *response_handling,
                matches!(action, AgentControlAction::QueuedPrompt { .. }),
            );
        }
        AgentControlAction::ReservedPrompt { target: _, input } => {
            item.authored_selector = authored_selector.map(ToOwned::to_owned);
            item.prompt_preview = agent_control_prompt_preview(input);
        }
        AgentControlAction::Resume {
            target,
            response_handling,
        } => {
            item.authored_selector = Some(authored_selector.unwrap_or(target).to_string());
            apply_agent_control_response_handling(
                &mut item,
                *response_handling,
                /*queued*/ false,
            );
        }
        AgentControlAction::Interrupt {
            target,
            input,
            response_handling,
        } => {
            item.authored_selector = Some(authored_selector.unwrap_or(target).to_string());
            item.prompt_preview = input.as_deref().and_then(agent_control_prompt_preview);
            if input.is_some() {
                apply_agent_control_response_handling(
                    &mut item,
                    *response_handling,
                    /*queued*/ false,
                );
            }
        }
        AgentControlAction::Close {
            target,
            response_handling,
        } => {
            item.authored_selector = Some(authored_selector.unwrap_or(target).to_string());
            let response_handling = response_handling.map(|response_handling| {
                AgentResponseHandling::new(
                    /*commentary*/ false,
                    response_handling.final_response,
                    /*target_messages*/ false,
                    response_handling.queue_input,
                )
            });
            apply_agent_control_response_handling(
                &mut item,
                response_handling,
                response_handling.is_some_and(|handling| handling.queue_input),
            );
        }
        AgentControlAction::Observe {
            target,
            response_handling,
        } => {
            item.authored_selector = Some(authored_selector.unwrap_or(target).to_string());
            item.observe_commentary = Some(false);
            item.final_response = Some(match response_handling {
                AgentObservationMode::Passive => AgentResponseFinalDelivery::Passive,
                AgentObservationMode::Wake => AgentResponseFinalDelivery::Wake,
                AgentObservationMode::Presentation => AgentResponseFinalDelivery::PresentationOnly,
            });
        }
    }
    item
}

pub(super) fn apply_agent_control_response_handling(
    item: &mut UserAgentControlItem,
    response_handling: Option<AgentResponseHandling>,
    queued: bool,
) {
    let response_handling = response_handling.unwrap_or(AgentResponseHandling::new(
        /*commentary*/ false,
        AgentFinalResponseHandling::Passive,
        /*target_messages*/ false,
        /*queue_input*/ false,
    ));
    let final_response = match response_handling.final_response {
        AgentFinalResponseHandling::None => AgentResponseFinalDelivery::None,
        AgentFinalResponseHandling::Passive => AgentResponseFinalDelivery::Passive,
        AgentFinalResponseHandling::Wake => AgentResponseFinalDelivery::Wake,
        AgentFinalResponseHandling::Presentation => AgentResponseFinalDelivery::PresentationOnly,
    };
    item.observe_commentary = Some(response_handling.commentary);
    item.final_response = Some(final_response);
    item.target_messages = Some(response_handling.target_messages);
    item.queue_input = Some(response_handling.queue_input || queued);
}

fn agent_control_prompt_preview(input: &[V2UserInput]) -> Option<String> {
    const MAX_PREVIEW_CHARS: usize = 240;

    let parts = input
        .iter()
        .filter_map(|item| match item {
            V2UserInput::Text { text, .. } => {
                let text = text.trim();
                (!text.is_empty()).then(|| text.to_string())
            }
            V2UserInput::Image { .. } | V2UserInput::LocalImage { .. } => {
                Some("[image]".to_string())
            }
            V2UserInput::Audio { .. } | V2UserInput::LocalAudio { .. } => {
                Some("[audio]".to_string())
            }
            V2UserInput::Skill { name, .. } => Some(format!("${name}")),
            V2UserInput::Mention { name, .. } => Some(format!("@{name}")),
        })
        .collect::<Vec<_>>();
    let preview = parts.join("\n");
    if preview.is_empty() {
        return None;
    }

    let mut chars = preview.chars();
    let mut truncated = chars.by_ref().take(MAX_PREVIEW_CHARS).collect::<String>();
    if chars.next().is_some() {
        truncated.pop();
        truncated.push('…');
    }
    Some(truncated)
}

pub(super) fn user_agent_response_handling(
    response_handling: AgentResponseHandling,
) -> UserAgentResponseHandling {
    let final_response = match response_handling.final_response {
        AgentFinalResponseHandling::None => UserAgentFinalResponseHandling::None,
        AgentFinalResponseHandling::Passive => UserAgentFinalResponseHandling::Passive,
        AgentFinalResponseHandling::Wake => UserAgentFinalResponseHandling::Wake,
        AgentFinalResponseHandling::Presentation => UserAgentFinalResponseHandling::Presentation,
    };
    UserAgentResponseHandling::from_parts(
        response_handling.commentary,
        final_response,
        response_handling.target_messages,
        response_handling.queue_input,
    )
}

pub(in crate::request_processors::thread_processor) fn agent_response_handling(
    response_handling: UserAgentResponseHandling,
) -> AgentResponseHandling {
    let final_response = match response_handling.final_response() {
        UserAgentFinalResponseHandling::None => AgentFinalResponseHandling::None,
        UserAgentFinalResponseHandling::Passive => AgentFinalResponseHandling::Passive,
        UserAgentFinalResponseHandling::Wake => AgentFinalResponseHandling::Wake,
        UserAgentFinalResponseHandling::Presentation => AgentFinalResponseHandling::Presentation,
    };
    AgentResponseHandling::new(
        response_handling.commentary(),
        final_response,
        response_handling.target_messages(),
        response_handling.queue_input(),
    )
}

pub(super) fn user_agent_final_response_handling(
    response_handling: AgentObservationMode,
) -> UserAgentObservationMode {
    match response_handling {
        AgentObservationMode::Passive => UserAgentObservationMode::Passive,
        AgentObservationMode::Wake => UserAgentObservationMode::Wake,
        AgentObservationMode::Presentation => UserAgentObservationMode::Presentation,
    }
}

pub(super) fn observation_mode_final_response_handling(
    response_handling: UserAgentObservationMode,
) -> AgentFinalResponseHandling {
    match response_handling {
        UserAgentObservationMode::Passive => AgentFinalResponseHandling::Passive,
        UserAgentObservationMode::Wake => AgentFinalResponseHandling::Wake,
        UserAgentObservationMode::Presentation => AgentFinalResponseHandling::Presentation,
    }
}

pub(super) fn agent_observation_binding(
    binding: UserAgentObservationBinding,
) -> AgentObservationBinding {
    match binding {
        UserAgentObservationBinding::ActiveTurn => AgentObservationBinding::ActiveTurn,
        UserAgentObservationBinding::NextTurn => AgentObservationBinding::NextTurn,
        UserAgentObservationBinding::UndeliveredCompletion => {
            AgentObservationBinding::UndeliveredCompletion
        }
    }
}

pub(super) fn agent_final_response_handling(
    response_handling: UserAgentFinalResponseHandling,
) -> AgentFinalResponseHandling {
    match response_handling {
        UserAgentFinalResponseHandling::None => AgentFinalResponseHandling::None,
        UserAgentFinalResponseHandling::Passive => AgentFinalResponseHandling::Passive,
        UserAgentFinalResponseHandling::Wake => AgentFinalResponseHandling::Wake,
        UserAgentFinalResponseHandling::Presentation => AgentFinalResponseHandling::Presentation,
    }
}
