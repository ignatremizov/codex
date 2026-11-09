ALTER TABLE queued_items ADD COLUMN scheduled_at INTEGER;
ALTER TABLE queued_items ADD COLUMN fallback_profile TEXT;
ALTER TABLE queued_items ADD COLUMN dispatch_error TEXT;

CREATE INDEX queued_items_due_profile_idx
    ON queued_items(fallback_profile, scheduled_at)
    WHERE scheduled_at IS NOT NULL;

CREATE TABLE queued_input_receipts (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    started INTEGER NOT NULL
);

CREATE INDEX queued_input_receipts_thread_idx
    ON queued_input_receipts(thread_id);
