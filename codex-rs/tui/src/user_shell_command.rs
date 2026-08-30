use codex_app_server_protocol::ThreadShellCommandFinalDelivery;
use codex_app_server_protocol::ThreadShellCommandResponseHandling;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ParsedUserShellCommand<'a> {
    pub(crate) command: &'a str,
    pub(crate) response_handling: ThreadShellCommandResponseHandling,
    pub(crate) response_option_prefix_len: Option<usize>,
}

pub(crate) fn parse_user_shell_command(input: &str) -> Result<ParsedUserShellCommand<'_>, String> {
    let input = input.trim_end();
    let command = input.trim_start();
    let Some(options) = input.strip_prefix("w:") else {
        return Ok(ParsedUserShellCommand {
            command,
            response_handling: ThreadShellCommandResponseHandling::default(),
            response_option_prefix_len: None,
        });
    };
    let (flags, command) = if let Some(flags_end) = options.find(char::is_whitespace) {
        options.split_at(flags_end)
    } else {
        (options, "")
    };
    if !flags
        .chars()
        .all(|flag| matches!(flag, 'c' | 'f' | 'm' | 'q' | 'x'))
    {
        return Ok(ParsedUserShellCommand {
            command: input,
            response_handling: ThreadShellCommandResponseHandling::default(),
            response_option_prefix_len: None,
        });
    }
    if flags.is_empty() {
        return Err(
            "invalid empty shell wake/event state; use unique f, q, or x flags in fqx order"
                .to_string(),
        );
    }

    let mut final_wake = false;
    let mut queue_command = false;
    let mut presentation_only = false;
    let mut previous_position = None;
    for flag in flags.chars() {
        let position = match flag {
            'f' if !final_wake => {
                final_wake = true;
                0
            }
            'q' if !queue_command => {
                queue_command = true;
                1
            }
            'x' if !presentation_only => {
                presentation_only = true;
                2
            }
            _ => {
                return Err(format!(
                    "invalid shell wake/event state `{flags}`; use unique f, q, or x flags in fqx order"
                ));
            }
        };
        if previous_position.is_some_and(|previous| position <= previous) {
            return Err(format!(
                "invalid shell wake/event state `{flags}`; use unique f, q, or x flags in fqx order"
            ));
        }
        previous_position = Some(position);
    }

    let final_delivery = match (final_wake, presentation_only) {
        (true, false) => ThreadShellCommandFinalDelivery::Wake,
        (false, true) => ThreadShellCommandFinalDelivery::PresentationOnly,
        (false, false) | (true, true) => ThreadShellCommandFinalDelivery::Passive,
    };

    Ok(ParsedUserShellCommand {
        command: command.trim_start(),
        response_handling: ThreadShellCommandResponseHandling {
            final_delivery,
            queue_command,
        },
        response_option_prefix_len: Some("w:".len() + flags.len()),
    })
}

pub(crate) fn user_shell_response_handling_label(
    response_handling: ThreadShellCommandResponseHandling,
) -> String {
    let delivery = match response_handling.final_delivery {
        ThreadShellCommandFinalDelivery::Passive => "passive",
        ThreadShellCommandFinalDelivery::Wake => "wake",
        ThreadShellCommandFinalDelivery::PresentationOnly => "presentation only",
    };
    if response_handling.queue_command {
        format!("{delivery} · queued")
    } else {
        delivery.to_string()
    }
}

#[cfg(test)]
#[path = "user_shell_command_tests.rs"]
mod tests;
