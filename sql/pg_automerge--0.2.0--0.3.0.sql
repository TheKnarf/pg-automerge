-- pg_automerge 0.2.0 -> 0.3.0 (hand-written; see
-- docs/src/pages/design/versioning.mdx, and CHANGELOG.md).
--
-- The SQL surface of 0.3.0 is 0.2.0's plus automerge_memory_usage() and
-- automerge_memory_reset() (see
-- docs/src/pages/design/memory-observability.mdx).
-- Every 0.2.0 object is unchanged (definition, labels, symbol, comment),
-- and every C symbol 0.2.0's objects name is still exported by the 0.3.0
-- library, so nothing else is redefined here and no dependent object
-- (index, generated column, trigger, view) is touched. No jsonb result
-- changes: expression indexes and generated columns on doc::jsonb stay
-- valid without a REINDEX or rewrite.
--
-- Plain CREATE FUNCTION: no 0.2.0 catalog has these functions, and a
-- function of the same signature that someone else put into the
-- extension's schema makes the update fail (it already exists) instead of
-- being replaced or adopted. Names are left unqualified, as in the install
-- script, so the extension stays relocatable: ALTER EXTENSION .. UPDATE
-- runs this with search_path set to the extension's schema followed by
-- pg_temp, and pg_catalog is searched first.

\echo Use "ALTER EXTENSION pg_automerge UPDATE TO '0.3.0'" to load this file. \quit

-- As generated for 0.3.0 by pgrx (src/memory.rs).
CREATE FUNCTION "automerge_memory_reset"() RETURNS void
STRICT VOLATILE PARALLEL RESTRICTED
LANGUAGE c
AS 'MODULE_PATHNAME', 'automerge_memory_reset_wrapper';

CREATE FUNCTION "automerge_memory_usage"() RETURNS TABLE (
	"allocated_bytes" bigint,
	"peak_allocated_bytes" bigint,
	"live_documents" bigint,
	"loads" bigint,
	"load_time" double precision
)
STRICT VOLATILE PARALLEL RESTRICTED
LANGUAGE c
AS 'MODULE_PATHNAME', 'automerge_memory_usage_wrapper';

COMMENT ON FUNCTION automerge_memory_usage() IS
    'Memory of pg_automerge in this backend, outside Postgres memory contexts: bytes its Rust code holds now and at most (since the backend started or automerge_memory_reset()), loaded documents alive, Automerge loads and their total time in milliseconds.';
COMMENT ON FUNCTION automerge_memory_reset() IS
    'Start the peak of automerge_memory_usage() over from the current allocation, and its load count and load time from zero, in this backend.';
