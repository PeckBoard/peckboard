-- A card may depend on other cards being `done` before a worker is
-- allowed to start it. Modelled as a directed edge: the row
-- (card_id -> depends_on_card_id) means `card_id` waits on
-- `depends_on_card_id`. Many-to-many, so it lives in its own junction
-- table with real FKs rather than a JSON blob on `cards`.
--
-- ON DELETE CASCADE: deleting either endpoint card removes the edge, so
-- a deleted dependency can never strand a dependent forever.
CREATE TABLE IF NOT EXISTS card_dependencies (
    card_id             TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    depends_on_card_id  TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    created_at          TEXT NOT NULL,
    PRIMARY KEY (card_id, depends_on_card_id)
);

CREATE INDEX IF NOT EXISTS idx_card_dependencies_card
    ON card_dependencies (card_id);
CREATE INDEX IF NOT EXISTS idx_card_dependencies_depends_on
    ON card_dependencies (depends_on_card_id);
