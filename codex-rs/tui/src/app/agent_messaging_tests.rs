use super::*;
use pretty_assertions::assert_eq;

#[test]
fn permission_summary_shows_each_direction_without_granting_main_access() {
    let main = ThreadId::new();
    let banach = ThreadId::new();
    let franklin = ThreadId::new();
    let mut navigation = AgentNavigationState::default();
    for (id, name) in [(main, "Main"), (banach, "Banach"), (franklin, "Franklin")] {
        navigation.upsert(
            id,
            Some(name.into()),
            /*agent_role*/ None,
            /*is_closed*/ false,
        );
    }
    navigation.replace_user_reply_route(franklin, banach, /*enabled*/ true);
    navigation.replace_user_reply_route(banach, franklin, /*enabled*/ false);
    let text = permission_lines(&navigation, banach, Some(main))
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text, @"
    Messaging permissions:
    Send to Franklin: enabled · explicit
    Receive from Franklin: disabled · explicit
    Permission does not assign a task.
    Visibility labels refer to the viewed thread's model.
    ");
    assert_eq!(
        permission_lines(&navigation, main, Some(main)),
        Vec::<Line<'static>>::new()
    );
}

#[test]
fn subtree_permissions_follow_new_members_with_explicit_exceptions() {
    let main = ThreadId::new();
    let coder = ThreadId::new();
    let reviewer = ThreadId::new();
    let mut navigation = AgentNavigationState::default();
    navigation.upsert(
        main,
        Some("Main".into()),
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    navigation.set_subtree_messaging(main, /*enabled*/ true);
    for (id, name) in [(coder, "Coder"), (reviewer, "Reviewer")] {
        navigation.upsert(
            id,
            Some(name.into()),
            /*agent_role*/ None,
            /*is_closed*/ false,
        );
        navigation.set_parent_thread_id(id, Some(main));
    }
    navigation.replace_user_reply_route(reviewer, coder, /*enabled*/ false);
    let text = permission_lines(&navigation, coder, Some(main))
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text, @"
    Messaging permissions:
    Send to Main [default]: enabled · inherited
    Send to Reviewer: disabled · explicit
    Receive from Reviewer: enabled · inherited
    Permission does not assign a task.
    Visibility labels refer to the viewed thread's model.
    ");
    navigation.mark_closed(main);
    assert_eq!(navigation.inherited_messaging(coder, reviewer), None);
}

#[test]
fn disabled_subtree_summary_excludes_supervisor_dispatch_in_both_views() {
    let main = ThreadId::new();
    let coder = ThreadId::new();
    let reviewer = ThreadId::new();
    let worker = ThreadId::new();
    let mut navigation = AgentNavigationState::default();
    for (id, name, parent) in [
        (main, "Main", None),
        (coder, "Coder", Some(main)),
        (reviewer, "Reviewer", Some(main)),
        (worker, "Worker", Some(coder)),
    ] {
        navigation.upsert(
            id,
            Some(name.into()),
            /*agent_role*/ None,
            /*is_closed*/ false,
        );
        navigation.set_parent_thread_id(id, parent);
    }
    navigation.set_subtree_messaging(main, /*enabled*/ false);
    let summaries = [(main, "Main"), (coder, "Coder")]
        .into_iter()
        .map(|(selected, name)| {
            let lines = permission_lines(&navigation, selected, Some(main))
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            format!("{name}:\n{lines}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    insta::assert_snapshot!(summaries, @"
    Main:
    Messaging permissions:
    Subtree messaging: disabled · includes future agents
    Receive from Coder: disabled · inherited
    Receive from Reviewer: disabled · inherited
    Receive from Worker: disabled · inherited
    Permission does not assign a task.
    Visibility labels refer to the viewed thread's model.

    Coder:
    Messaging permissions:
    Send to Main [default]: disabled · inherited
    Send to Reviewer: disabled · inherited
    Receive from Reviewer: disabled · inherited
    Receive from Worker: disabled · inherited
    Permission does not assign a task.
    Visibility labels refer to the viewed thread's model.
    ");
}
