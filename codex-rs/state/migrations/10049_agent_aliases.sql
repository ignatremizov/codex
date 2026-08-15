CREATE TABLE agent_alias_namespaces (
    session_id TEXT PRIMARY KEY,
    next_agent_ref INTEGER NOT NULL CHECK (next_agent_ref >= 2)
);

CREATE TABLE agent_aliases (
    session_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    agent_ref INTEGER NOT NULL CHECK (agent_ref >= 1),
    nickname TEXT,
    ownership_state TEXT NOT NULL CHECK (ownership_state IN ('current', 'transferred')),
    PRIMARY KEY (session_id, thread_id),
    UNIQUE (session_id, agent_ref),
    UNIQUE (session_id, nickname),
    FOREIGN KEY (session_id)
        REFERENCES agent_alias_namespaces(session_id)
        ON DELETE CASCADE
);

CREATE INDEX idx_agent_aliases_session_ownership_ref
    ON agent_aliases(session_id, ownership_state, agent_ref);

CREATE TABLE agent_alias_nickname_reservations (
    session_id TEXT NOT NULL,
    nickname TEXT NOT NULL,
    source_thread_id TEXT NOT NULL,
    PRIMARY KEY (session_id, nickname),
    FOREIGN KEY (session_id)
        REFERENCES agent_alias_namespaces(session_id)
        ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_agent_aliases_current_owner
    ON agent_aliases(thread_id)
    WHERE ownership_state = 'current';

CREATE TABLE agent_alias_transfers (
    transfer_id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    previous_session_id TEXT,
    new_session_id TEXT NOT NULL,
    previous_parent_thread_id TEXT,
    new_parent_thread_id TEXT NOT NULL,
    authored_selector TEXT NOT NULL,
    transferred_at_ms INTEGER NOT NULL,
    FOREIGN KEY (new_session_id)
        REFERENCES agent_alias_namespaces(session_id)
);

CREATE INDEX idx_agent_alias_transfers_thread
    ON agent_alias_transfers(thread_id, transfer_id);
