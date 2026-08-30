use super::*;

#[test]
fn parses_shell_response_options() {
    let cases = [
        (
            "echo passive",
            "echo passive",
            ThreadShellCommandResponseHandling::default(),
            None,
        ),
        (
            r"w:\tools\run.cmd",
            r"w:\tools\run.cmd",
            ThreadShellCommandResponseHandling::default(),
            None,
        ),
        (
            " w:f arg",
            "w:f arg",
            ThreadShellCommandResponseHandling::default(),
            None,
        ),
        (
            "w:format input",
            "w:format input",
            ThreadShellCommandResponseHandling::default(),
            None,
        ),
        (
            "w:f echo wake",
            "echo wake",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::Wake,
                queue_command: false,
            },
            Some(3),
        ),
        (
            "w:q echo queued",
            "echo queued",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::Passive,
                queue_command: true,
            },
            Some(3),
        ),
        (
            "w:fq echo queued",
            "echo queued",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::Wake,
                queue_command: true,
            },
            Some(4),
        ),
        (
            "w:x echo private",
            "echo private",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::PresentationOnly,
                queue_command: false,
            },
            Some(3),
        ),
        (
            "w:fx echo passive",
            "echo passive",
            ThreadShellCommandResponseHandling::default(),
            Some(4),
        ),
        (
            "w:qx echo private",
            "echo private",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::PresentationOnly,
                queue_command: true,
            },
            Some(4),
        ),
        (
            "w:fqx echo queued",
            "echo queued",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::Passive,
                queue_command: true,
            },
            Some(5),
        ),
    ];

    for (input, command, response_handling, response_option_prefix_len) in cases {
        assert_eq!(
            parse_user_shell_command(input),
            Ok(ParsedUserShellCommand {
                command,
                response_handling,
                response_option_prefix_len,
            })
        );
    }
}

#[test]
fn rejects_invalid_shell_response_options() {
    for input in [
        "w: echo empty",
        "w:c echo commentary",
        "w:m echo messages",
        "w:ff echo duplicate",
        "w:qf echo order",
        "w:fc echo commentary",
    ] {
        assert!(
            parse_user_shell_command(input).is_err(),
            "expected `{input}` to be rejected"
        );
    }
}
