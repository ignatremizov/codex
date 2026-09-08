use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::WidgetRef;

use super::picker_style::selection_style;
use super::popup_consts::MAX_POPUP_ROWS;
use super::scroll_state::ScrollState;
use super::selection_popup_common::ColumnWidthConfig;
use super::selection_popup_common::ColumnWidthMode;
use super::selection_popup_common::GenericDisplayRow;
use super::selection_popup_common::measure_rows_height_with_col_width_mode;
use super::selection_popup_common::render_rows_with_col_width_mode;
use super::slash_commands::BuiltinCommandFlags;
use super::slash_commands::ServiceTierCommand;
use super::slash_commands::SlashCommandItem;
use super::slash_commands::commands_for_input;
use crate::slash_command::SlashCommand;

// Hide alias commands in the default popup list so each unique action appears once.
// `quit` is an alias of `exit`, and `btw` is an alias of `side`, so we skip
// those aliases here.
const ALIAS_COMMANDS: &[SlashCommand] = &[SlashCommand::Quit, SlashCommand::Btw];
const COMMAND_COLUMN_WIDTH: ColumnWidthConfig = ColumnWidthConfig::new(
    ColumnWidthMode::AutoAllRows,
    /*name_column_width*/ None,
);

/// A selectable item in the popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommandItem {
    Builtin(SlashCommand),
    ServiceTier(ServiceTierCommand),
    Mcp(super::mcp_completion::McpCompletion),
    BackgroundTerminal(BackgroundTerminalCompletion),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundTerminalCompletion {
    pub(crate) process_id: String,
    pub(crate) command_display: String,
}

pub(crate) struct CommandPopup {
    command_filter: String,
    commands: Vec<CommandItem>,
    composer_text: String,
    background_terminals: Vec<BackgroundTerminalCompletion>,
    state: ScrollState,
    mcp_server_names: Vec<String>,
    mcp_candidates: Option<Vec<super::mcp_completion::McpCompletion>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CommandPopupFlags {
    pub(crate) collaboration_modes_enabled: bool,
    pub(crate) connectors_enabled: bool,
    pub(crate) plugins_command_enabled: bool,
    pub(crate) token_activity_command_enabled: bool,
    pub(crate) service_tier_commands_enabled: bool,
    pub(crate) goal_command_enabled: bool,
    pub(crate) voice_command_enabled: bool,
    pub(crate) worktrees_enabled: bool,
    pub(crate) windows_degraded_sandbox_active: bool,
    pub(crate) side_conversation_active: bool,
}

impl From<CommandPopupFlags> for BuiltinCommandFlags {
    fn from(value: CommandPopupFlags) -> Self {
        Self {
            collaboration_modes_enabled: value.collaboration_modes_enabled,
            connectors_enabled: value.connectors_enabled,
            plugins_command_enabled: value.plugins_command_enabled,
            token_activity_command_enabled: value.token_activity_command_enabled,
            service_tier_commands_enabled: value.service_tier_commands_enabled,
            goal_command_enabled: value.goal_command_enabled,
            voice_command_enabled: value.voice_command_enabled,
            worktrees_enabled: value.worktrees_enabled,
            allow_elevate_sandbox: value.windows_degraded_sandbox_active,
            side_conversation_active: value.side_conversation_active,
        }
    }
}

impl CommandPopup {
    pub(crate) fn set_mcp_server_names(&mut self, names: Vec<String>) {
        self.mcp_server_names = names;
    }

    pub(crate) fn new(
        flags: CommandPopupFlags,
        service_tier_commands: Vec<ServiceTierCommand>,
    ) -> Self {
        // Keep built-in availability in sync with the composer.
        let commands = commands_for_input(flags.into(), &service_tier_commands)
            .into_iter()
            .filter_map(|command| match command {
                SlashCommandItem::Builtin(cmd) => (!cmd.command().starts_with("debug")
                    && cmd != SlashCommand::Apps)
                    .then_some(CommandItem::Builtin(cmd)),
                SlashCommandItem::ServiceTier(command) => Some(CommandItem::ServiceTier(command)),
            })
            .collect();
        Self {
            command_filter: String::new(),
            commands,
            composer_text: String::new(),
            background_terminals: Vec::new(),
            state: ScrollState::new(),
            mcp_server_names: Vec::new(),
            mcp_candidates: None,
        }
    }

    pub(crate) fn set_background_terminals(
        &mut self,
        background_terminals: Vec<BackgroundTerminalCompletion>,
    ) {
        if self.background_terminals == background_terminals {
            return;
        }
        self.background_terminals = background_terminals;
        self.state.reset();
        let matches_len = self.filtered_items().len();
        self.state.clamp_selection(matches_len);
        self.state
            .ensure_visible(matches_len, MAX_POPUP_ROWS.min(matches_len));
    }
    /// Update the filter string based on the current composer text. The text
    /// passed in is expected to start with a leading '/'. Everything after the
    /// *first* '/' on the *first* line becomes the active filter that is used
    /// to narrow down the list of available commands.
    pub(crate) fn on_composer_text_change(&mut self, text: String) {
        self.composer_text = text.clone();
        let first_line = text.lines().next().unwrap_or("");
        let previous_filter = self.command_filter.clone();
        let previous_candidates = self.mcp_candidates.clone();
        self.mcp_candidates = super::mcp_completion::candidates(first_line, &self.mcp_server_names);

        if let Some(stripped) = first_line.strip_prefix('/') {
            // Extract the *first* token (sequence of non-whitespace
            // characters) after the slash so that `/clear something` still
            // shows the help for `/clear`.
            let token = stripped.trim_start();
            let cmd_token = token.split_whitespace().next().unwrap_or("");

            // Update the filter keeping the original case (commands are all
            // lower-case for now but this may change in the future).
            self.command_filter = cmd_token.to_string();
        } else {
            // The composer no longer starts with '/'. Reset the filter so the
            // popup shows the *full* command list if it is still displayed
            // for some reason.
            self.command_filter.clear();
        }

        if self.command_filter != previous_filter || self.mcp_candidates != previous_candidates {
            self.state.reset();
        }

        // Reset or clamp selected index based on new filtered list.
        let matches_len = self.filtered_items().len();
        self.state.clamp_selection(matches_len);
        self.state
            .ensure_visible(matches_len, MAX_POPUP_ROWS.min(matches_len));
    }

    /// Determine the preferred height of the popup for a given width.
    /// Accounts for wrapped descriptions so that long tooltips don't overflow.
    pub(crate) fn calculate_required_height(&self, width: u16) -> u16 {
        let rows = self.rows_from_matches(self.filtered());

        measure_rows_height_with_col_width_mode(
            &rows,
            &self.state,
            MAX_POPUP_ROWS,
            width,
            COMMAND_COLUMN_WIDTH,
        )
    }

    /// Compute exact/prefix matches over built-in commands and user prompts,
    /// paired with optional highlight indices. Preserves the original
    /// presentation order for built-ins and prompts.
    fn filtered(&self) -> Vec<(CommandItem, Option<Vec<usize>>)> {
        if let Some(candidates) = &self.mcp_candidates {
            return candidates
                .iter()
                .cloned()
                .map(|item| (CommandItem::Mcp(item), None))
                .collect();
        }
        if let Some(matches) = self.filtered_stop_args() {
            return matches;
        }
        let filter = self.command_filter.trim();
        let mut out: Vec<(CommandItem, Option<Vec<usize>>)> = Vec::new();
        if filter.is_empty() {
            for command in self.commands.iter() {
                if matches!(command, CommandItem::Builtin(cmd) if ALIAS_COMMANDS.contains(cmd)) {
                    continue;
                }
                out.push((command.clone(), None));
            }
            return out;
        }

        let filter_lower = filter.to_lowercase();
        let filter_chars = filter.chars().count();
        let mut exact: Vec<(CommandItem, Option<Vec<usize>>)> = Vec::new();
        let mut prefix: Vec<(CommandItem, Option<Vec<usize>>)> = Vec::new();
        let indices_for = |offset| Some((offset..offset + filter_chars).collect());

        let mut push_match =
            |item: CommandItem, display: &str, name: Option<&str>, name_offset: usize| {
                let display_lower = display.to_lowercase();
                let name_lower = name.map(str::to_lowercase);
                let display_exact = display_lower == filter_lower;
                let name_exact = name_lower.as_deref() == Some(filter_lower.as_str());
                if display_exact || name_exact {
                    let offset = if display_exact { 0 } else { name_offset };
                    exact.push((item, indices_for(offset)));
                    return;
                }
                let display_prefix = display_lower.starts_with(&filter_lower);
                let name_prefix = name_lower
                    .as_ref()
                    .is_some_and(|name| name.starts_with(&filter_lower));
                if display_prefix || name_prefix {
                    let offset = if display_prefix { 0 } else { name_offset };
                    prefix.push((item, indices_for(offset)));
                }
            };

        for command in self.commands.iter() {
            let display = command.command();
            push_match(command.clone(), &display, None, 0);
        }

        out.extend(exact);
        out.extend(prefix);
        out
    }

    fn filtered_stop_args(&self) -> Option<Vec<(CommandItem, Option<Vec<usize>>)>> {
        let tail = self.composer_text.strip_prefix("/stop")?;
        if !tail.is_empty() && !tail.starts_with(char::is_whitespace) {
            return None;
        }

        let query = tail.trim_start();
        if query.contains(char::is_whitespace) {
            return Some(Vec::new());
        }
        let matches = self
            .background_terminals
            .iter()
            .filter(|terminal| terminal.process_id.starts_with(query))
            .map(|terminal| {
                let indices = (!query.is_empty()).then(|| (0..query.chars().count()).collect());
                (CommandItem::BackgroundTerminal(terminal.clone()), indices)
            })
            .collect();
        Some(matches)
    }

    fn filtered_items(&self) -> Vec<CommandItem> {
        self.filtered().into_iter().map(|(c, _)| c).collect()
    }

    fn rows_from_matches(
        &self,
        matches: Vec<(CommandItem, Option<Vec<usize>>)>,
    ) -> Vec<GenericDisplayRow> {
        matches
            .into_iter()
            .enumerate()
            .map(|(index, (item, indices))| {
                let (name, description) = match &item {
                    CommandItem::Builtin(cmd) => {
                        (format!("/{}", cmd.command()), cmd.description().to_string())
                    }
                    CommandItem::ServiceTier(command) => {
                        (format!("/{}", command.name), command.description.clone())
                    }
                    CommandItem::Mcp(completion) => (
                        completion
                            .text()
                            .trim_start_matches('/')
                            .trim_end()
                            .to_string(),
                        completion.description().to_string(),
                    ),
                    CommandItem::BackgroundTerminal(terminal) => (
                        terminal.process_id.clone(),
                        terminal.command_display.clone(),
                    ),
                };
                GenericDisplayRow {
                    category_tag: None,
                    name,
                    name_style: Default::default(),
                    name_prefix_spans: vec![if self.state.selected_idx == Some(index) {
                        "› ".into()
                    } else {
                        "  ".into()
                    }],
                    selection_style: Some(selection_style()),
                    match_indices: indices.map(|v| v.into_iter().map(|i| i + 1).collect()),
                    display_shortcut: None,
                    description: Some(description),
                    wrap_indent: None,
                    is_disabled: false,
                    disabled_reason: None,
                }
            })
            .collect()
    }

    /// Move the selection cursor one step up.
    pub(crate) fn move_up(&mut self) {
        let len = self.filtered_items().len();
        self.state.move_up_wrap(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    /// Move the selection cursor one step down.
    pub(crate) fn move_down(&mut self) {
        let matches_len = self.filtered_items().len();
        self.state.move_down_wrap(matches_len);
        self.state
            .ensure_visible(matches_len, MAX_POPUP_ROWS.min(matches_len));
    }

    /// Return currently selected command, if any.
    pub(crate) fn selected_item(&self) -> Option<CommandItem> {
        let matches = self.filtered_items();
        self.state
            .selected_idx
            .and_then(|idx| matches.get(idx).cloned())
    }
}

impl CommandItem {
    pub(crate) fn command(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Builtin(cmd) => cmd.command().into(),
            Self::ServiceTier(command) => command.name.as_str().into(),
            Self::Mcp(completion) => completion
                .text()
                .trim_start_matches('/')
                .trim_end()
                .to_string()
                .into(),
            Self::BackgroundTerminal(terminal) => terminal.process_id.as_str().into(),
        }
    }
}

impl WidgetRef for CommandPopup {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let rows = self.rows_from_matches(self.filtered());
        render_rows_with_col_width_mode(
            area,
            buf,
            &rows,
            &self.state,
            MAX_POPUP_ROWS,
            "no matches",
            COMMAND_COLUMN_WIDTH,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn filter_includes_init_when_typing_prefix() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        // Simulate the composer line starting with '/in' so the popup filters
        // matching commands by prefix.
        popup.on_composer_text_change("/in".to_string());

        // Access the filtered list via the selected command and ensure that
        // one of the matches is the new "init" command.
        let matches = popup.filtered_items();
        let has_init = matches.iter().any(|item| match item {
            CommandItem::Builtin(cmd) => cmd.command() == "init",
            CommandItem::ServiceTier(_) | CommandItem::Mcp(_) => false,
            CommandItem::BackgroundTerminal(_) => false,
        });
        assert!(
            has_init,
            "expected '/init' to appear among filtered commands"
        );
    }

    #[test]
    fn selecting_init_by_exact_match() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/init".to_string());

        // When an exact match exists, the selected command should be that
        // command by default.
        let selected = popup.selected_item();
        match selected {
            Some(CommandItem::Builtin(cmd)) => assert_eq!(cmd.command(), "init"),
            Some(CommandItem::ServiceTier(command)) => {
                panic!("expected init command, got service tier {command:?}")
            }
            None => panic!("expected a selected command for exact match"),
            Some(CommandItem::Mcp(command)) => panic!("unexpected MCP completion {command:?}"),
            Some(CommandItem::BackgroundTerminal(terminal)) => {
                panic!("unexpected background terminal {terminal:?}")
            }
        }
    }

    #[test]
    fn model_is_first_suggestion_for_mo() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/mo".to_string());
        let matches = popup.filtered_items();
        match matches.first() {
            Some(CommandItem::Builtin(cmd)) => assert_eq!(cmd.command(), "model"),
            Some(CommandItem::ServiceTier(command)) => {
                panic!("expected model command, got service tier {command:?}")
            }
            None => panic!("expected at least one match for '/mo'"),
            Some(CommandItem::Mcp(command)) => panic!("unexpected MCP completion {command:?}"),
            Some(CommandItem::BackgroundTerminal(terminal)) => {
                panic!("unexpected background terminal {terminal:?}")
            }
        }
    }

    #[test]
    fn service_tier_command_uses_catalog_name_and_description() {
        let mut popup = CommandPopup::new(
            CommandPopupFlags {
                service_tier_commands_enabled: true,
                ..CommandPopupFlags::default()
            },
            vec![ServiceTierCommand {
                id: "priority".to_string(),
                name: "fast".to_string(),
                description: "Fastest inference with increased plan usage".to_string(),
            }],
        );
        popup.on_composer_text_change("/fa".to_string());

        match popup.selected_item() {
            Some(CommandItem::ServiceTier(command)) => assert_eq!(
                command,
                ServiceTierCommand {
                    id: "priority".to_string(),
                    name: "fast".to_string(),
                    description: "Fastest inference with increased plan usage".to_string(),
                }
            ),
            other => panic!("expected fast service tier to be selected, got {other:?}"),
        }
        let rows = popup.rows_from_matches(popup.filtered());
        assert_eq!(
            rows.first().and_then(|row| row.description.as_deref()),
            Some("Fastest inference with increased plan usage")
        );
    }

    #[test]
    fn command_popup_wrap_boundary_preserves_following_choice() {
        let mut popup = CommandPopup::new(
            CommandPopupFlags {
                service_tier_commands_enabled: true,
                ..CommandPopupFlags::default()
            },
            vec![
                ServiceTierCommand {
                    id: "priority".to_string(),
                    name: "tier-one".to_string(),
                    description: "Use faster inference".to_string(),
                },
                ServiceTierCommand {
                    id: "default".to_string(),
                    name: "tier-two".to_string(),
                    description: "Keep default speed".to_string(),
                },
            ],
        );
        popup.on_composer_text_change("/tier".to_string());

        // The first description exactly fits at width 33, including the left inset.
        // Rendering at the requested height must also retain the second choice
        // when the first description wraps one column below that boundary.
        let mut snapshots = Vec::new();
        for width in [32, 33, 34] {
            let area = Rect::new(
                /*x*/ 0,
                /*y*/ 0,
                width,
                popup.calculate_required_height(width),
            );
            let mut buf = Buffer::empty(area);
            popup.render_ref(area, &mut buf);

            let snapshot = format!("{buf:?}");
            assert!(snapshot.contains("/tier-two"));
            assert!(snapshot.contains("Keep default speed"));
            snapshots.push(format!("width {width}\n{snapshot}"));
        }

        insta::assert_snapshot!("command_popup_wrap_boundary", snapshots.join("\n\n"));
    }

    #[test]
    fn filtered_commands_keep_presentation_order_for_prefix() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/m".to_string());

        let cmds: Vec<String> = popup
            .filtered_items()
            .into_iter()
            .map(|item| match item {
                CommandItem::Builtin(cmd) => cmd.command().to_string(),
                CommandItem::ServiceTier(command) => command.name,
                CommandItem::Mcp(command) => command.text(),
                CommandItem::BackgroundTerminal(terminal) => terminal.process_id,
            })
            .collect();
        assert_eq!(
            cmds,
            vec![
                "model".to_string(),
                "memories".to_string(),
                "mention".to_string(),
                "mcp".to_string(),
                "mail".to_string(),
            ]
        );
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn app_command_popup_snapshot() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/app".to_string());

        let width = 72;
        let area = Rect::new(
            /*x*/ 0,
            /*y*/ 0,
            width,
            popup.calculate_required_height(width),
        );
        let mut buf = Buffer::empty(area);
        popup.render_ref(area, &mut buf);

        insta::assert_snapshot!("command_popup_app", format!("{buf:?}"));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn voice_command_popup_snapshot() {
        let mut popup = CommandPopup::new(
            CommandPopupFlags {
                voice_command_enabled: true,
                ..CommandPopupFlags::default()
            },
            Vec::new(),
        );
        popup.on_composer_text_change("/voice".to_string());

        let width = 72;
        let area = Rect::new(
            /*x*/ 0,
            /*y*/ 0,
            width,
            popup.calculate_required_height(width),
        );
        let mut buf = Buffer::empty(area);
        popup.render_ref(area, &mut buf);

        insta::assert_snapshot!("command_popup_voice", format!("{buf:?}"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_command_popup_items_snapshot() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/".to_string());

        let commands = popup
            .filtered_items()
            .into_iter()
            .map(|item| {
                let command = item.command();
                let description = item.description();
                format!("/{command} - {description}")
            })
            .collect::<Vec<_>>()
            .join("\n");

        insta::assert_snapshot!("command_popup_default_items", commands);
    }

    #[test]
    fn prefix_filter_limits_matches_for_ac() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/ac".to_string());

        let cmds: Vec<String> = popup
            .filtered_items()
            .into_iter()
            .map(|item| match item {
                CommandItem::Builtin(cmd) => cmd.command().to_string(),
                CommandItem::ServiceTier(command) => command.name,
                CommandItem::Mcp(command) => command.text(),
                CommandItem::BackgroundTerminal(terminal) => terminal.process_id,
            })
            .collect();
        assert!(
            !cmds.iter().any(|cmd| cmd == "compact"),
            "expected prefix search for '/ac' to exclude 'compact', got {cmds:?}"
        );
    }

    #[test]
    fn changing_filter_resets_selection_after_scrolling() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/".to_string());

        for _ in 0..MAX_POPUP_ROWS {
            popup.move_down();
        }
        assert!(popup.state.scroll_top > 0);

        popup.on_composer_text_change("/st".to_string());

        assert_eq!(
            popup.selected_item(),
            Some(CommandItem::Builtin(SlashCommand::Status))
        );
        assert_eq!(popup.state.scroll_top, 0);
        let width = 72;
        let area = Rect::new(
            /*x*/ 0,
            /*y*/ 0,
            width,
            popup.calculate_required_height(width),
        );
        let mut buf = Buffer::empty(area);
        popup.render_ref(area, &mut buf);
        insta::assert_snapshot!(
            "command_popup_filter_reset_after_scroll",
            format!("{buf:?}")
        );
    }

    #[test]
    fn quit_hidden_in_empty_filter_but_shown_for_prefix() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/".to_string());
        let items = popup.filtered_items();
        assert!(!items.contains(&CommandItem::Builtin(SlashCommand::Quit)));

        popup.on_composer_text_change("/qu".to_string());
        let items = popup.filtered_items();
        assert!(items.contains(&CommandItem::Builtin(SlashCommand::Quit)));
    }

    #[test]
    fn btw_hidden_in_empty_filter_but_shown_for_prefix() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/".to_string());
        let items = popup.filtered_items();
        assert!(!items.contains(&CommandItem::Builtin(SlashCommand::Btw)));

        popup.on_composer_text_change("/bt".to_string());
        let items = popup.filtered_items();
        assert!(items.contains(&CommandItem::Builtin(SlashCommand::Btw)));
    }

    #[test]
    fn plan_command_hidden_when_collaboration_modes_disabled() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/".to_string());

        let cmds: Vec<String> = popup
            .filtered_items()
            .into_iter()
            .map(|item| match item {
                CommandItem::Builtin(cmd) => cmd.command().to_string(),
                CommandItem::ServiceTier(command) => command.name,
                CommandItem::Mcp(command) => command.text(),
                CommandItem::BackgroundTerminal(terminal) => terminal.process_id,
            })
            .collect();
        assert!(
            !cmds.iter().any(|cmd| cmd == "plan"),
            "expected '/plan' to be hidden when collaboration modes are disabled, got {cmds:?}"
        );
    }

    #[test]
    fn plan_command_visible_when_collaboration_modes_enabled() {
        let mut popup = CommandPopup::new(
            CommandPopupFlags {
                collaboration_modes_enabled: true,
                connectors_enabled: false,
                plugins_command_enabled: false,
                token_activity_command_enabled: false,
                service_tier_commands_enabled: false,
                goal_command_enabled: false,
                voice_command_enabled: false,
                worktrees_enabled: true,
                windows_degraded_sandbox_active: false,
                side_conversation_active: false,
            },
            Vec::new(),
        );
        popup.on_composer_text_change("/plan".to_string());

        match popup.selected_item() {
            Some(CommandItem::Builtin(cmd)) => assert_eq!(cmd.command(), "plan"),
            Some(CommandItem::ServiceTier(command)) => {
                panic!("expected plan command, got service tier {command:?}")
            }
            other => panic!("expected plan to be selected for exact match, got {other:?}"),
        }
    }

    #[test]
    fn debug_commands_are_hidden_from_popup() {
        let popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        let cmds: Vec<String> = popup
            .filtered_items()
            .into_iter()
            .map(|item| match item {
                CommandItem::Builtin(cmd) => cmd.command().to_string(),
                CommandItem::ServiceTier(command) => command.name,
                CommandItem::Mcp(command) => command.text(),
                CommandItem::BackgroundTerminal(terminal) => terminal.process_id,
            })
            .collect();

        assert!(
            !cmds.iter().any(|name| name.starts_with("debug")),
            "expected no /debug* command in popup menu, got {cmds:?}"
        );
    }
    #[test]
    fn stop_args_suggest_live_background_terminal_ids() {
        let terminals = vec![
            BackgroundTerminalCompletion {
                process_id: "95306".to_string(),
                command_display: "sleep 600".to_string(),
            },
            BackgroundTerminalCompletion {
                process_id: "87742".to_string(),
                command_display: "date -Ins; sleep 3900; date -Ins".to_string(),
            },
        ];
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.set_background_terminals(terminals.clone());
        popup.on_composer_text_change("/stop 9".to_string());
        assert_eq!(
            popup.filtered_items(),
            vec![CommandItem::BackgroundTerminal(terminals[0].clone())]
        );

        popup.on_composer_text_change("/stop ".to_string());
        let width = 72;
        let area = Rect::new(
            /*x*/ 0,
            /*y*/ 0,
            width,
            popup.calculate_required_height(width),
        );
        let mut buf = Buffer::empty(area);
        popup.render_ref(area, &mut buf);
        insta::assert_snapshot!("command_popup_stop_processes", format!("{buf:?}"));
    }
}
