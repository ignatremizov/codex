-- Mailbox state is independent of the ordinary editable/reorderable user queue.
CREATE TABLE mailbox_messages (
    acceptance_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    receiver_thread_id TEXT NOT NULL,
    submission_key TEXT NOT NULL,
    sender_key TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'claimed', 'consumed', 'rejected')),
    rejection_reason TEXT,
    UNIQUE (receiver_thread_id, submission_key),
    UNIQUE (receiver_thread_id, id),
    CHECK ((state = 'rejected' AND rejection_reason IS NOT NULL)
        OR (state <> 'rejected' AND rejection_reason IS NULL))
);

CREATE INDEX mailbox_pending_receiver_sender
    ON mailbox_messages(receiver_thread_id, state, sender_key, acceptance_sequence);

-- An invocation row also records an empty selection result.
CREATE TABLE mailbox_claims (
    receiver_thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    selection_json TEXT NOT NULL,
    PRIMARY KEY (receiver_thread_id, turn_id, tool_call_id)
);

CREATE TABLE mailbox_claim_members (
    receiver_thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    message_id TEXT NOT NULL UNIQUE,
    delivery_id TEXT NOT NULL UNIQUE,
    PRIMARY KEY (receiver_thread_id, turn_id, tool_call_id, message_id),
    FOREIGN KEY (receiver_thread_id, turn_id, tool_call_id)
        REFERENCES mailbox_claims(receiver_thread_id, turn_id, tool_call_id),
    FOREIGN KEY (receiver_thread_id, message_id)
        REFERENCES mailbox_messages(receiver_thread_id, id)
);
