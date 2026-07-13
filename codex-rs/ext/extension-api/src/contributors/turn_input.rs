use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;
use std::marker::PhantomData;

/// Prepared fragments whose effects become authoritative only after host publication.
#[derive(Default)]
pub struct TurnInputContribution {
    fragments: Vec<Box<dyn crate::ContextualUserFragment + Send>>,
    acknowledgement: Option<TurnInputContributionAcknowledgement>,
}

impl TurnInputContribution {
    pub fn new(fragments: Vec<Box<dyn crate::ContextualUserFragment + Send>>) -> Self {
        Self {
            fragments,
            acknowledgement: None,
        }
    }

    pub fn with_acknowledgement(
        fragments: Vec<Box<dyn crate::ContextualUserFragment + Send>>,
        acknowledgement: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            fragments,
            acknowledgement: Some(TurnInputContributionAcknowledgement(Box::new(
                acknowledgement,
            ))),
        }
    }

    pub fn into_parts(
        self,
    ) -> (
        Vec<Box<dyn crate::ContextualUserFragment + Send>>,
        Option<TurnInputContributionAcknowledgement>,
    ) {
        (self.fragments, self.acknowledgement)
    }
}

/// Consumed exactly once after the complete contribution is writer-flushed and installed.
/// Dropping it, including after an ambiguous persistence failure, acknowledges nothing.
pub struct TurnInputContributionAcknowledgement(Box<dyn FnOnce() + Send>);

impl TurnInputContributionAcknowledgement {
    pub fn acknowledge(self) {
        (self.0)();
    }
}

/// Host-owned turn environment summary visible to turn-input contributors.
#[derive(Debug, Clone)]
pub struct TurnInputEnvironment<'a> {
    /// Stable host environment id used to route executor-scoped capabilities.
    pub environment_id: String,
    /// Effective working directory for this turn in the environment.
    pub cwd: PathUri,
    /// Whether this is the primary environment for the turn.
    pub is_primary: bool,
    // TODO(anp): Replace the marker with callback-scoped environment access.
    pub _lifetime: PhantomData<&'a ()>,
}

/// Turn facts supplied before the host records turn-local model input items.
#[derive(Debug, Clone)]
pub struct TurnInputContext<'a> {
    /// Stable host-owned turn identifier.
    pub turn_id: String,
    /// User input submitted for this turn.
    pub user_input: Vec<UserInput>,
    /// Resolved turn environments, in host priority order.
    pub environments: Vec<TurnInputEnvironment<'a>>,
}
