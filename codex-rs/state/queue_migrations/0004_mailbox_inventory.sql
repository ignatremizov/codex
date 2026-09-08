-- One recoverable inventory preparation per receiver, independent of consumption claims.
CREATE TABLE mailbox_inventory_state (
    receiver_thread_id TEXT PRIMARY KEY NOT NULL,
    notified_through INTEGER NOT NULL DEFAULT 0
        CHECK (typeof(notified_through) = 'integer' AND notified_through >= 0),
    notification_id TEXT UNIQUE,
    notification_through INTEGER,
    pending_senders_json TEXT,
    CHECK (
        (notification_id IS NULL AND notification_through IS NULL AND pending_senders_json IS NULL)
        OR
        (notification_id IS NOT NULL AND notification_through IS NOT NULL
         AND typeof(notification_through) = 'integer'
         AND notification_through > notified_through AND pending_senders_json IS NOT NULL)
    )
);
