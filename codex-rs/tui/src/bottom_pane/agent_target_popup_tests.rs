use super::*;
use pretty_assertions::assert_eq;

fn thread_id(value: &str) -> ThreadId {
    ThreadId::from_string(value).expect("valid thread id")
}

fn targets() -> Vec<AgentPromptTarget> {
    vec![
        AgentPromptTarget {
            thread_id: Some(thread_id("019f9906-992c-7cf1-9dba-55bc75159c9c")),
            selector: "Main".to_string(),
            label: "Main [default]".to_string(),
        },
        AgentPromptTarget {
            thread_id: Some(thread_id("019faa07-aa3d-78d3-9eca-66cd8626adad")),
            selector: "2".to_string(),
            label: "Robie [explorer]".to_string(),
        },
        AgentPromptTarget {
            thread_id: Some(thread_id("019fbb08-bb4e-79e4-afdb-77de9737bebe")),
            selector: "3".to_string(),
            label: "Herschel [worker] · closed".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "new".to_string(),
            label: "New default agent".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "queue".to_string(),
            label: "Queue a follow-up for an agent".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "interrupt".to_string(),
            label: "Interrupt an agent".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "close".to_string(),
            label: "Close an agent".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "resume".to_string(),
            label: "Resume or adopt an agent".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "observe".to_string(),
            label: "Change response observation".to_string(),
        },
        AgentPromptTarget {
            thread_id: None,
            selector: "reviewer".to_string(),
            label: "New reviewer agent".to_string(),
        },
    ]
}

fn render_popup(popup: &AgentTargetPopup) -> String {
    let width = 78;
    let area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        width,
        popup.calculate_required_height(width),
    );
    let mut buffer = Buffer::empty(area);
    popup.render_ref(area, &mut buffer);

    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn target_completion_only_covers_agent_first_argument() {
    assert_eq!(
        agent_target_completion("/agent ", "/agent ".len()),
        Some(AgentTargetCompletion {
            range: "/agent ".len().."/agent ".len(),
            query: String::new(),
            scope: AgentTargetCompletionScope::Any,
            action: None,
        })
    );
    assert_eq!(
        agent_target_completion("/agent 019f do this", "/agent 019f".len()),
        Some(AgentTargetCompletion {
            range: "/agent ".len().."/agent 019f".len(),
            query: "019f".to_string(),
            scope: AgentTargetCompletionScope::Any,
            action: None,
        })
    );
    assert_eq!(
        agent_target_completion("/agent 019f do this", "/agent 019f do".len()),
        None
    );
    assert_eq!(agent_target_completion("/agents 019f", 12), None);
}

#[test]
fn action_target_completion_only_covers_existing_target_argument() {
    assert_eq!(
        agent_target_completion("/agent close ", "/agent close ".len()),
        Some(AgentTargetCompletion {
            range: "/agent close ".len().."/agent close ".len(),
            query: String::new(),
            scope: AgentTargetCompletionScope::ExistingTarget,
            action: Some("close"),
        })
    );
    assert_eq!(
        agent_target_completion(
            "/agent queue nick:\"Ada Lovelace\" w:f follow up",
            "/agent queue nick:\"Ada Lovelace\"".len(),
        ),
        Some(AgentTargetCompletion {
            range: "/agent queue ".len().."/agent queue nick:\"Ada Lovelace\"".len(),
            query: "nick:\"Ada Lovelace\"".to_string(),
            scope: AgentTargetCompletionScope::ExistingTarget,
            action: Some("queue"),
        })
    );
    assert_eq!(
        agent_target_completion(
            "/agent observe 2 presentation",
            "/agent observe 2 presentation".len(),
        ),
        Some(AgentTargetCompletion {
            range: "/agent observe 2 ".len().."/agent observe 2 presentation".len(),
            query: "presentation".to_string(),
            scope: AgentTargetCompletionScope::ObservationMode,
            action: Some("observe"),
        })
    );
    assert_eq!(
        agent_target_completion("/agent new w:f", "/agent new w:f".len()),
        None
    );
}

#[test]
fn popup_filters_by_ref_uuid_or_agent_label() {
    let mut popup = AgentTargetPopup::new(targets(), "019fbb", AgentTargetCompletionScope::Any);
    assert_eq!(
        popup.selected_target().map(|target| target.label),
        Some("Herschel [worker] · closed".to_string())
    );

    popup.set_query("robie");
    assert_eq!(
        popup
            .selected_target()
            .and_then(|target| target.thread_id)
            .map(|thread_id| thread_id.to_string()),
        Some("019faa07-aa3d-78d3-9eca-66cd8626adad".to_string())
    );

    popup.set_query("3");
    assert_eq!(
        popup.selected_target().map(|target| target.label),
        Some("Herschel [worker] · closed".to_string())
    );

    popup.set_query("id:019faa");
    assert_eq!(
        popup.selected_target().map(|target| target.selector),
        Some("2".to_string())
    );

    popup.set_query("ref:3");
    assert_eq!(
        popup.selected_target().map(|target| target.label),
        Some("Herschel [worker] · closed".to_string())
    );

    popup.set_query("1");
    assert_eq!(
        popup.selected_target().map(|target| target.label),
        Some("Main [default]".to_string())
    );

    popup.set_query("ref:1");
    assert_eq!(
        popup.selected_target().map(|target| target.selector),
        Some("Main".to_string())
    );

    popup.set_query("nick:\"Rob");
    assert_eq!(
        popup.selected_target().map(|target| target.selector),
        Some("2".to_string())
    );

    popup.set_query("role:rev");
    assert_eq!(
        popup.selected_target().map(|target| target.selector),
        Some("reviewer".to_string())
    );
}

#[test]
fn action_target_scope_excludes_actions_and_spawn_choices() {
    let popup = AgentTargetPopup::new(targets(), "", AgentTargetCompletionScope::ExistingTarget);

    assert_eq!(
        popup
            .filtered_targets()
            .into_iter()
            .map(|target| target.selector)
            .collect::<Vec<_>>(),
        vec!["Main".to_string(), "2".to_string(), "3".to_string()]
    );
}

#[test]
fn agent_target_popup_snapshot() {
    let popup = AgentTargetPopup::new(targets(), "", AgentTargetCompletionScope::Any);
    insta::assert_snapshot!(render_popup(&popup), @r"
      Main       Main [default]  019f9906-992c-7cf1-9dba-55bc75159c9c
      2          Robie [explorer]  019faa07-aa3d-78d3-9eca-66cd8626adad
      3          Herschel [worker] · closed  019fbb08-bb4e-79e4-afdb-77de9737bebe
      new        New default agent
      queue      Queue a follow-up for an agent
      interrupt  Interrupt an agent
      close      Close an agent
      resume     Resume or adopt an agent
    ");
}

#[test]
fn observation_mode_popup_snapshot() {
    let targets = AGENT_OBSERVATION_MODE_CHOICES
        .map(|(selector, label)| AgentPromptTarget {
            thread_id: None,
            selector: selector.to_string(),
            label: label.to_string(),
        })
        .to_vec();
    let popup = AgentTargetPopup::new(targets, "", AgentTargetCompletionScope::ObservationMode);

    insta::assert_snapshot!(render_popup(&popup), @r"
      passive  Deliver the final response without waking
      wake  Deliver the final response and wake
      presentation  Keep the final response out of model context
    ");
}

#[test]
fn configured_role_popup_snapshot() {
    let popup = AgentTargetPopup::new(targets(), "rev", AgentTargetCompletionScope::Any);

    insta::assert_snapshot!(render_popup(&popup), @r"
      reviewer  New reviewer agent
    ");
}
