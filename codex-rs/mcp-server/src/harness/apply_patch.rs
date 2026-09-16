use codex_apply_patch::AppliedPatchFileChange;
use codex_apply_patch::ApplyPatchArgs;
use codex_apply_patch::ApplyPatchFileChange;
use codex_apply_patch::ApplyPatchOptions;
use codex_apply_patch::MaybeApplyPatchVerified;
use codex_apply_patch::apply_patch_with_options;
use codex_apply_patch::parse_patch;
use codex_apply_patch::verify_apply_patch_args;
use codex_utils_path_uri::PathUri;

use crate::harness::types::ApplyPatchParams;
use crate::harness::types::ApplyPatchResponse;
use crate::harness::types::FilePatchAction;

pub async fn handle_apply_patch(params: ApplyPatchParams) -> ApplyPatchResponse {
    let native_cwd = match &params.cwd {
        Some(dir) => match codex_utils_absolute_path::AbsolutePathBuf::from_unknown_path(dir) {
            Ok(p) => p,
            Err(e) => {
                return ApplyPatchResponse {
                    success: false,
                    summary: format!("Invalid working directory '{dir}': {e}"),
                    files: Vec::new(),
                };
            }
        },
        None => match codex_utils_absolute_path::AbsolutePathBuf::current_dir() {
            Ok(p) => p,
            Err(e) => {
                return ApplyPatchResponse {
                    success: false,
                    summary: format!("Failed to determine current directory: {e}"),
                    files: Vec::new(),
                };
            }
        },
    };
    let cwd_uri = PathUri::from_abs_path(&native_cwd);

    let parsed_patch = match parse_patch(&params.patch) {
        Ok(p) => p,
        Err(e) => {
            return ApplyPatchResponse {
                success: false,
                summary: format!("Failed to parse patch: {e}"),
                files: Vec::new(),
            };
        }
    };

    if params.check_only.unwrap_or(false) {
        let args = ApplyPatchArgs {
            patch: params.patch.clone(),
            hunks: parsed_patch.hunks,
            workdir: None,
            environment_id: None,
        };

        match verify_apply_patch_args(
            args,
            &cwd_uri,
            codex_exec_server::LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        {
            MaybeApplyPatchVerified::Body(action) => {
                let mut files = Vec::new();
                for (path_uri, change) in action.changes() {
                    let action_str = match change {
                        ApplyPatchFileChange::Add { .. } => "created",
                        ApplyPatchFileChange::Delete { .. } => "deleted",
                        ApplyPatchFileChange::Update { .. } => "modified",
                    };
                    files.push(FilePatchAction {
                        path: path_uri.inferred_native_path_string(),
                        action: action_str.to_string(),
                    });
                }
                let file_count = files.len();
                ApplyPatchResponse {
                    success: true,
                    summary: format!(
                        "Dry-run succeeded cleanly. {file_count} file(s) would be affected."
                    ),
                    files,
                }
            }
            MaybeApplyPatchVerified::CorrectnessError(e) => ApplyPatchResponse {
                success: false,
                summary: format!("Dry-run validation failed: {e}"),
                files: Vec::new(),
            },
            MaybeApplyPatchVerified::ShellParseError(e) => ApplyPatchResponse {
                success: false,
                summary: format!("Dry-run shell parse error: {e}"),
                files: Vec::new(),
            },
            MaybeApplyPatchVerified::NotApplyPatch => ApplyPatchResponse {
                success: false,
                summary: "Content does not represent an apply_patch invocation".to_string(),
                files: Vec::new(),
            },
        }
    } else {
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let options = ApplyPatchOptions::default();

        match apply_patch_with_options(
            &params.patch,
            options,
            &cwd_uri,
            &mut stdout_buf,
            &mut stderr_buf,
            codex_exec_server::LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        {
            Ok(delta) => {
                let mut files = Vec::new();
                for change in delta.changes() {
                    let action_str = match &change.change {
                        AppliedPatchFileChange::Add { .. } => "created",
                        AppliedPatchFileChange::Delete { .. } => "deleted",
                        AppliedPatchFileChange::Update { .. } => "modified",
                    };
                    files.push(FilePatchAction {
                        path: change.path.inferred_native_path_string(),
                        action: action_str.to_string(),
                    });
                }
                let file_count = files.len();
                let summary = if !stdout_buf.is_empty() {
                    String::from_utf8_lossy(&stdout_buf).to_string()
                } else {
                    format!("Successfully applied patch to {file_count} file(s).")
                };
                ApplyPatchResponse {
                    success: true,
                    summary,
                    files,
                }
            }
            Err(failure) => {
                let err_msg = if !stderr_buf.is_empty() {
                    String::from_utf8_lossy(&stderr_buf).to_string()
                } else {
                    failure.to_string()
                };
                ApplyPatchResponse {
                    success: false,
                    summary: format!("Failed to apply patch: {err_msg}"),
                    files: Vec::new(),
                }
            }
        }
    }
}
