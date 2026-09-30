-- pg_automerge 0.1.0 -> 0.2.0 (hand-written; see docs/DESIGN.md,
-- "Versioning and upgrades", and CHANGELOG.md).
--
-- The SQL surface of 0.2.0 is 0.1.0's plus automerge_spans (two
-- overloads). Every 0.1.0 object is unchanged (definition, labels,
-- symbol, comment), and every C symbol 0.1.0's objects name is still
-- exported by the 0.2.0 library, so nothing else is redefined here and no
-- dependent object (index, generated column, trigger, view) is touched.
-- The deep-block fix of 0.2.0 is inside the library and changes no jsonb
-- result: expression indexes and generated columns on doc::jsonb stay
-- valid without a REINDEX or rewrite.
--
-- Two 0.1.0 catalogs exist: the released one (the 0.1.0 image, built
-- before automerge_spans; sql/snapshots/pg_automerge--0.1.0.sql), and the
-- one of images built from the development tree between automerge_spans
-- and the version bump, still labelled 0.1.0, whose install script
-- already created automerge_spans exactly as 0.2.0 does
-- (sql/snapshots/variants/pg_automerge--0.1.0+spans.sql). Hence CREATE OR
-- REPLACE: it creates the functions in the first case and redefines them
-- identically, in place, in the second (same signature and definition).
--
-- Security (PostgreSQL's rules for extension scripts): CREATE OR REPLACE
-- in an extension script fails with "is not a member of extension" if the
-- existing function is not already the extension's, so a function of the
-- same signature that someone else put into the extension's schema makes
-- the update fail instead of being adopted. Names are left unqualified,
-- as in the install script, so the extension stays relocatable: ALTER
-- EXTENSION .. UPDATE runs this with search_path set to the extension's
-- schema followed by pg_temp, pg_catalog is searched first (jsonb and
-- text are the built-in types), and automerge is the extension's own
-- type.

\echo Use "ALTER EXTENSION pg_automerge UPDATE TO '0.2.0'" to load this file. \quit

-- As generated for 0.2.0 by pgrx (src/spans.rs).
CREATE OR REPLACE FUNCTION "automerge_spans"(
	"doc" automerge,
	"path" TEXT[]
) RETURNS jsonb
IMMUTABLE STRICT PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'automerge_spans_wrapper';

CREATE OR REPLACE FUNCTION "automerge_spans"(
	"doc" automerge,
	"path" TEXT[],
	"heads" TEXT[]
) RETURNS jsonb
IMMUTABLE STRICT PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'automerge_spans_at_wrapper';

COMMENT ON FUNCTION automerge_spans(automerge, text[]) IS
    'The text object at path (as for #>) as jsonb spans: text runs with their marks, and blocks, as Automerge''s JavaScript spans() returns them; NULL if nothing is at path.';
COMMENT ON FUNCTION automerge_spans(automerge, text[], text[]) IS
    'automerge_spans(doc, path) as of the given heads (''{}'': before any change).';
