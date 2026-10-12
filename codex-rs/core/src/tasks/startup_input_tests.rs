//! Both runtime owners receive the already-ordered input during tracked startup and execution.

use super::SessionTask;
use super::SessionTaskResult;
use super::TaskStartupOutcome;
use crate::agent::host::AgentRuntimeOwnership;
use crate::agent::host::AgentRuntimeServices;
use crate::agent::host::AgentTurnAdmission;
use crate::agent::host::AgentTurnAdmissionRequest;
use crate::agent::types::AgentExecutionGuard;
use crate::session::TurnInput;
use crate::session::UserInputMetadata;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_protocol::AgentPath;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::AgentInputPresentation;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::user_input::UserInput;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct AdmitInput;

impl AgentTurnAdmission for AdmitInput {
    fn admit_turn(
        &self,
        _request: AgentTurnAdmissionRequest,
    ) -> BoxFuture<'_, CodexResult<AgentExecutionGuard>> {
        Box::pin(async { Ok(AgentExecutionGuard::new(())) })
    }
}

struct CaptureInput(mpsc::UnboundedSender<Vec<TurnInput>>);

impl SessionTask for CaptureInput {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.capture_prepared_input"
    }
    async fn run_startup<'a>(
        &'a self,
        _session: Arc<Session>,
        _context: Arc<TurnContext>,
        input: &'a [TurnInput],
        _cancellation: &'a CancellationToken,
    ) -> TaskStartupOutcome {
        self.0.send(input.to_vec()).expect("capture startup input");
        TaskStartupOutcome::Run
    }
    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation: CancellationToken,
    ) -> SessionTaskResult {
        self.0.send(input).expect("capture execution input");
        cancellation.cancelled().await;
        Ok(None)
    }
}

#[test_case::test_case(false, false; "native human input")]
#[test_case::test_case(false, true; "native attributed input")]
#[test_case::test_case(true, false; "host human input")]
#[test_case::test_case(true, true; "host attributed input")]
#[tokio::test]
async fn startup_and_run_keep_prepared_mailbox_order_and_exact_prompt_metadata(
    host: bool,
    agent: bool,
) {
    let (mut session, context, _events) = make_session_and_context_with_rx().await;
    if host {
        session
            .services
            .thread_extension_data
            .insert(AgentRuntimeOwnership::Host);
        session
            .services
            .thread_extension_data
            .insert(Arc::new(AdmitInput) as Arc<dyn AgentTurnAdmission>);
        let runtime =
            AgentRuntimeServices::from_extensions(&session.services.thread_extension_data)
                .expect("host runtime providers");
        Arc::get_mut(&mut session)
            .expect("unique fixture session")
            .agent_runtime = runtime;
    }
    let communication = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("child path"),
        AgentPath::root(),
        Vec::new(),
        "earlier mailbox input".to_string(),
        /*trigger_turn*/ false,
    );
    session
        .input_queue
        .enqueue_mailbox_communication(communication.clone().into(), Default::default())
        .await;
    let content = vec![UserInput::Text {
        text: "admitted prompt".to_string(),
        text_elements: Vec::new(),
    }];
    let metadata = UserInputMetadata {
        extension_data: BTreeMap::from([(
            "input-owner".to_string(),
            serde_json::json!({"sequence": 17}),
        )]),
        acceptance_order: Some(17),
        response_item_uuid: Some(uuid::Uuid::now_v7()),
        ..Default::default()
    };
    let prompt = if agent {
        TurnInput::AgentInput {
            context: Vec::new(),
            content,
            presentation: AgentInputPresentation::Attributed("agent task".to_string()),
            metadata,
        }
    } else {
        TurnInput::UserInput {
            content,
            client_id: Some("client-submission".to_string()),
            metadata,
        }
    };
    let expected = vec![
        TurnInput::InterAgentCommunication(communication.into()),
        prompt.clone(),
    ];
    let (sent, mut received) = mpsc::unbounded_channel();
    session
        .start_task(Arc::clone(&context), vec![prompt], CaptureInput(sent))
        .await;
    let captured = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        [
            received.recv().await.expect("startup"),
            received.recv().await.expect("execution"),
        ]
    })
    .await
    .expect("registered task reaches both phases");
    assert_eq!(captured, [expected.clone(), expected]);
    assert!(!session.input_queue.has_pending_mailbox_items().await);
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}
