use super::*;
use codex_app_server_protocol::MailboxReadItem;
use codex_app_server_protocol::MailboxReadSelector;

#[tokio::test]
async fn replayed_mailbox_read_renders_the_canonical_outcome() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let _ = drain_insert_history(&mut rx);

    chat.replay_thread_item(
        AppServerThreadItem::MailboxRead(MailboxReadItem {
            id: "check-mail-replay".into(),
            selector: MailboxReadSelector::User,
            consumed_count: 2,
            rejected_count: 1,
        }),
        "turn-1".into(),
        ReplayKind::ThreadSnapshot,
    );

    let rendered = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    insta::assert_snapshot!(rendered.join("\n"), @"
    • Checked mailbox from user · 2 messages consumed · 1 message rejected
    ");
}
