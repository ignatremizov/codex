use super::*;
use pretty_assertions::assert_eq;

#[test]
fn forced_path_popup_selects_canonical_existing_agent_and_hides_alias_duplicates() {
    let id = ThreadId::from_string("019faa07-aa3d-78d3-9eca-66cd8626adad").unwrap();
    let target = AgentPromptTarget {
        thread_id: Some(id),
        selector: "3".to_string(),
        label: "Pascal [coder] /root/backend/auth".to_string(),
    };
    let path_target = AgentPromptTarget {
        selector: "task:/root/backend/auth".to_string(),
        ..target.clone()
    };
    let mut popup = AgentTargetPopup::new(
        vec![target.clone(), path_target.clone()],
        "",
        AgentTargetCompletionScope::Any,
    );
    assert_eq!(popup.filtered_targets(), vec![target]);
    popup.set_query("task:/root/backend");
    assert_eq!(popup.selected_target(), Some(path_target));
    let rows = popup
        .rows()
        .into_iter()
        .map(|row| format!("{}  {}", row.name, row.description.unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rows, @r"
    task:/root/backend/auth  Pascal [coder] /root/backend/auth  019faa07-aa3d-78d3-9eca-66cd8626adad
    ");
}

#[test]
fn task_option_does_not_stop_later_model_completion() {
    let input = "/agent new task:review model:custom";
    assert_eq!(
        agent_target_completion(input, input.len(), &[]),
        Some(AgentTargetCompletion {
            range: 23..35,
            query: "model:custom".to_string(),
            scope: AgentTargetCompletionScope::Model,
            action: None,
        }),
    );
}
