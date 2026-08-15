use super::*;
use pretty_assertions::assert_eq;

fn entry(id: &str) -> AgentQueueEntry {
    AgentQueueEntry {
        id: id.to_string(),
        source_thread_id: ThreadId::new().to_string(),
        target_thread_id: ThreadId::new().to_string(),
        input: Vec::new(),
        prompt_preview: String::new(),
        response_handling: AgentResponseHandling::new(
            /*commentary*/ false,
            AgentFinalResponseHandling::Passive,
            /*target_messages*/ false,
            /*queue_input*/ true,
        ),
        authored_selector: None,
    }
}

#[test]
fn queue_cursor_remains_valid_after_prior_entries_are_removed() {
    let first = entry("00000000-0000-7000-8000-000000000001");
    let second = entry("00000000-0000-7000-8000-000000000002");
    let third = entry("00000000-0000-7000-8000-000000000003");

    let (page, cursor) = paginate_agent_queue(
        &[first.clone(), second.clone(), third.clone()],
        /*cursor*/ None,
        Some(2),
    )
    .expect("first page");
    assert_eq!(page, vec![first, second]);
    assert_eq!(
        cursor.as_deref(),
        Some("00000000-0000-7000-8000-000000000002")
    );

    let (page, cursor) = paginate_agent_queue(std::slice::from_ref(&third), cursor, Some(2))
        .expect("second page after drain");
    assert_eq!(page, vec![third]);
    assert_eq!(cursor, None);
}
