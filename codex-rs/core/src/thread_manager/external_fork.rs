//! Foreign copied history has audit identity but no source-runtime authority in this Codex home.

use super::*;

#[derive(Clone, Copy)]
pub(super) enum ForkSourceScope {
    CurrentHome,
    ExternalHome,
}

impl ForkSourceScope {
    pub(super) fn runtime_source(self, source: Option<ThreadId>) -> Option<ThreadId> {
        match self {
            Self::CurrentHome => source,
            Self::ExternalHome => None,
        }
    }
}

impl ThreadManager {
    /// Creates a root from copied external history without consulting a same-ID local runtime.
    ///
    /// Persisted source instructions/settings remain history; supplied destination providers stay
    /// explicit. Local thread providers, attachment membership, and runtime-version fallbacks are
    /// not inherited merely because this manager happens to contain the same UUID.
    pub async fn fork_thread_from_external_history<S>(
        &self,
        snapshot: S,
        options: StartThreadOptions,
        history: InitialHistory,
    ) -> CodexResult<NewThread>
    where
        S: Into<ForkSnapshot>,
    {
        if options.internal_parent.is_some()
            || options
                .session_source
                .as_ref()
                .unwrap_or(&self.state.session_source)
                .is_non_root_agent()
        {
            return Err(CodexErr::InvalidRequest(
                "external history forks must start a root without a local control parent"
                    .to_string(),
            ));
        }
        self.fork_thread_with_initial_history(
            options,
            ForkHistory {
                snapshot: snapshot.into(),
                initial_history: history,
                persistence: ForkPersistence::Copied,
                source_scope: ForkSourceScope::ExternalHome,
            },
        )
        .await
    }
}
