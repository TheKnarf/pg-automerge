-- Run by `mise run regress` (cargo pgrx regress --resetdb) right after the
-- regression database is created: install the extension being tested.
CREATE EXTENSION pg_automerge;
