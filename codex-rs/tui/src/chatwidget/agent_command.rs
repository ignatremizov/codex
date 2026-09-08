//! Parsing for the user-facing `/agent` control grammar.

use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::AgentForkMode;
use codex_app_server_protocol::AgentObservationMode;
use codex_app_server_protocol::AgentReplyRouteMode;
use codex_app_server_protocol::AgentResponseHandling;
use codex_protocol::MAIN_AGENT_NICKNAME;
use codex_protocol::ThreadId;
use codex_protocol::WakeEventFinalDelivery;
use codex_protocol::WakeEventFlags;
use codex_protocol::WakeEventSurface;
use codex_protocol::openai_models::ReasoningEffort;

pub(super) const AGENT_COMMAND_USAGE: &str =
    "Usage: /agent [new|<target>|<role>|queue|interrupt|close|resume|observe|sends] ...";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AgentCommand<'a> {
    OpenPane,
    New {
        task: Option<String>,
        fork: Option<AgentForkMode>,
        response: Option<AgentResponseHandling>,
        model: Option<String>,
        reasoning_effort: Option<ReasoningEffort>,
        prompt: Option<AgentCommandPrompt<'a>>,
    },
    SelectOrDispatch {
        selector: AgentSelector,
        task: Option<String>,
        fork: Option<AgentForkMode>,
        response: Option<AgentResponseHandling>,
        model: Option<String>,
        reasoning_effort: Option<ReasoningEffort>,
        prompt: Option<AgentCommandPrompt<'a>>,
    },
    Queue {
        selector: AgentSelector,
        response: Option<AgentResponseHandling>,
        prompt: Option<AgentCommandPrompt<'a>>,
    },
    Interrupt {
        selector: AgentSelector,
        response: Option<AgentResponseHandling>,
        prompt: Option<AgentCommandPrompt<'a>>,
    },
    Close {
        selector: AgentSelector,
        response: Option<AgentResponseHandling>,
    },
    Resume {
        selector: AgentSelector,
        task: Option<String>,
        response: Option<AgentResponseHandling>,
        prompt: Option<AgentCommandPrompt<'a>>,
    },
    Observe {
        selector: AgentSelector,
        observer: Option<AgentSelector>,
        mode: AgentObservationMode,
    },
    ReplyRoute {
        selector: AgentSelector,
        recipient: Option<AgentSelector>,
        mode: AgentReplyRouteMode,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentSelector {
    pub(crate) kind: AgentSelectorKind,
    pub(crate) authored: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentSelectorKind {
    Id(ThreadId),
    Ref(u64),
    Nickname(String),
    Task(String),
    Role(String),
    UnprefixedName(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AgentCommandPrompt<'a> {
    pub(super) text: &'a str,
    /// Byte offset of `text` within the slash command's complete argument string.
    pub(super) offset: usize,
}

#[derive(Debug)]
struct ControlToken {
    value: String,
    raw: String,
    start: usize,
}

#[derive(Default)]
struct ParsedOptions {
    task: Option<String>,
    fork: Option<AgentForkMode>,
    response: Option<AgentResponseHandling>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentCommandOptionScope {
    Spawn,
    Resume,
    Existing,
}

impl AgentCommandOptionScope {
    fn allows_spawn_options(self) -> bool {
        self == Self::Spawn
    }
}

struct AgentCommandParser<'a> {
    input: &'a str,
    cursor: usize,
}

#[cfg(test)]
pub(super) fn parse_agent_command(args: &str) -> Result<AgentCommand<'_>, String> {
    parse_agent_command_with_attached_input(args, /*has_attached_input*/ false)
}

pub(super) fn parse_agent_command_with_attached_input(
    args: &str,
    has_attached_input: bool,
) -> Result<AgentCommand<'_>, String> {
    let mut parser = AgentCommandParser {
        input: args,
        cursor: 0,
    };
    let Some(first) = parser.next_token()? else {
        return Ok(AgentCommand::OpenPane);
    };

    match first.value.as_str() {
        "new" => {
            let (options, prompt) = parser.options_and_prompt(AgentCommandOptionScope::Spawn)?;
            Ok(AgentCommand::New {
                task: options.task,
                fork: options.fork,
                response: options.response,
                model: options.model,
                reasoning_effort: options.reasoning_effort,
                prompt,
            })
        }
        "queue" => {
            let selector = parser.required_selector("queue")?;
            let (options, prompt) = parser.options_and_prompt(AgentCommandOptionScope::Existing)?;
            if options.response.is_some() && prompt.is_none() && !has_attached_input {
                return Err("`w` requires a queued prompt.".to_string());
            }
            Ok(AgentCommand::Queue {
                selector,
                response: options.response,
                prompt,
            })
        }
        "interrupt" => {
            let selector = parser.required_selector("interrupt")?;
            let (options, prompt) = parser.options_and_prompt(AgentCommandOptionScope::Existing)?;
            if options.response.is_some() && prompt.is_none() && !has_attached_input {
                return Err("`w` requires a follow-up prompt after `interrupt`.".to_string());
            }
            Ok(AgentCommand::Interrupt {
                selector,
                response: options.response,
                prompt,
            })
        }
        "close" => {
            let selector = parser.required_selector("close")?;
            parser.close_command(selector)
        }
        "resume" => {
            let selector = parser.required_selector("resume")?;
            parser.resume_command(selector)
        }
        "observe" => {
            let selector = parser.required_selector("observe")?;
            let token = parser.next_token()?.ok_or_else(|| {
                "Usage: /agent observe <target> [from <observer>] <passive|wake|presentation>"
                    .to_string()
            })?;
            let (observer, mode) = if token.raw == "from" {
                let observer = parser.required_selector("observe <target> from")?;
                let mode = parser.next_token()?.ok_or_else(|| {
                    "Expected passive, wake, or presentation after observer.".to_string()
                })?;
                (Some(observer), parse_observe_mode(&mode.value)?)
            } else {
                (None, parse_observe_mode(&token.value)?)
            };
            parser.require_end("observe")?;
            Ok(AgentCommand::Observe {
                selector,
                observer,
                mode,
            })
        }
        "sends" => {
            let selector = parser.required_selector("sends")?;
            parser.reply_route_command(selector)
        }
        _ => {
            let selector = parse_selector(&first.value, first.raw.clone())?;
            let target_first_action_start = parser.cursor;
            if let Some(action) = parser.next_token()?
                && action.raw == "close"
            {
                return parser.close_command(selector);
            }
            parser.cursor = target_first_action_start;
            if let Some(action) = parser.next_token()?
                && action.raw == "sends"
            {
                return parser.reply_route_command(selector);
            }
            parser.cursor = target_first_action_start;
            if let Some(action) = parser.next_token()?
                && action.raw == "resume"
            {
                return parser.resume_command(selector);
            }
            parser.cursor = target_first_action_start;
            let (options, prompt) = parser.options_and_prompt(AgentCommandOptionScope::Spawn)?;
            let known_target = matches!(
                selector.kind(),
                AgentSelectorKind::Id(_)
                    | AgentSelectorKind::Ref(_)
                    | AgentSelectorKind::Nickname(_)
                    | AgentSelectorKind::Task(_)
            );
            if known_target && options.task.is_some() {
                return Err("`task` is valid only when spawning an agent.".to_string());
            }
            if known_target && options.fork.is_some() {
                return Err("`fork` is valid only when spawning an agent.".to_string());
            }
            if known_target && (options.model.is_some() || options.reasoning_effort.is_some()) {
                return Err(
                    "`model` and `effort` are valid only when spawning an agent.".to_string(),
                );
            }
            if known_target && options.response.is_some() && prompt.is_none() && !has_attached_input
            {
                return Err("`w` requires a prompt for an existing target.".to_string());
            }
            Ok(AgentCommand::SelectOrDispatch {
                selector,
                task: options.task,
                fork: options.fork,
                response: options.response,
                model: options.model,
                reasoning_effort: options.reasoning_effort,
                prompt,
            })
        }
    }
}

impl<'a> AgentCommandParser<'a> {
    fn resume_command(&mut self, selector: AgentSelector) -> Result<AgentCommand<'a>, String> {
        let (options, prompt) = self.options_and_prompt(AgentCommandOptionScope::Resume)?;
        Ok(AgentCommand::Resume {
            selector,
            task: options.task,
            response: options.response,
            prompt,
        })
    }

    fn close_command(&mut self, selector: AgentSelector) -> Result<AgentCommand<'a>, String> {
        let (options, prompt) = self.options_and_prompt(AgentCommandOptionScope::Existing)?;
        if prompt.is_some() {
            return Err("`close` accepts response handling but not a prompt.".to_string());
        }
        Ok(AgentCommand::Close {
            selector,
            response: options.response,
        })
    }

    fn reply_route_command(&mut self, selector: AgentSelector) -> Result<AgentCommand<'a>, String> {
        let token = self.next_token()?.ok_or_else(|| {
            "Usage: /agent sends <sender> [to <recipient>] <enable|disable>".to_string()
        })?;
        let (recipient, mode) = if token.value == "to" {
            if selector.authored() == "all" {
                return Err("`sends all` sets the subtree default; omit `to`.".into());
            }
            let recipient = self.required_selector("sends <sender> to")?;
            let mode = self
                .next_token()?
                .ok_or_else(|| "Expected enable or disable after reply recipient.".to_string())?;
            (Some(recipient), parse_reply_route_mode(&mode.value)?)
        } else {
            (None, parse_reply_route_mode(&token.value)?)
        };
        self.require_end("sends")?;
        Ok(AgentCommand::ReplyRoute {
            selector,
            recipient,
            mode,
        })
    }

    fn required_selector(&mut self, action: &str) -> Result<AgentSelector, String> {
        let token = self
            .next_token()?
            .ok_or_else(|| format!("Usage: /agent {action} <target> ..."))?;
        parse_selector(&token.value, token.raw)
    }

    fn options_and_prompt(
        &mut self,
        scope: AgentCommandOptionScope,
    ) -> Result<(ParsedOptions, Option<AgentCommandPrompt<'a>>), String> {
        let mut options = ParsedOptions::default();
        loop {
            let Some(token) = self.next_token()? else {
                return Ok((options, None));
            };
            if token.value == "--" {
                let prompt_start = self.skip_whitespace();
                let prompt = (prompt_start < self.input.len()).then_some(AgentCommandPrompt {
                    text: &self.input[prompt_start..],
                    offset: prompt_start,
                });
                return Ok((options, prompt));
            }
            if let Some(value) = token.value.strip_prefix("task:") {
                if scope == AgentCommandOptionScope::Existing {
                    return Err(
                        "`task` is valid only when spawning or adopting an agent.".to_string()
                    );
                }
                if options.task.is_some() {
                    return Err("`task` may be specified only once.".to_string());
                }
                options.task = Some(nonempty_selector(value, "task path")?);
                continue;
            }
            if token.value.strip_prefix("fork:").is_some() && !scope.allows_spawn_options() {
                return Err("`fork` is valid only when spawning an agent.".to_string());
            }
            if let Some(value) = token.value.strip_prefix("fork:") {
                if options.fork.is_some() {
                    return Err("`fork` may be specified only once.".to_string());
                }
                options.fork = Some(parse_fork_mode(value)?);
                continue;
            }
            if token.value.strip_prefix("w:").is_some() && options.response.is_some() {
                return Err("`w` may be specified only once.".to_string());
            }
            if let Some(value) = token.value.strip_prefix("w:") {
                options.response = Some(parse_response_mode(value)?);
                continue;
            }
            if token.value.strip_prefix("model:").is_some() && !scope.allows_spawn_options() {
                return Err("`model` is valid only when spawning an agent.".to_string());
            }
            if let Some(value) = token.value.strip_prefix("model:") {
                if options.model.is_some() {
                    return Err("`model` may be specified only once.".to_string());
                }
                if value.is_empty() {
                    return Err("`model` requires a nonempty model slug.".to_string());
                }
                options.model = Some(value.to_string());
                continue;
            }
            if token.value.strip_prefix("effort:").is_some() && !scope.allows_spawn_options() {
                return Err("`effort` is valid only when spawning an agent.".to_string());
            }
            if let Some(value) = token.value.strip_prefix("effort:") {
                if options.reasoning_effort.is_some() {
                    return Err("`effort` may be specified only once.".to_string());
                }
                options.reasoning_effort = Some(
                    value
                        .parse()
                        .map_err(|error| format!("Invalid reasoning effort `{value}`: {error}"))?,
                );
                continue;
            }
            return Ok((
                options,
                Some(AgentCommandPrompt {
                    text: &self.input[token.start..],
                    offset: token.start,
                }),
            ));
        }
    }

    fn require_end(&mut self, action: &str) -> Result<(), String> {
        if self.next_token()?.is_some() {
            return Err(format!("`{action}` does not accept additional arguments."));
        }
        Ok(())
    }

    fn next_token(&mut self) -> Result<Option<ControlToken>, String> {
        self.skip_whitespace();
        if self.cursor == self.input.len() {
            return Ok(None);
        }

        let start = self.cursor;
        let mut value = String::new();
        let mut quoted = false;
        while self.cursor < self.input.len() {
            let Some(ch) = self.input[self.cursor..].chars().next() else {
                break;
            };
            if quoted {
                match ch {
                    '"' => {
                        quoted = false;
                        self.cursor += ch.len_utf8();
                    }
                    '\\' => {
                        self.cursor += ch.len_utf8();
                        let escaped = self.input[self.cursor..]
                            .chars()
                            .next()
                            .ok_or_else(|| "Unterminated escape in quoted selector.".to_string())?;
                        if !matches!(escaped, '"' | '\\') {
                            return Err(format!(
                                "Unsupported escape `\\{escaped}` in quoted selector."
                            ));
                        }
                        value.push(escaped);
                        self.cursor += escaped.len_utf8();
                    }
                    _ => {
                        value.push(ch);
                        self.cursor += ch.len_utf8();
                    }
                }
            } else if ch == '"' {
                quoted = true;
                self.cursor += ch.len_utf8();
            } else if ch.is_whitespace() {
                break;
            } else {
                value.push(ch);
                self.cursor += ch.len_utf8();
            }
        }
        if quoted {
            return Err("Unterminated double quote in `/agent` selector.".to_string());
        }
        if value.is_empty() {
            return Err("Agent selectors cannot be empty.".to_string());
        }
        let raw = self.input[start..self.cursor].to_string();
        Ok(Some(ControlToken { value, raw, start }))
    }

    fn skip_whitespace(&mut self) -> usize {
        while self.cursor < self.input.len() {
            let Some(ch) = self.input[self.cursor..].chars().next() else {
                break;
            };
            if !ch.is_whitespace() {
                break;
            }
            self.cursor += ch.len_utf8();
        }
        self.cursor
    }
}

fn parse_selector(value: &str, authored: String) -> Result<AgentSelector, String> {
    let kind = parse_selector_kind(value)?;
    Ok(AgentSelector { kind, authored })
}

fn parse_selector_kind(value: &str) -> Result<AgentSelectorKind, String> {
    if let Some(value) = value.strip_prefix("task:") {
        return nonempty_selector(value, "task path").map(AgentSelectorKind::Task);
    }
    if value == "/root" || value.starts_with("/root/") {
        return Ok(AgentSelectorKind::Task(value.to_string()));
    }
    if let Some(value) = value.strip_prefix("id:") {
        return parse_thread_id(value).map(AgentSelectorKind::Id);
    }
    if let Some(value) = value.strip_prefix("ref:") {
        return parse_agent_ref(value).map(AgentSelectorKind::Ref);
    }
    if let Some(value) = value.strip_prefix("nick:") {
        let nickname = nonempty_selector(value, "nickname")?;
        let nickname = if nickname.eq_ignore_ascii_case(MAIN_AGENT_NICKNAME) {
            MAIN_AGENT_NICKNAME.to_string()
        } else {
            nickname
        };
        return Ok(AgentSelectorKind::Nickname(nickname));
    }
    if let Some(value) = value.strip_prefix("role:") {
        return nonempty_selector(value, "role").map(AgentSelectorKind::Role);
    }
    if let Ok(thread_id) = ThreadId::from_string(value) {
        return Ok(AgentSelectorKind::Id(thread_id));
    }
    if value.chars().all(|ch| ch.is_ascii_digit()) {
        return parse_agent_ref(value).map(AgentSelectorKind::Ref);
    }
    if value.eq_ignore_ascii_case(MAIN_AGENT_NICKNAME) {
        return Ok(AgentSelectorKind::Nickname(MAIN_AGENT_NICKNAME.to_string()));
    }
    nonempty_selector(value, "agent").map(AgentSelectorKind::UnprefixedName)
}

impl AgentSelector {
    pub(crate) fn kind(&self) -> &AgentSelectorKind {
        &self.kind
    }

    pub(crate) fn authored(&self) -> &str {
        &self.authored
    }

    pub(crate) fn control_target(&self) -> Result<String, String> {
        match &self.kind {
            AgentSelectorKind::Id(thread_id) => Ok(thread_id.to_string()),
            AgentSelectorKind::Ref(agent_ref) => Ok(agent_ref.to_string()),
            AgentSelectorKind::Nickname(nickname) => Ok(format!("nick:{nickname}")),
            AgentSelectorKind::Task(task) => Ok(format!("task:{task}")),
            AgentSelectorKind::UnprefixedName(name) => Ok(name.clone()),
            AgentSelectorKind::Role(role) => Err(format!(
                "{role:?} selects a configured role, not an existing agent"
            )),
        }
    }
}

fn parse_thread_id(value: &str) -> Result<ThreadId, String> {
    ThreadId::from_string(value).map_err(|_| format!("Invalid agent UUID `{value}`."))
}

fn parse_agent_ref(value: &str) -> Result<u64, String> {
    let value = value
        .parse::<u64>()
        .map_err(|_| format!("Invalid agent ref `{value}`."))?;
    if value == 0 {
        return Err("Agent refs start at 1.".to_string());
    }
    Ok(value)
}

fn nonempty_selector(value: &str, kind: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("Agent {kind} cannot be empty."));
    }
    Ok(value.to_string())
}

fn parse_fork_mode(value: &str) -> Result<AgentForkMode, String> {
    match value {
        "none" => Ok(AgentForkMode::None),
        "all" => Ok(AgentForkMode::All),
        _ => {
            let turns = value.parse::<u32>().map_err(|_| {
                format!("Invalid fork mode `{value}`; use none, all, or a positive number.")
            })?;
            if turns == 0 {
                return Err("`fork:0` is invalid; use `fork:none` for no history.".to_string());
            }
            Ok(AgentForkMode::LastNTurns { turns })
        }
    }
}

fn parse_response_mode(value: &str) -> Result<AgentResponseHandling, String> {
    let flags = WakeEventFlags::parse(value, WakeEventSurface::Agent)
        .map_err(|error| format!("Invalid response mode `{value}`; {error}."))?;
    let final_response = match flags.final_delivery {
        WakeEventFinalDelivery::Wake => AgentFinalResponseHandling::Wake,
        WakeEventFinalDelivery::PresentationOnly => AgentFinalResponseHandling::Presentation,
        WakeEventFinalDelivery::Passive => AgentFinalResponseHandling::Passive,
    };
    Ok(AgentResponseHandling::new(
        flags.commentary,
        final_response,
        flags.target_messages,
        flags.queue_input,
    ))
}

fn parse_observe_mode(value: &str) -> Result<AgentObservationMode, String> {
    match value {
        "passive" => Ok(AgentObservationMode::Passive),
        "wake" => Ok(AgentObservationMode::Wake),
        "presentation" => Ok(AgentObservationMode::Presentation),
        _ => Err(format!(
            "Invalid observation mode `{value}`; use passive, wake, or presentation."
        )),
    }
}

fn parse_reply_route_mode(value: &str) -> Result<AgentReplyRouteMode, String> {
    match value {
        "enable" => Ok(AgentReplyRouteMode::Enabled),
        "disable" => Ok(AgentReplyRouteMode::Disabled),
        _ => Err(format!(
            "Invalid reply-route mode `{value}`; use enable or disable."
        )),
    }
}

#[cfg(test)]
#[path = "agent_command_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "agent_task_command_tests.rs"]
mod task_tests;
