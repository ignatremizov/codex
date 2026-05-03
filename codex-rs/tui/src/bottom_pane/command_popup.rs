use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::WidgetRef;

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
use crate::render::Insets;
use crate::render::RectExt;
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
    McpSubcommand(&'static str),
    McpServer(String),
}

pub(crate) struct CommandPopup {
    command_filter: String,
    commands: Vec<CommandItem>,
    composer_text: String,
    mcp_server_names: Vec<String>,
    state: ScrollState,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CommandPopupFlags {
    pub(crate) collaboration_modes_enabled: bool,
    pub(crate) connectors_enabled: bool,
    pub(crate) plugins_command_enabled: bool,
    pub(crate) token_activity_command_enabled: bool,
    pub(crate) service_tier_commands_enabled: bool,
    pub(crate) goal_command_enabled: bool,
    pub(crate) personality_command_enabled: bool,
    pub(crate) realtime_conversation_enabled: bool,
    pub(crate) audio_device_selection_enabled: bool,
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
            personality_command_enabled: value.personality_command_enabled,
            realtime_conversation_enabled: value.realtime_conversation_enabled,
            audio_device_selection_enabled: value.audio_device_selection_enabled,
            allow_elevate_sandbox: value.windows_degraded_sandbox_active,
            side_conversation_active: value.side_conversation_active,
        }
    }
}

impl CommandPopup {
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
            mcp_server_names: Vec::new(),
            state: ScrollState::new(),
        }
    }

    pub(crate) fn set_mcp_server_names(&mut self, server_names: Vec<String>) {
        self.mcp_server_names = server_names;
    }

    /// Update the filter string based on the current composer text. The text
    /// passed in is expected to start with a leading '/'. Everything after the
    /// *first* '/' on the *first* line becomes the active filter that is used
    /// to narrow down the list of available commands.
    pub(crate) fn on_composer_text_change(&mut self, text: String) {
        let first_line = text.lines().next().unwrap_or("");
        let previous_filter = self.command_filter.clone();
        self.composer_text = first_line.to_string();

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

        if self.command_filter != previous_filter {
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
        if let Some(mcp_matches) = self.filtered_mcp_args() {
            return mcp_matches;
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
            match command {
                CommandItem::Builtin(cmd) => {
                    push_match((*command).clone(), cmd.command(), None, 0);
                }
                CommandItem::ServiceTier(command) => {
                    push_match(
                        CommandItem::ServiceTier(command.clone()),
                        &command.name,
                        None,
                        0,
                    );
                }
                CommandItem::McpSubcommand(_) | CommandItem::McpServer(_) => {}
            }
        }

        out.extend(exact);
        out.extend(prefix);
        out
    }

    fn filtered_mcp_args(&self) -> Option<Vec<(CommandItem, Option<Vec<usize>>)>> {
        let tail = self.composer_text.strip_prefix("/mcp")?;
        if !tail.is_empty() && !tail.starts_with(char::is_whitespace) {
            return None;
        }

        let args = tail.trim_start();
        if args.is_empty() {
            return Some(vec![
                (CommandItem::McpSubcommand("use"), None),
                (CommandItem::McpSubcommand("verbose"), None),
            ]);
        }

        let mut parts = args.splitn(2, char::is_whitespace);
        let first_token = parts.next().unwrap_or("");
        let rest = parts.next();
        if let Some(rest) = rest
            && first_token.eq_ignore_ascii_case("use")
        {
            let server_filter = rest.trim_start();
            let server_filter_lower = server_filter.to_lowercase();
            let filter_chars = server_filter.chars().count();
            let matches = self
                .mcp_server_names
                .iter()
                .filter(|server_name| server_name.to_lowercase().starts_with(&server_filter_lower))
                .map(|server_name| {
                    let indices = (!server_filter.is_empty()).then(|| (0..filter_chars).collect());
                    (CommandItem::McpServer(server_name.clone()), indices)
                })
                .collect();
            return Some(matches);
        }

        let filter_lower = first_token.to_lowercase();
        let filter_chars = first_token.chars().count();
        let matches = ["use", "verbose"]
            .into_iter()
            .filter(|subcommand| subcommand.starts_with(&filter_lower))
            .map(|subcommand| {
                let indices = (!first_token.is_empty()).then(|| (0..filter_chars).collect());
                (CommandItem::McpSubcommand(subcommand), indices)
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
            .map(|(item, indices)| {
                let (name, description, match_offset) = match item {
                    CommandItem::Builtin(cmd) => (
                        format!("/{}", cmd.command()),
                        cmd.description().to_string(),
                        1,
                    ),
                    CommandItem::ServiceTier(command) => {
                        (format!("/{}", command.name), command.description, 1)
                    }
                    CommandItem::McpSubcommand("use") => (
                        "use".to_string(),
                        "add an MCP server's tools to context".to_string(),
                        0,
                    ),
                    CommandItem::McpSubcommand("verbose") => (
                        "verbose".to_string(),
                        "show detailed MCP inventory".to_string(),
                        0,
                    ),
                    CommandItem::McpSubcommand(subcommand) => {
                        (subcommand.to_string(), String::new(), 0)
                    }
                    CommandItem::McpServer(server_name) => (
                        server_name,
                        "add this MCP server's tools to context".to_string(),
                        0,
                    ),
                };
                GenericDisplayRow {
                    name,
                    name_prefix_spans: Vec::new(),
                    match_indices: indices
                        .map(|v| v.into_iter().map(|i| i + match_offset).collect()),
                    display_shortcut: None,
                    description: Some(description),
                    category_tag: None,
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
    pub(crate) fn command(&self) -> &str {
        match self {
            Self::Builtin(cmd) => cmd.command(),
            Self::ServiceTier(command) => &command.name,
            Self::McpSubcommand(subcommand) => subcommand,
            Self::McpServer(server_name) => server_name,
        }
    }
}

impl WidgetRef for CommandPopup {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let rows = self.rows_from_matches(self.filtered());
        render_rows_with_col_width_mode(
            area.inset(Insets::tlbr(
                /*top*/ 0, /*left*/ 2, /*bottom*/ 0, /*right*/ 0,
            )),
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

    fn builtin_command_names(popup: &CommandPopup) -> Vec<&'static str> {
        popup
            .filtered_items()
            .into_iter()
            .filter_map(|item| match item {
                CommandItem::Builtin(cmd) => Some(cmd.command()),
                CommandItem::ServiceTier(_)
                | CommandItem::McpSubcommand(_)
                | CommandItem::McpServer(_) => None,
            })
            .collect()
    }

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
            CommandItem::ServiceTier(_)
            | CommandItem::McpSubcommand(_)
            | CommandItem::McpServer(_) => false,
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
            other => panic!("expected init to be selected for exact match, got {other:?}"),
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
            other => panic!("expected model as first match for '/mo', got {other:?}"),
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
    fn filtered_commands_keep_presentation_order_for_prefix() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/m".to_string());

        let cmds = builtin_command_names(&popup);
        assert_eq!(cmds, vec!["model", "memories", "mention", "mcp"]);
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

    #[test]
    fn prefix_filter_limits_matches_for_ac() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/ac".to_string());

        let cmds = builtin_command_names(&popup);
        assert!(
            !cmds.contains(&"compact"),
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

        let cmds = builtin_command_names(&popup);
        assert!(
            !cmds.contains(&"collab"),
            "expected '/collab' to be hidden when collaboration modes are disabled, got {cmds:?}"
        );
        assert!(
            !cmds.contains(&"plan"),
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
                personality_command_enabled: true,
                realtime_conversation_enabled: false,
                audio_device_selection_enabled: false,
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
    fn personality_command_hidden_when_disabled() {
        let mut popup = CommandPopup::new(
            CommandPopupFlags {
                collaboration_modes_enabled: true,
                connectors_enabled: false,
                plugins_command_enabled: false,
                token_activity_command_enabled: false,
                service_tier_commands_enabled: false,
                goal_command_enabled: false,
                personality_command_enabled: false,
                realtime_conversation_enabled: false,
                audio_device_selection_enabled: false,
                windows_degraded_sandbox_active: false,
                side_conversation_active: false,
            },
            Vec::new(),
        );
        popup.on_composer_text_change("/pers".to_string());

        let cmds = builtin_command_names(&popup);
        assert!(
            !cmds.contains(&"personality"),
            "expected '/personality' to be hidden when disabled, got {cmds:?}"
        );
    }

    #[test]
    fn personality_command_visible_when_enabled() {
        let mut popup = CommandPopup::new(
            CommandPopupFlags {
                collaboration_modes_enabled: true,
                connectors_enabled: false,
                plugins_command_enabled: false,
                token_activity_command_enabled: false,
                service_tier_commands_enabled: false,
                goal_command_enabled: false,
                personality_command_enabled: true,
                realtime_conversation_enabled: false,
                audio_device_selection_enabled: false,
                windows_degraded_sandbox_active: false,
                side_conversation_active: false,
            },
            Vec::new(),
        );
        popup.on_composer_text_change("/personality".to_string());

        match popup.selected_item() {
            Some(CommandItem::Builtin(cmd)) => assert_eq!(cmd.command(), "personality"),
            Some(CommandItem::ServiceTier(command)) => {
                panic!("expected personality command, got service tier {command:?}")
            }
            other => panic!("expected personality to be selected for exact match, got {other:?}"),
        }
    }

    #[test]
    fn settings_command_hidden_when_audio_device_selection_is_disabled() {
        let mut popup = CommandPopup::new(
            CommandPopupFlags {
                collaboration_modes_enabled: false,
                connectors_enabled: false,
                plugins_command_enabled: false,
                token_activity_command_enabled: false,
                service_tier_commands_enabled: false,
                goal_command_enabled: false,
                personality_command_enabled: true,
                realtime_conversation_enabled: true,
                audio_device_selection_enabled: false,
                windows_degraded_sandbox_active: false,
                side_conversation_active: false,
            },
            Vec::new(),
        );
        popup.on_composer_text_change("/aud".to_string());

        let cmds = builtin_command_names(&popup);

        assert!(
            !cmds.contains(&"settings"),
            "expected '/settings' to be hidden when audio device selection is disabled, got {cmds:?}"
        );
    }

    #[test]
    fn debug_commands_are_hidden_from_popup() {
        let popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        let cmds = builtin_command_names(&popup);

        assert!(
            !cmds.iter().any(|name| name.starts_with("debug")),
            "expected no /debug* command in popup menu, got {cmds:?}"
        );
    }

    #[test]
    fn mcp_args_suggest_use_and_verbose_subcommands() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.on_composer_text_change("/mcp ".to_string());

        assert_eq!(
            popup.filtered_items(),
            vec![
                CommandItem::McpSubcommand("use"),
                CommandItem::McpSubcommand("verbose"),
            ]
        );

        popup.on_composer_text_change("/mcp u".to_string());
        assert_eq!(
            popup.filtered_items(),
            vec![CommandItem::McpSubcommand("use")]
        );
    }

    #[test]
    fn mcp_use_args_suggest_configured_server_names() {
        let mut popup = CommandPopup::new(CommandPopupFlags::default(), Vec::new());
        popup.set_mcp_server_names(vec![
            "linear".to_string(),
            "docs search".to_string(),
            "github".to_string(),
        ]);
        popup.on_composer_text_change("/mcp use l".to_string());

        assert_eq!(
            popup.filtered_items(),
            vec![CommandItem::McpServer("linear".to_string())]
        );

        popup.on_composer_text_change("/mcp use docs".to_string());
        assert_eq!(
            popup.filtered_items(),
            vec![CommandItem::McpServer("docs search".to_string())]
        );
    }
}
