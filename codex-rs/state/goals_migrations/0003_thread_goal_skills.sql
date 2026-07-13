ALTER TABLE thread_goals
ADD COLUMN skill_selections_json TEXT NOT NULL DEFAULT '[]';
