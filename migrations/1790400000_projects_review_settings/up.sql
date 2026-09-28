-- Per-project settings for the shared `review` workflow step: a fresh
-- session independently verifies each card before it lands on `done`.
-- review_enabled defaults to 1 (on); review_model / review_effort NULL
-- mean "same as the project's model / effort".
ALTER TABLE projects ADD COLUMN review_enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE projects ADD COLUMN review_model TEXT NULL;
ALTER TABLE projects ADD COLUMN review_effort TEXT NULL;
