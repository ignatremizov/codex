//! Isolate process-global terminal modes while exercising the normal exit and outer restore.

use crate::app::AppExitInfo;
use crate::app::ExitReason;
use crate::app::ResumableThread;
use codex_protocol::ThreadId;
use crossterm::queue;
use crossterm::style::Print;
use pretty_assertions::assert_eq;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use std::io::Write;
use std::process::Command;

const CHILD_ENV: &str = "CODEX_TUI_EXIT_FRAME_CHILD";
const BEGIN: &str = "CODEX_EXIT_FRAME_BEGIN";
const END: &str = "CODEX_EXIT_FRAME_END";

#[test]
fn exit_retains_owned_frame_without_replaying_inline_history() -> color_eyre::Result<()> {
    for mode in ["owned", "inline"] {
        let output = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "tui::exit::tests::exit_frame_child",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD_ENV, mode)
            .output()?;
        assert!(
            output.status.success(),
            "{mode}: stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let output = String::from_utf8(output.stdout)?;
        let (_, output) = output
            .split_once(BEGIN)
            .ok_or_else(|| color_eyre::eyre::eyre!("missing exit-frame capture start"))?;
        let (output, _) = output
            .split_once(END)
            .ok_or_else(|| color_eyre::eyre::eyre!("missing exit-frame capture end"))?;
        let mut parser = vt100::Parser::new(
            /*rows*/ 24, /*cols*/ 80, /*scrollback_len*/ 128,
        );
        parser.process(output.as_bytes());
        let screen = parser.screen();
        let visible = screen.contents();
        assert!(visible.contains("recent output"));
        assert!(visible.contains("To continue this session, run:"));
        assert!(visible.contains("codex resume 00000000-0000-0000-0000-000000000001"));
        assert_eq!(visible.matches("recent output").count(), 1);
        assert_eq!(visible.contains("test | ready"), mode == "owned");
        assert_eq!(
            (screen.alternate_screen(), screen.mouse_protocol_mode()),
            (false, vt100::MouseProtocolMode::None),
        );
        let mut scrollback = screen.clone();
        scrollback.set_scrollback(/*rows*/ usize::MAX);
        assert!(scrollback.contents().starts_with("shell$ codex"));
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "spawned by the exit-frame regression test"]
async fn exit_frame_child() -> color_eyre::Result<()> {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return Ok(());
    };
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.terminal.backend_mut().write_all(BEGIN.as_bytes())?;
    queue!(tui.terminal.backend_mut(), Print("shell$ codex\r\n"))?;
    let owned = mode == "owned";
    let inline_row = if owned {
        1
    } else {
        queue!(tui.terminal.backend_mut(), Print("recent output\r\n"))?;
        2
    };
    tui.terminal.set_viewport_area(Rect::new(
        /*x*/ 0, inline_row, /*width*/ 80, /*height*/ 3,
    ));
    tui.set_owned_screen(owned)?;
    tui.draw(if owned { 24 } else { 3 }, |frame| {
        let area = frame.area();
        let bottom = Rect::new(area.x, area.bottom() - 3, area.width, /*height*/ 3);
        Paragraph::new(vec![
            Line::from(if owned { "recent output" } else { "draft" }),
            Line::from(">"),
            Line::from("test | ready"),
        ])
        .render(bottom, frame.buffer_mut());
    })?;
    tui.finish()?;
    tui.finish()?;
    crate::tui::restore_after_exit()?;
    assert!(!tui.is_alt_screen_active());
    let thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")?;
    let info = AppExitInfo {
        token_usage: Default::default(),
        thread_id: Some(thread_id),
        resume_hint: Some(ResumableThread {
            thread_id,
            thread_name: None,
        }),
        disconnect_info: None,
        update_action: None,
        exit_reason: ExitReason::UserRequested,
    };
    for line in info.format_exit_messages(/*color_enabled*/ false) {
        queue!(tui.terminal.backend_mut(), Print(line), Print("\r\n"))?;
    }
    tui.terminal.backend_mut().write_all(END.as_bytes())?;
    tui.terminal.backend_mut().flush()?;
    Ok(())
}
