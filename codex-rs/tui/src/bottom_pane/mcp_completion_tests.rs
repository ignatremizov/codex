use super::*;
use crate::bottom_pane::command_popup::CommandItem;
use crate::bottom_pane::command_popup::CommandPopup;
use crate::bottom_pane::command_popup::CommandPopupFlags;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::WidgetRef;

#[test]
fn completion_preserves_server_names_with_spaces() {
    let names = vec!["team docs".to_string(), "other".to_string()];
    assert_eq!(
        candidates("/mcp use TEAM ", &names),
        Some(vec![McpCompletion::Server("team docs".into())])
    );
    assert_eq!(
        candidates("/mcp ", &names),
        Some(vec![McpCompletion::Use, McpCompletion::Verbose])
    );
    assert_eq!(candidates("/mc", &names), None);
    assert_eq!(candidates("/mcpfoo ", &names), None);
}

#[test]
fn server_completion_inserts_the_full_command() {
    let names = vec!["team docs".to_string()];
    let completions = candidates("/mcp use te", &names).expect("MCP argument mode");
    assert_eq!(
        completions
            .iter()
            .map(McpCompletion::text)
            .collect::<Vec<_>>(),
        vec!["/mcp use team docs"]
    );
}

#[test]
fn mcp_popup_filters_and_renders_server_completion() {
    let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
    popup.set_mcp_server_names(vec!["team docs".into(), "other".into()]);
    popup.on_composer_text_change("/mcp use te".into());
    assert_eq!(
        popup.selected_item(),
        Some(CommandItem::Mcp(McpCompletion::Server("team docs".into())))
    );
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 100, /*height*/ 1,
    );
    let mut buffer = Buffer::empty(area);
    popup.render_ref(area, &mut buffer);
    let rendered = (0..area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>();
    insta::assert_snapshot!(rendered.trim_end(), @"› /mcp use team docs  request explicit MCP context");
}
