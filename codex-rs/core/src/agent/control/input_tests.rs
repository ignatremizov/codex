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
fn attributed_input_keeps_original_input_separate_from_model_context() {
    let sender = codex_protocol::AgentInputIdentity {
        thread_id: ThreadId::new(),
        nickname: Some("sender".into()),
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let recipient = codex_protocol::AgentInputIdentity {
        thread_id: ThreadId::new(),
        nickname: Some("recipient".into()),
        ..sender.clone()
    };
    let attribution = Box::new(codex_protocol::AgentInputAttribution {
        sender,
        recipient,
        sender_turn_id: "source-turn".into(),
        batch_id: None,
    });
    let input = AgentControlInput::AttributedAgentInput {
        content: vec![text("trusted envelope")],
        attribution: attribution.clone(),
        presentation: vec![text("payload")],
    };
    let submitted = input.into_request().input;
    let replayed: TurnInput =
        serde_json::from_value(serde_json::to_value(&submitted).expect("serialize trusted input"))
            .expect("restore trusted input");
    assert_eq!(replayed, submitted);
    assert_eq!(
        submitted,
        TurnInput::AgentInput {
            content: vec![text("trusted envelope")],
            presentation: AgentInputPresentation::AttributedInput {
                attribution,
                input: vec![text("payload")],
            },
            context: Vec::new(),
        }
    );
}
