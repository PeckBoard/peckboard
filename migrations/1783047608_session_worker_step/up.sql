-- Which workflow step a worker session is working on. Used to decide
-- whether a card returning to a step can resume its previous worker
-- session's conversation instead of starting a fresh one. NULL for
-- non-worker sessions and for worker sessions whose card has since
-- advanced to a different step (resume link severed).
ALTER TABLE sessions ADD COLUMN worker_step TEXT;
