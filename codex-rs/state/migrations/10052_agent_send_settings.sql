-- Explicit messaging settings are independent of transient response subscriptions.
CREATE TABLE agent_directed_send_settings (
    sender_thread_id TEXT NOT NULL,
    receiver_thread_id TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('enabled', 'disabled')),
    revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision > 0),
    PRIMARY KEY (sender_thread_id, receiver_thread_id)
);

CREATE TABLE agent_subtree_send_settings (
    supervisor_thread_id TEXT PRIMARY KEY NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('enabled', 'disabled')),
    revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision > 0)
);
