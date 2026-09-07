use codex_app_server_protocol::ThreadShellCommandFinalDelivery;
use codex_app_server_protocol::ThreadShellCommandResponseHandling;
use codex_protocol::WakeEventFinalDelivery;
use codex_protocol::WakeEventFlags;
use codex_protocol::WakeEventSurface;

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
    let parsed = WakeEventFlags::parse(flags, WakeEventSurface::UserShell)
        .map_err(|error| format!("invalid shell wake/event state `{flags}`; {error}"))?;
    let final_delivery = match parsed.final_delivery {
        WakeEventFinalDelivery::Wake => ThreadShellCommandFinalDelivery::Wake,
        WakeEventFinalDelivery::PresentationOnly => {
            ThreadShellCommandFinalDelivery::PresentationOnly
        }
        WakeEventFinalDelivery::Passive => ThreadShellCommandFinalDelivery::Passive,
    };

    Ok(ParsedUserShellCommand {
        command: command.trim_start(),
        response_handling: ThreadShellCommandResponseHandling {
            final_delivery,
            queue_command: parsed.queue_input,
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
