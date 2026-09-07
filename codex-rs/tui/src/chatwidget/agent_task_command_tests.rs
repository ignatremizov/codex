use super::*;
use pretty_assertions::assert_eq;

#[test]
fn task_spawn_preserves_independent_options_and_prompt_boundary() {
    let args = "new task:backend/auth model:custom effort:high fork:all w:cmx -- task:payload";
    assert_eq!(
        parse_agent_command(args),
        Ok(AgentCommand::New {
            task: Some("backend/auth".to_string()),
            fork: Some(AgentForkMode::All),
            response: Some(AgentResponseHandling::new(
                /*commentary*/ true,
                AgentFinalResponseHandling::Presentation,
                /*target_messages*/ true,
                /*queue_input*/ false,
            )),
            model: Some("custom".to_string()),
            reasoning_effort: Some(ReasoningEffort::High),
            prompt: Some(AgentCommandPrompt {
                text: "task:payload",
                offset: args.find("task:payload").unwrap(),
            }),
        })
    );
}

#[test]
fn role_task_label_is_not_resolved_against_root_in_parser() {
    assert_eq!(
        parse_agent_command("role:reviewer task:review"),
        Ok(AgentCommand::SelectOrDispatch {
            selector: AgentSelector {
                kind: AgentSelectorKind::Role("reviewer".to_string()),
                authored: "role:reviewer".to_string(),
            },
            task: Some("review".to_string()),
            fork: None,
            response: None,
            model: None,
            reasoning_effort: None,
            prompt: None,
        })
    );
}

#[test]
fn forced_and_canonical_paths_always_select_existing_targets() {
    for (authored, path) in [
        ("task:review", "review"),
        ("task:/root/backend", "/root/backend"),
        ("/root/backend", "/root/backend"),
    ] {
        assert_eq!(
            parse_agent_command(authored),
            Ok(AgentCommand::SelectOrDispatch {
                selector: AgentSelector {
                    kind: AgentSelectorKind::Task(path.to_string()),
                    authored: authored.to_string(),
                },
                task: None,
                fork: None,
                response: None,
                model: None,
                reasoning_effort: None,
                prompt: None,
            })
        );
        for option in ["task:other", "fork:all", "model:custom", "effort:high"] {
            assert!(parse_agent_command(&format!("{authored} {option}")).is_err());
        }
    }
}

#[test]
fn task_labels_require_a_value_and_cannot_repeat() {
    for command in ["new task:", "task:", "new task:a task:b", "queue 2 task:a"] {
        assert!(parse_agent_command(command).is_err(), "{command}");
    }
}

#[test]
fn adoption_label_accepts_both_resume_orders_and_preserves_follow_up() {
    for args in [
        "resume ref:2 task:import w:x hello",
        "ref:2 resume task:import w:x hello",
    ] {
        assert_eq!(
            parse_agent_command(args),
            Ok(AgentCommand::Resume {
                selector: AgentSelector {
                    kind: AgentSelectorKind::Ref(2),
                    authored: "ref:2".to_string(),
                },
                task: Some("import".to_string()),
                response: Some(AgentResponseHandling::Presentation),
                prompt: Some(AgentCommandPrompt {
                    text: "hello",
                    offset: args.find("hello").unwrap(),
                }),
            })
        );
    }
    for option in ["model:custom", "effort:high", "fork:all"] {
        assert!(parse_agent_command(&format!("resume 2 task:import {option}")).is_err());
    }
}

#[test]
fn explicit_prompt_boundary_keeps_resume_as_user_text() {
    let args = "ref:2 -- resume the review";
    assert_eq!(
        parse_agent_command(args),
        Ok(AgentCommand::SelectOrDispatch {
            selector: AgentSelector {
                kind: AgentSelectorKind::Ref(2),
                authored: "ref:2".to_string(),
            },
            task: None,
            fork: None,
            response: None,
            model: None,
            reasoning_effort: None,
            prompt: Some(AgentCommandPrompt {
                text: "resume the review",
                offset: args.find("resume").unwrap(),
            }),
        }),
    );
}
