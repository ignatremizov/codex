use super::*;
use pretty_assertions::assert_eq;

#[test]
fn optional_metadata_preserves_order_without_empty_delimiters() {
    let render = |model, effort| {
        IdentityHeader {
            fallback: "agent-id",
            nickname: Some("Pascal"),
            role: Some(" "),
            task_path: None,
            agent_ref: Some(""),
            model,
            reasoning_effort: effort,
        }
        .render()
    };
    let lines = [
        render(Some("gpt-6-astra"), None),
        render(None, Some(&ReasoningEffort::Low)),
        render(None, None),
    ];
    insta::assert_snapshot!(
        lines.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"),
        @r"
    Pascal (gpt-6-astra)
    Pascal (low)
    Pascal
    "
    );
    assert_eq!(
        lines,
        [
            Line::from(vec!["Pascal".magenta().bold(), " (gpt-6-astra)".dim()]),
            Line::from(vec!["Pascal".magenta().bold(), " (low)".dim()]),
            Line::from(vec!["Pascal".magenta().bold(), "".dim()]),
        ],
    );
}
