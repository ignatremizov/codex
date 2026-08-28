use super::*;

#[test]
fn parses_shell_response_options() {
    let cases = [
        (
            "echo passive",
            "echo passive",
            ThreadShellCommandResponseHandling::default(),
        ),
        (
            "[w:f] echo wake",
            "echo wake",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::Wake,
                queue_command: false,
            },
        ),
        (
            "[w:q] echo queued",
            "echo queued",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::Passive,
                queue_command: true,
            },
        ),
        (
            "[w:fq]echo queued",
            "echo queued",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::Wake,
                queue_command: true,
            },
        ),
        (
            "[w:x] echo private",
            "echo private",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::PresentationOnly,
                queue_command: false,
            },
        ),
        (
            "[w:fx] echo passive",
            "echo passive",
            ThreadShellCommandResponseHandling::default(),
        ),
        (
            "[w:qx] echo private",
            "echo private",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::PresentationOnly,
                queue_command: true,
            },
        ),
        (
            "[w:fqx] echo queued",
            "echo queued",
            ThreadShellCommandResponseHandling {
                final_delivery: ThreadShellCommandFinalDelivery::Passive,
                queue_command: true,
            },
        ),
    ];

    for (input, command, response_handling) in cases {
        assert_eq!(
            parse_user_shell_command(input),
            Ok(ParsedUserShellCommand {
                command,
                response_handling,
            })
        );
    }
}

#[test]
fn rejects_invalid_shell_response_options() {
    for input in [
        "[w:] echo empty",
        "[w:c] echo commentary",
        "[w:m] echo messages",
        "[w:ff] echo duplicate",
        "[w:qf] echo order",
        "[w:f echo unclosed",
    ] {
        assert!(
            parse_user_shell_command(input).is_err(),
            "expected `{input}` to be rejected"
        );
    }
}
