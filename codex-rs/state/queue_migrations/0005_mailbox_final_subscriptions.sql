-- Final-response observation is tied to an accepted mailbox identity and a
-- specific receiver turn. It is separate from payload consumption and from
-- the response-delivery receipt stored in the observer's canonical history.
CREATE TABLE mailbox_final_subscriptions (
    receiver_thread_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    sender_thread_id TEXT NOT NULL,
    state TEXT NOT NULL
        CHECK (state IN ('pending', 'bound', 'delivered', 'superseded', 'rejected')),
    bound_turn_id TEXT,
    PRIMARY KEY (receiver_thread_id, message_id),
    FOREIGN KEY (receiver_thread_id, message_id)
        REFERENCES mailbox_messages(receiver_thread_id, id),
    CHECK (
        (state = 'pending' AND bound_turn_id IS NULL)
        OR (state IN ('bound', 'delivered') AND bound_turn_id IS NOT NULL)
        OR state IN ('superseded', 'rejected')
    )
);

-- One current final subscription per sender/receiver pair. New accepted `zf`
-- mail supersedes the previous pending or bound opportunity in one transaction.
CREATE UNIQUE INDEX mailbox_active_final_subscription
    ON mailbox_final_subscriptions(receiver_thread_id, sender_thread_id)
    WHERE state IN ('pending', 'bound');
