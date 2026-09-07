use super::*;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BackgroundTerminalCompletion;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::tui::FrameRequester;
use pretty_assertions::assert_eq;

fn input_and_activity_rows(pane: &BottomPane) -> String {
    let width = 100;
    let area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        width,
        pane.desired_height(width),
    );
    let mut buffer = Buffer::empty(area);
    pane.render(area, &mut buffer);
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .filter(|line| line.contains("running") || line.contains('›'))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn idle_agent_activity_coexists_with_terminals_and_disappears_at_zero() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let mut pane = BottomPane::new(BottomPaneParams {
        app_event_tx: AppEventSender::new(tx),
        frame_requester: FrameRequester::test_dummy(),
        has_input_focus: true,
        enhanced_keys_supported: false,
        placeholder_text: "Ask Codex to do anything".to_string(),
        disable_paste_burst: false,
        animations_enabled: false,
        skills: Some(Vec::new()),
    });
    let empty_height = pane.desired_height(/*width*/ 100);
    insta::assert_snapshot!(input_and_activity_rows(&pane), @r"
    › Ask Codex to do anything
    ");
    pane.set_running_agent_count(/*running*/ 3);
    assert!(!pane.is_task_running());
    assert_eq!(pane.desired_height(/*width*/ 100), empty_height + 2);
    insta::assert_snapshot!(input_and_activity_rows(&pane), @r"
      3 agents running · /agent to view
    › Ask Codex to do anything
    ");
    pane.set_unified_exec_processes(vec![BackgroundTerminalCompletion {
        process_id: "51".to_string(),
        command_display: "sleep 30".to_string(),
    }]);
    insta::assert_snapshot!(input_and_activity_rows(&pane), @r"
      1 background terminal running · /ps to view · /stop to close
      3 agents running · /agent to view
    › Ask Codex to do anything
    ");
    pane.set_running_agent_count(/*running*/ 0);
    assert!(!pane.is_task_running());
    insta::assert_snapshot!(input_and_activity_rows(&pane), @r"
      1 background terminal running · /ps to view · /stop to close
    › Ask Codex to do anything
    ");
    pane.set_unified_exec_processes(Vec::new());
    assert_eq!(pane.desired_height(/*width*/ 100), empty_height);
}

#[test]
fn footer_singular_and_narrow_width_are_bounded() {
    let footer = AgentActivityFooter { running: 1 };
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 40, /*height*/ 1,
    );
    let mut buffer = Buffer::empty(area);
    footer.render(area, &mut buffer);
    let text = (0..area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>();
    insta::assert_snapshot!(text.trim_end(), @"  1 agent running · /agent to view");
    assert_eq!(footer.desired_height(/*width*/ 3), 0);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 10, /*height*/ 1,
    );
    let mut buffer = Buffer::empty(area);
    footer.render(area, &mut buffer);
    let text = (0..area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>();
    insta::assert_snapshot!(text, @"  1 agent ");
}
