//! Profile seeding shared by shell snapshot capture and other initialized shells.
//! Preserve the existing scripts; callers own the shell's launch flags and lifecycle.

use crate::shell_detect::ShellType;

/// Return a script that loads interactive configuration for snapshot capture.
///
/// Callers choose the shell's launch flags and whether login profiles run. Core
/// capture can use login startup, while executor Bash capture suppresses login
/// profiles and automatic `.bashrc` loading before running this script.
/// Bash sources `$HOME/.bashrc` only when `BASH_ENV` is unset or empty, otherwise
/// relying on the shell's automatic `BASH_ENV` startup. Zsh sources `.zshrc` from
/// `ZDOTDIR`, falling back to `HOME`.
/// Only Bash and Zsh are supported here; POSIX sh's ENV handling remains in capture.
pub fn shell_startup_script(shell_type: ShellType) -> &'static str {
    match shell_type {
        ShellType::Zsh => {
            r#"if [[ -n "${ZDOTDIR-}" ]]; then
  rc="$ZDOTDIR/.zshrc"
elif [[ -n "${HOME-}" ]]; then
  rc="$HOME/.zshrc"
else
  rc=
fi
[[ -r "$rc" ]] && . "$rc"
"#
        }
        ShellType::Bash => {
            r#"if [ -z "${BASH_ENV-}" ] && [ -n "${HOME-}" ] && [ -r "$HOME/.bashrc" ]; then
  . "$HOME/.bashrc"
fi
"#
        }
        ShellType::Sh | ShellType::PowerShell | ShellType::Cmd => "",
    }
}
