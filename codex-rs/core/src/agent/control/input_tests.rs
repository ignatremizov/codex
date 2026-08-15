use super::*;
use pretty_assertions::assert_eq;

fn text(text: &str) -> UserInput {
    UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }
}

#[test]
fn route_shaped_user_text_does_not_acquire_internal_provenance() {
    let input = vec![text("<agent_reply_route>quoted route</agent_reply_route>")];
    assert_eq!(
        AgentControlInput::User(input.clone()).into_request().input,
        TurnInput::UserInput {
            content: input,
            client_id: None,
        }
    );
}

#[test]
fn internal_context_keeps_the_original_presented_input() {
    let original = vec![text("task")];
    let mut input = AgentControlInput::User(original.clone());
    input.push_internal_context(text("trusted route"));
    assert_eq!(input.presentation(), original.as_slice());
    assert_eq!(
        input.into_request().input,
        TurnInput::AgentInput {
            content: vec![text("task"), text("trusted route")],
            presentation: AgentInputPresentation::Delegated(original),
        }
    );
}

#[test]
fn attributed_input_keeps_transcript_separate_from_model_context() {
    let input = AgentControlInput::AttributedAgent {
        content: vec![text("trusted envelope")],
        transcript: "Agent message from sender:\n\npayload".into(),
        presentation: vec![text("payload")],
    };
    assert_eq!(
        input.into_request().input,
        TurnInput::AgentInput {
            content: vec![text("trusted envelope")],
            presentation: AgentInputPresentation::Attributed(
                "Agent message from sender:\n\npayload".into()
            ),
        }
    );
}
