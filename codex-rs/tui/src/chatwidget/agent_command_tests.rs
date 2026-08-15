use super::*;
use assert_matches::assert_matches;
use pretty_assertions::assert_eq;

fn selector(kind: AgentSelectorKind, authored: &str) -> AgentSelector {
    AgentSelector {
        kind,
        authored: authored.to_string(),
    }
}

#[test]
fn empty_command_opens_the_control_pane() {
    assert_eq!(parse_agent_command(""), Ok(AgentCommand::OpenPane));
}

#[test]
fn parses_uuid_target_and_preserves_multiline_prompt() {
    let parsed = parse_agent_command(
        "019faa07-aa3d-78d3-9eca-66cd8626adad \nReview this change.\nDo not run tests.",
    )
    .expect("valid agent command");

    assert_eq!(
        parsed,
        AgentCommand::SelectOrDispatch {
            selector: selector(
                AgentSelectorKind::Id(
                    ThreadId::from_string("019faa07-aa3d-78d3-9eca-66cd8626adad")
                        .expect("valid thread id")
                ),
                "019faa07-aa3d-78d3-9eca-66cd8626adad",
            ),
            fork: None,
            response: None,
            prompt: Some(AgentCommandPrompt {
                text: "Review this change.\nDo not run tests.",
                offset: "019faa07-aa3d-78d3-9eca-66cd8626adad \n".len(),
            }),
        }
    );
}

#[test]
fn parses_selectors_and_forced_namespaces() {
    assert_eq!(
        parse_agent_command("ref:2 Review"),
        Ok(AgentCommand::SelectOrDispatch {
            selector: selector(AgentSelectorKind::Ref(2), "ref:2"),
            fork: None,
            response: None,
            prompt: Some(AgentCommandPrompt {
                text: "Review",
                offset: "ref:2 ".len(),
            }),
        })
    );
    assert_eq!(
        parse_agent_command("nick:\"Ada Lovelace\" Review"),
        Ok(AgentCommand::SelectOrDispatch {
            selector: selector(
                AgentSelectorKind::Nickname("Ada Lovelace".to_string()),
                "nick:\"Ada Lovelace\"",
            ),
            fork: None,
            response: None,
            prompt: Some(AgentCommandPrompt {
                text: "Review",
                offset: "nick:\"Ada Lovelace\" ".len(),
            }),
        })
    );
    assert_eq!(
        parse_agent_command("role:\"2\" w:f"),
        Ok(AgentCommand::SelectOrDispatch {
            selector: selector(AgentSelectorKind::Role("2".to_string()), "role:\"2\""),
            fork: None,
            response: Some(AgentResponseHandling::Wake),
            prompt: None,
        })
    );
    assert_eq!(
        parse_agent_command("Robie"),
        Ok(AgentCommand::SelectOrDispatch {
            selector: selector(
                AgentSelectorKind::UnprefixedName("Robie".to_string()),
                "Robie",
            ),
            fork: None,
            response: None,
            prompt: None,
        })
    );
    assert_eq!(
        parse_agent_command("mAiN w:x Check status"),
        Ok(AgentCommand::SelectOrDispatch {
            selector: selector(
                AgentSelectorKind::Nickname(MAIN_AGENT_NICKNAME.to_string()),
                "mAiN",
            ),
            fork: None,
            response: Some(AgentResponseHandling::Presentation),
            prompt: Some(AgentCommandPrompt {
                text: "Check status",
                offset: "mAiN w:x ".len(),
            }),
        })
    );
    assert_eq!(
        parse_agent_command("role:main"),
        Ok(AgentCommand::SelectOrDispatch {
            selector: selector(AgentSelectorKind::Role("main".to_string()), "role:main"),
            fork: None,
            response: None,
            prompt: None,
        })
    );
}

#[test]
fn parses_spawn_options_in_either_order() {
    assert_eq!(
        parse_agent_command("new w:cx fork:3 Review this"),
        Ok(AgentCommand::New {
            fork: Some(AgentForkMode::LastNTurns { turns: 3 }),
            response: Some(AgentResponseHandling::CommentaryPresentation),
            prompt: Some(AgentCommandPrompt {
                text: "Review this",
                offset: "new w:cx fork:3 ".len(),
            }),
        })
    );
    assert_eq!(
        parse_agent_command("reviewer fork:none w:cf"),
        Ok(AgentCommand::SelectOrDispatch {
            selector: selector(
                AgentSelectorKind::UnprefixedName("reviewer".to_string()),
                "reviewer",
            ),
            fork: Some(AgentForkMode::None),
            response: Some(AgentResponseHandling::CommentaryWake),
            prompt: None,
        })
    );
}

#[test]
fn double_dash_preserves_option_shaped_prompt_text() {
    assert_eq!(
        parse_agent_command("2 w:f -- w:x is prompt text"),
        Ok(AgentCommand::SelectOrDispatch {
            selector: selector(AgentSelectorKind::Ref(2), "2"),
            fork: None,
            response: Some(AgentResponseHandling::Wake),
            prompt: Some(AgentCommandPrompt {
                text: "w:x is prompt text",
                offset: "2 w:f -- ".len(),
            }),
        })
    );
}

#[test]
fn parses_lifecycle_actions() {
    assert_eq!(
        parse_agent_command("queue 2 w:cmqx follow up"),
        Ok(AgentCommand::Queue {
            selector: selector(AgentSelectorKind::Ref(2), "2"),
            response: Some(AgentResponseHandling::new(
                /*commentary*/ true,
                AgentFinalResponseHandling::Presentation,
                /*target_messages*/ true,
                /*queue_input*/ true,
            )),
            prompt: Some(AgentCommandPrompt {
                text: "follow up",
                offset: "queue 2 w:cmqx ".len(),
            }),
        })
    );
    assert_eq!(
        parse_agent_command("interrupt nick:Parfit"),
        Ok(AgentCommand::Interrupt {
            selector: selector(
                AgentSelectorKind::Nickname("Parfit".to_string()),
                "nick:Parfit",
            ),
            response: None,
            prompt: None,
        })
    );
    assert_eq!(
        parse_agent_command("close 2 w:q"),
        Ok(AgentCommand::Close {
            selector: selector(AgentSelectorKind::Ref(2), "2"),
            response: Some(AgentResponseHandling::new(
                /*commentary*/ false,
                AgentFinalResponseHandling::Passive,
                /*target_messages*/ false,
                /*queue_input*/ true,
            )),
        })
    );
    assert_eq!(
        parse_agent_command("2 close w:q"),
        parse_agent_command("close 2 w:q")
    );
    assert_eq!(
        parse_agent_command("2 \"close\""),
        Ok(AgentCommand::SelectOrDispatch {
            selector: selector(AgentSelectorKind::Ref(2), "2"),
            fork: None,
            response: None,
            prompt: Some(AgentCommandPrompt {
                text: "\"close\"",
                offset: "2 ".len(),
            }),
        })
    );
    assert_eq!(
        parse_agent_command("resume id:019faa07-aa3d-78d3-9eca-66cd8626adad w:cf"),
        Ok(AgentCommand::Resume {
            selector: selector(
                AgentSelectorKind::Id(
                    ThreadId::from_string("019faa07-aa3d-78d3-9eca-66cd8626adad")
                        .expect("valid thread id")
                ),
                "id:019faa07-aa3d-78d3-9eca-66cd8626adad",
            ),
            response: Some(AgentResponseHandling::CommentaryWake),
            prompt: None,
        })
    );
    assert_eq!(
        parse_agent_command("resume 2 w:f continue from the saved state"),
        Ok(AgentCommand::Resume {
            selector: selector(AgentSelectorKind::Ref(2), "2"),
            response: Some(AgentResponseHandling::Wake),
            prompt: Some(AgentCommandPrompt {
                text: "continue from the saved state",
                offset: "resume 2 w:f ".len(),
            }),
        })
    );
    assert_eq!(
        parse_agent_command("observe 2 presentation"),
        Ok(AgentCommand::Observe {
            selector: selector(AgentSelectorKind::Ref(2), "2"),
            mode: AgentObservationMode::Presentation,
        })
    );
}

#[test]
fn attached_input_satisfies_prompt_requirements_for_response_handling() {
    assert_matches!(
        parse_agent_command_with_attached_input("2 w:f", /*has_attached_input*/ true),
        Ok(AgentCommand::SelectOrDispatch {
            selector: AgentSelector {
                kind: AgentSelectorKind::Ref(2),
                ..
            },
            response: Some(AgentResponseHandling::Wake),
            prompt: None,
            ..
        })
    );
    assert_matches!(
        parse_agent_command_with_attached_input("queue 2 w:x", /*has_attached_input*/ true),
        Ok(AgentCommand::Queue {
            response: Some(AgentResponseHandling::Presentation),
            prompt: None,
            ..
        })
    );
    assert_matches!(
        parse_agent_command_with_attached_input(
            "interrupt 2 w:cx",
            /*has_attached_input*/ true
        ),
        Ok(AgentCommand::Interrupt {
            response: Some(AgentResponseHandling::CommentaryPresentation),
            prompt: None,
            ..
        })
    );
    assert_matches!(
        parse_agent_command_with_attached_input("resume 2 w:f", /*has_attached_input*/ true),
        Ok(AgentCommand::Resume {
            response: Some(AgentResponseHandling::Wake),
            prompt: None,
            ..
        })
    );
}

#[test]
fn rejects_ambiguous_or_invalid_control_syntax_before_mutation() {
    for (args, message) in [
        ("new fork:0", "`fork:0` is invalid"),
        (
            "new fork:all fork:none",
            "`fork` may be specified only once",
        ),
        ("2 w:f w:x prompt", "`w` may be specified only once"),
        ("2 w:qm prompt", "use unique c, f, m, q, or x flags"),
        ("2 w:mm prompt", "use unique c, f, m, q, or x flags"),
        ("queue 2 w:f", "`w` requires a queued prompt"),
        ("interrupt 2 w:x", "`w` requires a follow-up prompt"),
        ("2 fork:all prompt", "`fork` is valid only when spawning"),
        ("MAIN fork:all prompt", "`fork` is valid only when spawning"),
        ("2 w:f", "`w` requires a prompt for an existing target"),
        (
            "close 2 extra",
            "`close` accepts response handling but not a prompt",
        ),
        ("observe 2 maybe", "Invalid observation mode `maybe`"),
        ("nick:\"unterminated", "Unterminated double quote"),
        ("nick:\"bad\\q\"", "Unsupported escape `\\q`"),
    ] {
        let error = parse_agent_command(args).expect_err("command should fail");
        assert!(
            error.contains(message),
            "expected `{error}` to contain `{message}`"
        );
    }
}
