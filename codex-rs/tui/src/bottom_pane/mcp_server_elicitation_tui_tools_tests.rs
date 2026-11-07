use super::*;
use crate::app_event::AppEvent;
use crate::render::renderable::Renderable;
use pretty_assertions::assert_eq;
use ratatui::buffer::Cell;
use serde_json::json;
use tokio::sync::mpsc::unbounded_channel;

#[test]
fn tui_tool_always_allow_is_visible_and_routes_to_requesting_thread() -> anyhow::Result<()> {
    let thread_id = ThreadId::default();
    let request_id = AppServerRequestId::String("tui-tool-approval".to_string());
    let params = McpServerElicitationRequestParams {
        thread_id: thread_id.to_string(),
        turn_id: Some("turn-1".to_string()),
        server_name: "codex_tui".to_string(),
        request: McpServerElicitationRequest::Form {
            message: r#"Allow the codex_tui MCP server to run tool "send_message_to_thread"?"#
                .to_string(),
            requested_schema: serde_json::from_value(json!({
                "type": "object",
                "properties": {},
            }))?,
            meta: Some(json!({
                APPROVAL_META_KIND_KEY: APPROVAL_META_KIND_MCP_TOOL_CALL,
                APPROVAL_PERSIST_KEY: [
                    APPROVAL_PERSIST_SESSION_VALUE,
                    APPROVAL_PERSIST_ALWAYS_VALUE,
                ],
            })),
        },
    };
    let request = McpServerElicitationFormRequest::from_app_server_request(
        thread_id,
        request_id.clone(),
        &params,
    )
    .ok_or_else(|| anyhow::anyhow!("expected a tool approval"))?;
    let (tx, mut rx) = unbounded_channel();
    let mut overlay = McpServerElicitationOverlay::new(
        request,
        AppEventSender::new(tx),
        /*has_input_focus*/ true,
        /*enhanced_keys_supported*/ false,
        /*disable_paste_burst*/ false,
    );
    let answer = overlay
        .current_answer_mut()
        .ok_or_else(|| anyhow::anyhow!("expected the approval choices"))?;
    answer.selection.selected_idx = Some(2);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 120, /*height*/ 10,
    );
    let mut buffer = Buffer::empty(area);
    overlay.render(area, &mut buffer);
    let rendered = buffer
        .content()
        .chunks(usize::from(area.width))
        .map(|row| {
            row.iter()
                .map(Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r#"

      Field 1/1
      Allow the codex_tui MCP server to run tool "send_message_to_thread"?
        1. Allow                   Run the tool and continue
        2. Allow for this session  Run the tool and remember this choice for this session
      › 3. Always allow            Run the tool and remember this choice for future tool calls
        4. Cancel                  Cancel this tool call

      enter to submit | esc to cancel
    "#);

    overlay.select_current_option(/*committed*/ true);
    overlay.submit_answers();
    let AppEvent::SubmitThreadOp {
        thread_id: resolved_thread_id,
        op,
    } = rx.try_recv()?
    else {
        anyhow::bail!("expected the approval resolution");
    };
    assert_eq!(
        (resolved_thread_id, op),
        (
            thread_id,
            Op::ResolveElicitation {
                server_name: "codex_tui".to_string(),
                request_id,
                decision: McpServerElicitationAction::Accept,
                content: None,
                meta: Some(json!({APPROVAL_PERSIST_KEY: APPROVAL_PERSIST_ALWAYS_VALUE})),
            },
        )
    );
    Ok(())
}
