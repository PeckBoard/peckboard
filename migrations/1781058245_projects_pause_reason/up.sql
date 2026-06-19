-- Free-form explanation shown when a project is paused — set automatically
-- by the worker-completion listener when a card's worker fails too many
-- times in a row, and cleared by `POST /api/projects/:id/resume`.
ALTER TABLE projects ADD COLUMN pause_reason TEXT;
