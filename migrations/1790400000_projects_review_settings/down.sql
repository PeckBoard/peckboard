-- SQLite cannot drop a column without rebuilding the table; leave the
-- review columns in place (unused by older binaries).
SELECT 1;
