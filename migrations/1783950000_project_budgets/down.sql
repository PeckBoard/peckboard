-- SQLite cannot drop a column without rebuilding the table; the project is
-- "never DROP" by convention. Leave the columns in place if rolling back.
SELECT 1;
