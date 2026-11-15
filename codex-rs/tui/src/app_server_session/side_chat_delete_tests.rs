//! Exercise the real session facade over JSON-RPC, including preflight and mutation boundaries.

use super::super::ThreadParamsMode;
use super::ATTACHED_SIDE_CHAT_SOURCE;
use super::AppServerSession;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadSource;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use color_eyre::Result;
use color_eyre::eyre::bail;
use color_eyre::eyre::ensure;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const ROOT: &str = "00000000-0000-7000-8000-000000000001";
const ARCHIVED: &str = "00000000-0000-7000-8000-000000000002";
const ACTIVE: &str = "00000000-0000-7000-8000-000000000003";
const NESTED: &str = "00000000-0000-7000-8000-000000000004";
const UNRELATED: &str = "00000000-0000-7000-8000-000000000005";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Complete,
    UnrelatedAttachment,
    Cycle,
    ProbeUnsupported,
    ListUnsupported,
    LateListFailure,
    RepeatedCursor,
    ChildUnreadable,
    DetachedBeforeDelete,
    ParentChanged,
    ChildDeleteFailure,
    ChildReplyLost,
    ParentDeleteFailure,
}

#[derive(Debug, PartialEq, Eq)]
enum Observed {
    List {
        parent: String,
        archived: bool,
        cursor: Option<String>,
    },
    Read(String),
    Delete(String),
}

#[derive(Debug)]
struct CapturedError {
    message: String,
    detail: String,
}

#[derive(Debug)]
struct Outcome {
    result: std::result::Result<(), CapturedError>,
    requests: Vec<Observed>,
    confirmed: Vec<String>,
}

fn thread(id: &str, parent: Option<&str>) -> Thread {
    Thread {
        id: id.to_string(),
        environments: None,
        extra: None,
        session_id: ROOT.to_string(),
        forked_from_id: parent.map(ToString::to_string),
        attached_to_thread_id: parent.map(ToString::to_string),
        parent_thread_id: None,
        preview: String::new(),
        ephemeral: false,
        section: None,
        section_entered_at: None,
        project_id: None,
        history_mode: ThreadHistoryMode::Paginated,
        model_provider: "other-shared-home-profile".to_string(),
        model: None,
        reasoning_effort: None,
        created_at: 0,
        updated_at: 0,
        recency_at: None,
        status: ThreadStatus::Idle,
        path: None,
        cwd: test_path_buf("/saved-side-chat-tests").abs(),
        cli_version: "test".to_string(),
        originator: None,
        source: SessionSource::Cli,
        can_accept_direct_input: None,
        thread_source: parent.map(|_| ThreadSource::Feature(ATTACHED_SIDE_CHAT_SOURCE.to_string())),
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        daybreak_enabled: None,
        turns: Vec::new(),
    }
}

async fn run_case(case: Case) -> Result<Outcome> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut requests = Vec::new();
        let mut confirmed = Vec::new();
        let mut reads = HashMap::<String, usize>::new();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let params = request.params.unwrap_or(Value::Null);
            let mut reply = match request.method.as_str() {
                "initialize" => json!({"result": {"userAgent": "side-chat-delete-test/1.0"}}),
                "thread/list" => {
                    let params: ThreadListParams = serde_json::from_value(params)?;
                    ensure!(
                        !params.current_account_only,
                        "saved children must be visible across shared-home profiles"
                    );
                    ensure!(
                        params.model_providers == Some(Vec::new()),
                        "do not filter out saved child providers"
                    );
                    let parent = params
                        .attached_to_thread_id
                        .as_deref()
                        .ok_or_else(|| color_eyre::eyre::eyre!("missing parent filter"))?;
                    if params.parent_thread_id.is_some() {
                        if case == Case::ProbeUnsupported {
                            json!({"result": {"data": [], "nextCursor": null}})
                        } else {
                            json!({"error": {"code": -32600, "message": "attachedToThreadId cannot be combined with parentThreadId or ancestorThreadId"}})
                        }
                    } else {
                        let archived = params.archived == Some(true);
                        requests.push(Observed::List {
                            parent: parent.to_string(),
                            archived,
                            cursor: params.cursor.clone(),
                        });
                        if case == Case::ListUnsupported {
                            json!({"error": {"code": -32601, "message": "attached threads is not supported yet"}})
                        } else if case == Case::LateListFailure && parent == ARCHIVED && archived {
                            json!({"error": {"code": -32603, "message": "last page unavailable"}})
                        } else {
                            let (data, cursor) = match (parent, archived, params.cursor.as_deref())
                            {
                                (ROOT, false, _) if case == Case::RepeatedCursor => {
                                    (vec![thread(ACTIVE, Some(ROOT))], Some("repeated"))
                                }
                                (ROOT, false, None) => (
                                    vec![thread(ACTIVE, Some(ROOT)), thread(ACTIVE, Some(ROOT))],
                                    Some("active-next"),
                                ),
                                (ROOT, false, Some("active-next")) => {
                                    (vec![thread(ACTIVE, Some(ROOT))], None)
                                }
                                // Duplicate listings do not create duplicate delete requests.
                                (ROOT, true, None) => (
                                    vec![thread(ARCHIVED, Some(ROOT)), thread(ACTIVE, Some(ROOT))],
                                    None,
                                ),
                                (ACTIVE, false, None) => (vec![thread(NESTED, Some(ACTIVE))], None),
                                (NESTED, false, None) if case == Case::Cycle => {
                                    (vec![thread(ROOT, Some(NESTED))], None)
                                }
                                (_, _, None) => (Vec::new(), None),
                                _ => bail!("unexpected list cursor"),
                            };
                            let data = data
                                .into_iter()
                                .map(|mut thread| {
                                    // List rows are discovery, not complete attachment authority.
                                    thread.forked_from_id = None;
                                    thread.thread_source = None;
                                    thread
                                })
                                .collect::<Vec<_>>();
                            json!({"result": {"data": data, "nextCursor": cursor}})
                        }
                    }
                }
                "thread/read" => {
                    let params: ThreadReadParams = serde_json::from_value(params)?;
                    let id = params.thread_id.as_str();
                    ensure!(!params.include_turns, "metadata-only verification");
                    requests.push(Observed::Read(id.to_string()));
                    let count = reads.entry(id.to_string()).or_default();
                    *count += 1;
                    if id == ARCHIVED && case == Case::ChildUnreadable {
                        json!({"error": {"code": -32600, "message": "thread not found"}})
                    } else {
                        let mut current = match id {
                            ROOT => thread(ROOT, None),
                            ARCHIVED | ACTIVE => thread(id, Some(ROOT)),
                            NESTED => thread(NESTED, Some(ACTIVE)),
                            _ => bail!("unexpected metadata read for {id}"),
                        };
                        if id == ARCHIVED && case == Case::UnrelatedAttachment {
                            current.thread_source = Some(ThreadSource::User);
                        }
                        if id == ARCHIVED && *count > 1 && case == Case::DetachedBeforeDelete {
                            current.attached_to_thread_id = None;
                        }
                        if id == ROOT && *count > 1 && case == Case::ParentChanged {
                            current.attached_to_thread_id = Some(UNRELATED.to_string());
                        }
                        json!({"result": {"thread": current, "readState": null}})
                    }
                }
                "thread/delete" => {
                    let id = params["threadId"]
                        .as_str()
                        .ok_or_else(|| color_eyre::eyre::eyre!("missing delete ID"))?;
                    requests.push(Observed::Delete(id.to_string()));
                    if id == ARCHIVED && case == Case::ChildReplyLost {
                        // The mutation happened, but its acknowledgement is deliberately lost.
                        confirmed.push(id.to_string());
                        socket.close(None).await?;
                        break;
                    }
                    if (id == ARCHIVED && case == Case::ChildDeleteFailure)
                        || (id == ROOT && case == Case::ParentDeleteFailure)
                    {
                        json!({"error": {"code": -32600, "message": "writer or surviving reference prevents deletion"}})
                    } else {
                        ensure!(
                            !confirmed.contains(&id.to_string()),
                            "no automatic retry or duplicate delete"
                        );
                        confirmed.push(id.to_string());
                        json!({"result": {}})
                    }
                }
                method => bail!("unexpected request {method}"),
            };
            reply["id"] = json!(request.id);
            socket.send(Message::Text(reply.to_string().into())).await?;
        }
        Ok::<_, color_eyre::Report>((requests, confirmed))
    });
    let mut session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Embedded,
    );
    let result = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 10),
        session.delete_thread_with_saved_side_chats(ThreadId::from_string(ROOT)?),
    )
    .await?
    .map_err(|error| CapturedError {
        message: error.to_string(),
        detail: format!("{error:#}"),
    });
    session.shutdown().await?;
    let (requests, confirmed) =
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), server).await???;
    Ok(Outcome {
        result,
        requests,
        confirmed,
    })
}

#[tokio::test]
async fn complete_shared_home_preflight_deletes_nested_and_archived_children_before_parent()
-> Result<()> {
    let outcome = run_case(Case::Complete).await?;
    assert!(outcome.result.is_ok(), "{outcome:?}");
    assert_eq!(outcome.confirmed, [NESTED, ACTIVE, ARCHIVED, ROOT]);
    let first_delete = outcome
        .requests
        .iter()
        .position(|request| matches!(request, Observed::Delete(_)))
        .expect("delete requests");
    let last_list = outcome
        .requests
        .iter()
        .rposition(|request| matches!(request, Observed::List { .. }))
        .expect("preflight discovery");
    assert!(
        last_list < first_delete,
        "entire graph is discovered before mutation"
    );
    for id in [ROOT, ARCHIVED, ACTIVE, NESTED] {
        assert_eq!(
            outcome
                .requests
                .iter()
                .filter(|request| **request == Observed::Read(id.to_string()))
                .count(),
            2
        );
    }
    assert!(!outcome.confirmed.iter().any(|id| id == UNRELATED));
    Ok(())
}

#[tokio::test]
async fn cascade_rejects_unrelated_attachment() -> Result<()> {
    incomplete_or_unverified_cascade_never_sends_delete_requests(
        Case::UnrelatedAttachment,
        "not a current saved TUI side chat",
    )
    .await
}

#[tokio::test]
async fn cascade_rejects_attachment_cycle() -> Result<()> {
    incomplete_or_unverified_cascade_never_sends_delete_requests(Case::Cycle, "Cycle").await
}

#[tokio::test]
async fn cascade_rejects_legacy_daemon_without_filter() -> Result<()> {
    incomplete_or_unverified_cascade_never_sends_delete_requests(
        Case::ProbeUnsupported,
        "safely enumerate",
    )
    .await
}

#[tokio::test]
async fn cascade_rejects_unsupported_store() -> Result<()> {
    incomplete_or_unverified_cascade_never_sends_delete_requests(
        Case::ListUnsupported,
        "not supported",
    )
    .await
}

#[tokio::test]
async fn cascade_rejects_late_enumeration_failure() -> Result<()> {
    incomplete_or_unverified_cascade_never_sends_delete_requests(
        Case::LateListFailure,
        "last page unavailable",
    )
    .await
}

#[tokio::test]
async fn cascade_rejects_repeated_cursor() -> Result<()> {
    incomplete_or_unverified_cascade_never_sends_delete_requests(
        Case::RepeatedCursor,
        "Repeated side chat list cursor",
    )
    .await
}

#[tokio::test]
async fn cascade_rejects_missing_canonical_child() -> Result<()> {
    incomplete_or_unverified_cascade_never_sends_delete_requests(
        Case::ChildUnreadable,
        "thread not found",
    )
    .await
}

async fn incomplete_or_unverified_cascade_never_sends_delete_requests(
    case: Case,
    detail: &str,
) -> Result<()> {
    let outcome = run_case(case).await?;
    let error = outcome.result.expect_err("preflight must fail closed");
    assert!(error.detail.contains(detail), "{}", error.detail);
    assert!(error.message.contains("no delete requests were sent"));
    assert_eq!(outcome.confirmed, Vec::<String>::new());
    assert!(
        !outcome
            .requests
            .iter()
            .any(|request| matches!(request, Observed::Delete(_)))
    );
    Ok(())
}

#[tokio::test]
async fn cascade_stops_when_child_detaches() -> Result<()> {
    attachment_revalidation_stops_before_deleting_a_changed_identity(Case::DetachedBeforeDelete)
        .await
}

#[tokio::test]
async fn cascade_stops_when_parent_changes() -> Result<()> {
    attachment_revalidation_stops_before_deleting_a_changed_identity(Case::ParentChanged).await
}

async fn attachment_revalidation_stops_before_deleting_a_changed_identity(
    case: Case,
) -> Result<()> {
    let outcome = run_case(case).await?;
    let error = outcome.result.expect_err("revalidation must fail");
    let expected = if case == Case::ParentChanged {
        vec![NESTED, ACTIVE, ARCHIVED]
    } else {
        vec![NESTED, ACTIVE]
    };
    assert_eq!(outcome.confirmed, expected);
    assert_eq!(
        outcome
            .requests
            .iter()
            .filter_map(|request| match request {
                Observed::Delete(id) => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        expected
    );
    assert!(error.message.contains("Parent deletion was not requested"));
    Ok(())
}

#[tokio::test]
async fn cascade_reports_progress_after_rejected_delete() -> Result<()> {
    child_delete_failure_reports_confirmed_progress_without_retrying_or_deleting_parent(
        Case::ChildDeleteFailure,
    )
    .await
}

#[tokio::test]
async fn cascade_reports_progress_after_lost_delete_reply() -> Result<()> {
    child_delete_failure_reports_confirmed_progress_without_retrying_or_deleting_parent(
        Case::ChildReplyLost,
    )
    .await
}

async fn child_delete_failure_reports_confirmed_progress_without_retrying_or_deleting_parent(
    case: Case,
) -> Result<()> {
    let outcome = run_case(case).await?;
    let error = outcome
        .result
        .expect_err("server rejects a protected child");
    let actually_deleted = if case == Case::ChildReplyLost {
        vec![NESTED, ACTIVE, ARCHIVED]
    } else {
        vec![NESTED, ACTIVE]
    };
    assert_eq!(outcome.confirmed, actually_deleted);
    assert_eq!(
        outcome
            .requests
            .iter()
            .filter_map(|request| match request {
                Observed::Delete(id) => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [NESTED, ACTIVE, ARCHIVED]
    );
    if case == Case::ChildDeleteFailure {
        assert!(error.detail.contains("surviving reference"));
    }
    insta::assert_snapshot!(error.message, @r"Deletion of saved side chat 00000000-0000-7000-8000-000000000002 was not confirmed. Saved side chats confirmed deleted: [00000000-0000-7000-8000-000000000004, 00000000-0000-7000-8000-000000000003]. Parent deletion was not requested. Refresh before retrying.");
    Ok(())
}

#[tokio::test]
async fn parent_delete_failure_does_not_claim_success_after_confirmed_child_deletions() -> Result<()>
{
    let outcome = run_case(Case::ParentDeleteFailure).await?;
    let error = outcome.result.expect_err("server keeps a protected parent");
    assert_eq!(outcome.confirmed, [NESTED, ACTIVE, ARCHIVED]);
    assert_eq!(
        outcome
            .requests
            .iter()
            .filter_map(|request| match request {
                Observed::Delete(id) => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [NESTED, ACTIVE, ARCHIVED, ROOT]
    );
    assert!(error.message.contains("Deletion of parent"));
    assert!(error.message.contains("was not confirmed"));
    assert!(!error.message.contains("Parent deletion was not requested"));
    Ok(())
}
