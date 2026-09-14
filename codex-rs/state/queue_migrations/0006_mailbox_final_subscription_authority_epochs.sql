-- Queue rows live in a separate database from graph ownership. Captured graph epochs fence an
-- accepted final-response opportunity after close/transfer even if queue cleanup is interrupted.
ALTER TABLE mailbox_final_subscriptions
    ADD COLUMN receiver_lifecycle_epoch INTEGER NOT NULL DEFAULT 0
        CHECK (receiver_lifecycle_epoch >= 0);
ALTER TABLE mailbox_final_subscriptions
    ADD COLUMN sender_lifecycle_epoch INTEGER NOT NULL DEFAULT 0
        CHECK (sender_lifecycle_epoch >= 0);

CREATE INDEX mailbox_final_subscription_sender
    ON mailbox_final_subscriptions(sender_thread_id, state);
