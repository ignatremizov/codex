-- Monotonic observation authority survives close/resume and ownership transfer.
-- Mailbox final subscriptions capture both endpoints' epochs in the separate queue database;
-- lifecycle transitions increment these values in the same transaction as graph ownership.
CREATE TABLE agent_thread_lifecycle_authority_epochs (
    thread_id TEXT PRIMARY KEY,
    epoch INTEGER NOT NULL CHECK (epoch >= 0)
);
