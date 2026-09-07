ALTER TABLE agent_aliases ADD COLUMN task_path TEXT;

-- Assignment labels belong to aliases, never to threads.agent_path (lifecycle ancestry).
UPDATE agent_aliases SET task_path = '/root' WHERE agent_ref = 1;

-- Closed agents still own their labels; transferred aliases are historical attribution only.
CREATE UNIQUE INDEX idx_agent_aliases_current_task_path
    ON agent_aliases(session_id, task_path)
    WHERE ownership_state = 'current' AND task_path IS NOT NULL;
