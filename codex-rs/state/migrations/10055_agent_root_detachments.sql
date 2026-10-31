CREATE TABLE agent_root_detachments (
    detachment_id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    previous_session_id TEXT NOT NULL,
    previous_parent_thread_id TEXT NOT NULL,
    detached_at_ms INTEGER NOT NULL,
    FOREIGN KEY (thread_id) REFERENCES agent_alias_namespaces(session_id)
);

CREATE INDEX idx_agent_root_detachments_thread
    ON agent_root_detachments(thread_id, detachment_id);
