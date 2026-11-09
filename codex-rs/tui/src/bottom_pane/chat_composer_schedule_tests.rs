use super::InputResult;
use super::tests::new_test_composer;
use crate::slash_command::SlashCommand;
use pretty_assertions::assert_eq;

#[test]
fn schedule_uses_durable_delivery_when_the_user_presses_the_queue_binding() {
    for running in [false, true] {
        let (mut composer, _events) = new_test_composer();
        composer.set_task_running(running);
        composer.set_text_content(
            "/schedule in 2h review results".into(),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            composer.handle_submission(/*should_queue*/ true),
            (
                InputResult::CommandWithArgs(
                    SlashCommand::Schedule,
                    "in 2h review results".into(),
                    Vec::new()
                ),
                true
            ),
        );
    }
}
