-- Add worker communication configuration to projects
ALTER TABLE projects ADD COLUMN auto_notify_changes BOOLEAN NOT NULL DEFAULT 1;
ALTER TABLE projects ADD COLUMN worker_communication BOOLEAN NOT NULL DEFAULT 1;
