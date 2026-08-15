use super::*;

pub(super) fn agent_control_error(err: CodexErr) -> JSONRPCErrorError {
    match err.details() {
        CodexErrorDetails::InvalidRequest(message)
            if message.starts_with("queued input requires an idle target;") =>
        {
            let mut error = invalid_request(message.clone());
            error.data = Some(serde_json::json!({ "reason": "targetActive" }));
            error
        }
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
            let (observe_commentary, final_response) =
                agent_control_response_observation(*response_handling);
            item.observe_commentary = observe_commentary;
            item.final_response = final_response;
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
            let (observe_commentary, final_response) =
                agent_control_response_observation(*response_handling);
            item.observe_commentary = observe_commentary;
            item.final_response = final_response;
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
            let (observe_commentary, final_response) =
                agent_control_response_observation(*response_handling);
            item.observe_commentary = observe_commentary;
            item.final_response = final_response;
        }
        AgentControlAction::Interrupt {
            target,
            input,
            response_handling,
        } => {
            item.authored_selector = Some(authored_selector.unwrap_or(target).to_string());
            item.prompt_preview = input.as_deref().and_then(agent_control_prompt_preview);
            if input.is_some() {
                let (observe_commentary, final_response) =
                    agent_control_response_observation(*response_handling);
                item.observe_commentary = observe_commentary;
                item.final_response = final_response;
            }
        }
        AgentControlAction::Close { target } => {
            item.authored_selector = Some(authored_selector.unwrap_or(target).to_string());
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
) -> (Option<bool>, Option<AgentResponseFinalDelivery>) {
    let (observe_commentary, final_response) = match response_handling {
        None => (false, AgentResponseFinalDelivery::Passive),
        Some(AgentResponseHandling::Commentary) => (true, AgentResponseFinalDelivery::Passive),
        Some(AgentResponseHandling::Wake) => (false, AgentResponseFinalDelivery::Wake),
        Some(AgentResponseHandling::Presentation) => {
            (false, AgentResponseFinalDelivery::PresentationOnly)
        }
        Some(AgentResponseHandling::CommentaryWake) => (true, AgentResponseFinalDelivery::Wake),
        Some(AgentResponseHandling::CommentaryPresentation) => {
            (true, AgentResponseFinalDelivery::PresentationOnly)
        }
    };
    (Some(observe_commentary), Some(final_response))
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
    match response_handling {
        AgentResponseHandling::Commentary => UserAgentResponseHandling::Commentary,
        AgentResponseHandling::Wake => UserAgentResponseHandling::Wake,
        AgentResponseHandling::Presentation => UserAgentResponseHandling::Presentation,
        AgentResponseHandling::CommentaryWake => UserAgentResponseHandling::CommentaryWake,
        AgentResponseHandling::CommentaryPresentation => {
            UserAgentResponseHandling::CommentaryPresentation
        }
    }
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
