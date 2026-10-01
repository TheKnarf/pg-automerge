# pg_automerge design

A Postgres 18 extension (Rust, pgrx 0.19.3, `automerge` crate 0.12.0) that
stores [Automerge](https://automerge.org) documents in an `automerge`
column type, lets you **query them as `jsonb`**, and **merges** concurrent
writes. Only Postgres 18 is supported.

Contents: [Scope](#scope) · [Architecture](#architecture) ·
[The `automerge` type](#the-automerge-type) ·
[SQL API and semantics](#sql-api-and-semantics) ·
[jsonb mapping](#jsonb-mapping) · [Resource limits](#resource-limits) ·
[Memory observability](#memory-observability) ·
[Error codes](#error-codes) ·
[Performance](#performance) · [Implementation notes](#implementation-notes) ·
[Testing](#testing) ·
[Installation, schema and privileges](#installation-schema-and-privileges) ·
[Versioning and upgrades](#versioning-and-upgrades) ·
[Docker image](#docker-image) ·
[Future work](#future-work) ·
[Appendix: benchmarks](#appendix-benchmarks)

## Scope

In scope:

- Storing Automerge documents produced elsewhere.
- Reading them with every `jsonb` operator/function/index.
- Reading the structure of rich text (marks and blocks, which the jsonb
  view flattens to a string) as jsonb spans (see
  [Rich text spans](#rich-text-spans)).
- `merge`: CRDT merge so concurrent writers never overwrite each other.
- Read-only history: listing a document's changes, fetching change bytes
  (e.g. everything since a replica's heads), and the state as of earlier
  heads (see [History](#history-read-only)).
- Change notifications: a trigger that tells listening backends (`LISTEN`)
  which rows' documents changed and their new heads (see
  [Change notifications](#change-notifications)).

Out of scope (by decision):

- Server-side edits (no `set`/`splice`/subscripting, no `jsonb -> automerge`).
- Actor IDs. All changes are authored by the application backend, which owns
  actor IDs. The extension never creates changes.
- Sync protocol, history pruning, and anything that writes history (the
  history functions only read it).

### Intended usage

The application backend syncs with frontends via Automerge and persists
documents in Postgres:

```sql
CREATE TABLE docs (id uuid PRIMARY KEY, doc automerge NOT NULL);

-- persist a full save: $2 is Automerge.save() (optionally followed by later
-- save_incremental() chunks), bound as bytea. Every writer must use its own
-- actor id: two different histories claiming the same (actor, seq) cannot be
-- merged (ERROR 22000 "conflicting automerge changes").
INSERT INTO docs (id, doc) VALUES ($1, $2)
ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc);

-- persist incrementally: $2 is only the new changes (save_incremental() /
-- save_after(heads) output, one or more change chunks) bound as bytea, or a
-- full save. A typed bytea parameter selects merge(automerge, bytea), which
-- applies the chunks on top of the stored document; no cast needed.
UPDATE docs SET doc = merge(doc, $2) WHERE id = $1;

-- (The row must exist for incremental changes: an INSERT's VALUES are cast
-- to automerge on their own, and bare changes are not a complete document.)

-- The upsert costs two loads of a full save (the cast validates it, and
-- EXCLUDED reaches merge flat, see "Expanded values"); when the row usually
-- exists, UPDATE .. SET doc = merge(doc, $2) with the save as bytea costs
-- one (see "Merging"), with an INSERT when it updated no row.

-- read
SELECT doc->>'title' FROM docs WHERE doc @> '{"status": "open"}';
SELECT jsonb_path_query(doc, '$.items[*] ? (@.done == false)') FROM docs;
```

#### Why `SET doc = merge(doc, $1)` is concurrency-safe

Under READ COMMITTED, when two transactions update the same row, the second
blocks on the row lock; after the first commits, Postgres re-evaluates the
SET expression against the *newly committed* row version (EvalPlanQual). So
`merge(doc, $1)` merges into the latest state and no write is lost.

Under REPEATABLE READ / SERIALIZABLE the second writer instead fails with a
serialization error and must retry, as with any concurrent update.

## Architecture

Two crates:

- `crates/pg_automerge_core`: all Automerge logic, plain Rust with no
  Postgres dependency, tested with `cargo test`: validation and
  normalization (`normalize`), merging and containment on stored bytes or
  loaded documents (`loaded`: `Input`, `LoadedDoc`, `merge`,
  `merge_changes`, `contains`, `contains_changes`), the `merge_agg` state
  (`MergeAccumulator`), the stored-header parser behind the heads fast path
  and the change-chunk splitter (`header`), the scan that prices a load
  before Automerge allocates (`budget`, see
  [Resource limits](#resource-limits)), the history functions
  (`history`), the jsonb mapping as a walk into a `JsonSink` (`json`), the
  notification payload builder (`notify`), text encodings (`encoding`),
  the counters of live documents and loads (`stats`, see
  [Memory observability](#memory-observability)) and, with the
  `test-hooks` feature, test instrumentation (`test_hooks`).
  Every Automerge call runs under a panic guard (see
  [Errors](#errors-and-panics)).
- The root crate (`src/`): the pgrx glue, one module per SQL area:
  `datum.rs` (the Rust types of `automerge` arguments and results, and the
  prefix reads), `expanded.rs` (expanded values), `io.rs` (the type's SQL,
  I/O functions and casts), `merge.rs` (`merge`, `||`, `merge_agg`, the
  support function), `introspect.rs` (`automerge_heads`,
  `automerge_contains`), `history.rs`, `notify.rs` (the trigger),
  `jsonb.rs` (building jsonb from the core's walk), `error.rs` (raising
  errors), and `alloc.rs` (the counting global allocator) with
  `memory.rs` (`automerge_memory_usage`, `automerge_memory_reset`). The
  glue converts datums, calls the core,
  and raises core errors with their SQLSTATE.

Data flow:

- **Writes** arrive as `bytea` or text and pass the input functions, which
  validate and normalize them into stored bytes; or they go through
  `merge`, whose result is a loaded document (an expanded value) that is
  saved once, when Postgres stores it.
- **Reads** (`::jsonb`, history) load the stored bytes into an Automerge
  document per call; `automerge_heads`, `automerge_change_count`, the
  no-op checks and the notification trigger read only a prefix of the
  stored value (the header).
- **Notifications** are built from the old and new row's headers by the
  trigger and sent with `NOTIFY`; listeners fetch what they lack with
  `automerge_changes_bytes`.

## The `automerge` type

- Variable-length (`varlena`), `STORAGE = extended`, alignment `int4`.
- Payload: the raw bytes of `Automerge::save_nocompress()`, no extra header.
  Stored uncompressed so TOAST compression (pglz, or `lz4` via
  `ALTER TABLE .. ALTER COLUMN .. SET COMPRESSION lz4`) is effective, instead of
  compressing Automerge's already-deflated columns.
- Text I/O is **lossless**, because `pg_dump`/`COPY` use it: output is `\x`
  followed by lowercase hex of the stored bytes (same shape as `bytea`); input
  accepts the same form, hex digits in either case. (No JSON text input: that
  would create history.)
- Binary I/O (`send`/`recv`): the raw bytes (recv validates and normalizes).

### Invariants

- **Normalized.** Every value entering the type (text input, binary
  receive, the `bytea` cast) is loaded with `Automerge::load` (which accepts
  a document chunk plus trailing change chunks, compressed or not) and
  re-saved with `save_nocompress()`. A stored value is therefore always a
  single uncompressed document chunk, whatever the producer sent. Empty
  input (`''::bytea`, `'\x'`) is the empty document, whose jsonb is `{}`.
- **Validated.** Input that does not load is rejected (`22P02`; `22000`
  when it is well formed but holds two conflicting histories of one actor,
  see [Error codes](#error-codes)), and so is
  input whose changes have missing dependencies: no orphaned changes are
  stored.
- **Nothing unloadable is stored** (while `pg_automerge.verify_writes` is
  on, the default; see [The `pg_automerge.verify_writes`
  setting](#the-pg_automergeverify_writes-setting)). The normalized result
  is loaded back once and must have the same heads, unless that load
  provably reads the bytes the first load already accepted:
  - the input already was the canonical encoding, or
  - the input is one document chunk with deflated columns (a compressed
    `save()`) and inflating it gives exactly the canonical encoding.
    Automerge inflates such a chunk before it parses it (copying the
    actors and heads, rewriting the column metadata with the deflate bit
    cleared, inflating each column with `flate2`, copying the head
    indices); `header::inflate_document` does the same with the same
    `flate2` crate and backend (Cargo unifies them) and adds the chunk
    header. If that equals `save_nocompress()` of the loaded document,
    loading the save parses the very bytes that `Automerge::load(input)`
    already parsed with `VerificationMode::Check`, so it cannot fail or
    change the heads. With a load memory limit (the default), the scan
    that prices the input (see [The scan](#the-scan)) has already inflated
    those columns with the same decoder, and keeps them through the load:
    `header::inflated_document_is` compares the save with the input's
    layout and those columns in place, without inflating the input again
    or hashing the result (the save's checksum is Automerge's hash of its
    own data, so equal data means an equal checksum). The core tests
    compare this path with the full load-save-load on generated
    documents, trailing changes, corrupt and non-canonical deflate
    streams, with and without a limit, and the fuzz harness checks it on
    every input.

  Malformed input with valid checksums can load into a document whose
  re-save does not load ("mismatching heads", found by the fuzz harness);
  storing it would leave an unreadable value, so it is rejected as invalid
  input.
  Such input never takes the shortcut (its re-save differs from what was
  loaded). Anything else (bare change chunks, a document plus trailing
  changes, a document encoded by another implementation) pays the second
  load. The same check applies to results of `merge(automerge, bytea)`,
  when they are first flattened (see
  [The deferred verification](#the-deferred-verification)); a full save
  merged that way takes the same shortcut when it is the result.
- **Fit the limit when written.** Every value built from client bytes,
  and every merge result, was priced before it was built and stored
  within `pg_automerge.max_load_memory` as it was set at that time (see
  [Resource limits](#resource-limits)). Loading a stored value is never
  checked, so a lower limit later never makes data unreadable.
- **Immutable.** Nothing modifies a stored value in place: `merge` returns
  a new value, and an expanded value's document is only ever replaced as a
  whole, and only through a read-write pointer.

### The `pg_automerge.verify_writes` setting

A boolean, `on` by default, superuser-only (`PGC_SUSET`; `GRANT SET ON
PARAMETER pg_automerge.verify_writes TO role` lets a role change it). It
switches the save-and-load check of the invariant above: when `off`,
values built from client bytes (text input, binary receive, the `bytea`
cast, results of `merge(automerge, bytea)`) are normalized as always but
their save is not loaded back. That removes one load from every write that
needs the check: incremental changes (`merge(doc, $changes)`: 2 loads → 1),
a save followed by change chunks, a save encoded by another
implementation, a save merged with a document neither contains. Writes
that take the shortcut (canonical bytes, a compressed save, a newer full
save merged into its older version) cost the same either way.

The risk it takes: input that passes Automerge's checksums and loads, but
whose re-save does not load (so far seen only as fuzzed input with
recomputed checksums; `tests/corpus/reload-b-*.bin`), is stored, and every
later read of that value fails with `XX000` ("corrupt stored automerge
value") until it is overwritten. With the check on, the write fails with
`22P02` instead. Turn it off only for trusted writers (a backend sending
what its own Automerge produced).

Why `PGC_SUSET` and not `PGC_USERSET`: the check protects every reader of
a table, not only the writer. A session that turned it off could store a
value that makes every later read of that row fail, for all roles, until
someone overwrites it; that is a denial of service against other users, so
only a superuser (directly, through `ALTER ROLE/DATABASE .. SET`, or by
`GRANT SET ON PARAMETER`) may decide which writers are trusted.
`PGC_SIGHUP`/`PGC_POSTMASTER` would be needlessly coarse: the typical use
is one trusted backend role alongside untrusted ones.

What stays when it is off: panic guards around every Automerge call,
Automerge's own parsing and checksum verification (`VerificationMode::Check`
on load), the missing-dependencies check of change chunks, and the
structural header parsing. Only the second load of the re-saved bytes is
skipped.

One setting covers both places the check runs: `normalize` (text input,
binary receive, the `bytea` cast, for input that is not its own canonical
or compressed encoding) and the deferred check of merge results. Both
guard the same invariant against the same failure (a re-save that does not
load), and the cost is the same load in both; a separate switch for one of
them would leave the other path open to the very value the first one lets
through (such bytes can be sent as a change chunk or as a save followed by
changes alike), so splitting it would buy no safety and add a
configuration trap. The default keeps both on.

The setting only decides whether an extra check can reject input; it never
changes a result's bytes, so the functions stay `IMMUTABLE`: for a given
input, the result is either the one value or an error, and which one does
not depend on anything but the input and this setting (like
`statement_timeout` can turn a result into an error without making a
function volatile). The deferred
check of a merge result reads the setting when the value is flattened,
not when `merge` runs. Only the extension's library defines it: set it
after the library is loaded (any use of the type loads it) or in
`postgresql.conf`/`ALTER ROLE .. SET`, where Postgres keeps the value until
the library defines the setting.

### Casts

| From → To | Context | Notes |
|---|---|---|
| `bytea → automerge` | assignment | validates + normalizes; lets drivers bind `bytea` params directly; the result is an expanded value (the loaded document and its stored bytes, see [Expanded values](#expanded-values)) |
| `automerge → bytea` | explicit | `WITHOUT FUNCTION` (same varlena layout); stored bytes, loadable by any Automerge implementation |
| `automerge → jsonb` | **implicit** | so every jsonb operator/function applies to `automerge` directly |

No `jsonb → automerge` cast (out of scope, and it would silently fabricate
history).

The implicit `jsonb` cast is deliberate. It is the only implicit cast from
`automerge`, so operator resolution is unambiguous. Do not add an implicit cast
to `json` or `text`.

### Equality and identity

There is no equality or btree opclass. `=`, `<>`, `<` etc. on two
`automerge` values still resolve, via the implicit cast, to jsonb
comparisons of the current state (not history); history equality is
`automerge_heads(a) = automerge_heads(b)`.

`merge` is commutative in state only: when neither input contains the other
the re-saved bytes depend on argument order, so `doc::bytea` is not an
identity; `automerge_heads` is.

### Errors and panics

Errors are raised with `ereport(ERROR)` and a SQLSTATE (see
[Error codes](#error-codes)); nothing panics across FFI. Client input that
is malformed (bytes, text, hashes) is always `22P02`; well-formed input
whose history conflicts with the document it meets, or that holds two
conflicting histories itself (a reused actor id), is `22000`; stored values that fail are `XX000`.

The automerge decoder is not panic-free: input whose chunk checksums are
valid but whose column data is malformed can hit `unwrap`s, index panics
and assertions inside `Automerge::load` (found by fuzzing with recomputed
checksums; `crates/pg_automerge_core/tests/fuzz.rs` finds about one per 60
mutated inputs, and `tests/corpus/` keeps one of each kind). The core crate
therefore runs every Automerge call under `catch_unwind` and maps a panic
to `22P02` for external input and `XX000` for stored values. Only panics
with a string payload (what `panic!` and failed assertions raise) are
converted; any other payload is resumed untouched. In a backend that is a
Postgres ERROR raised inside the guarded code (a query cancel from the
interrupt checks, an error from a jsonb function the walk calls), which
pgrx carries as a panic with its own payload type and must re-raise as
is, not relabel as `22P02`/`XX000`.

Messages follow PostgreSQL's error message style guide (see [Error
codes](#error-codes)). The LOCATION of an error (shown with `\set
VERBOSITY verbose`) is the source file and line that raised it.

## SQL API and semantics

All functions are `IMMUTABLE STRICT PARALLEL SAFE` unless noted: the
result depends only on the arguments (a stored value's history is part of
its bytes). `STRICT` with defaults: a NULL argument gives NULL (no rows for
the set-returning ones). Every extension object has a `COMMENT` (a pg_test
enforces it).

### Merging

- `merge(automerge, automerge) → automerge`: the state after applying every
  change of `b` missing from `a`. Commutative and idempotent in state
  (`merge(a,b)` and `merge(b,a)` have identical heads and jsonb).
  - No-op fast path: if `b`'s history is already in `a`, return `a`
    unchanged, with no re-save; symmetrically, if `b` already contains all
    of `a`, return `b` unchanged (the state is the same; this is the common
    case of a backend sending a full, newer save). Checked cheapest first:
    the same expanded object; the heads read from a prefix of each value
    (no load, and nothing else detoasted, see
    [Heads fast path](#heads-fast-path)): `heads(b) ⊆ heads(a)` → `a`,
    `heads(a) ⊊ heads(b)` → `b`; the history
    of an input that is already loaded; then the larger stored input is
    loaded and checked against the other's heads (a containing document is
    usually the larger one, so one load decides the common linear case);
    only then the other.
  - An unchanged input is returned as the datum it arrived as, not a copy:
    a stored value stays compressed or a TOAST pointer, so `UPDATE docs
    SET doc = merge(doc, x)` with nothing new keeps the row's TOAST value
    (Postgres reuses an identical TOAST pointer) instead of detoasting,
    compressing and writing the document again. On the 3 MB document such
    an update takes about 1 ms instead of 185 ms.
  - A new document is returned as an expanded value (see
    [Expanded values](#expanded-values)): kept loaded in memory and saved
    only when it is stored, sent or cast to `bytea`. When `a` is a
    read-write expanded pointer (a PL/pgSQL variable in `d := merge(d, x)`,
    or the result of an inner `merge`), the merge happens in place.
  - Merging two valid stored documents cannot fail except for a reused
    actor id (two histories with different changes for the same (actor,
    seq)): `22000` "conflicting automerge changes", with a HINT (see
    [Error codes](#error-codes)).
  - `merge` is an unreserved keyword since PG15; unqualified
    `SELECT merge(a, b)` and `SET doc = merge(doc, ...)` work on PG18
    (tested), so the function keeps the name `merge`.
- `merge(doc automerge, changes bytea) → automerge` (`doc` is called `a`
  below, as in `merge(a, b)`): applies external bytes on top of `a`, like Automerge's `load_incremental`. `changes` may be a full
  save (compressed or not, optionally followed by change chunks) or bare
  change chunks (`save_incremental()` / `save_after()` output, several may
  be concatenated) whose dependencies are in `a` or earlier in `changes`.
  - Strict, unlike `load_incremental`: a chunk that fails to parse or has
    a bad checksum fails the whole call instead of being skipped.
  - Bare uncompressed change chunks (the usual input) are split and
    checksum-checked by `header::change_chunks`, each parsed with
    `Change::try_from` (the parser `Automerge::load` uses for change
    chunks) and applied to `a`'s document with `apply_changes`: the same
    steps as a load of `a ++ changes`, without re-loading `a` when it is
    already loaded (an expanded value).
  - A save (input starting with a document chunk, compressed or not,
    optionally followed by change chunks) is merged like
    `merge(a, b::automerge)`, cheapest check first:
    1. exactly one document chunk that passes Automerge's chunk parse
       (below) with a valid checksum, whose header heads are heads of `a`
       (or changes of an expanded `a`): `a` unchanged, nothing loaded. A
       load of `a ++ changes` decides the same: for a document chunk whose
       heads it has, Automerge 0.12 (`storage/load.rs`) still parses the
       chunk (`Chunk::parse` / `Document::parse`: header, actors, heads,
       column metadata, column data, head indices, inflating deflated
       columns, the column layout) and checks its checksum, and skips only
       reconstructing its changes. `header::document_parses` checks the
       same, without reconstructing, before this answer (conservatively:
       every LEB128 canonical, everything in bounds, the
       head indices absent or complete with nothing after them, deflated
       columns inflating, and each column block a subsequence of the specs
       Automerge writes with value columns right after their metadata;
       anything it does not recognize takes the loading path, where
       Automerge decides). So a chunk with known heads and a malformed
       body is `22P02`, as that load would make it. The check is part of
       the load memory scan of the input (see
       [Resource limits](#resource-limits)), which also prices this
       answer at what parsing the chunk takes (its inflated columns,
       nothing for an uncompressed save).
    2. When the headers say the save has no more changes than a stored `a`
       (an older save) and the chunk parses, `a` is loaded first and
       checked for those heads, again as that load would.
    3. The save is loaded on its own (strictly, and complete: if trailing
       change chunks depend on changes only `a` has, the input takes the
       `a ++ changes` path below). If it contains `a` (by the heads, or its
       history has `a`'s heads), it is the result: an expanded value whose
       stored bytes are its `save_nocompress()`, which needs no
       save-and-load check when it is the input's own encoding (canonical
       bytes, or a compressed save that inflates to them: the shortcut of
       normalization). So a newer full save of the stored document costs
       one load, and the stored document is never loaded.
    4. Otherwise `a` (loaded, or a clone of an expanded `a`) gets the
       save's missing changes; the result is unverified.

    The result of case 3 is the save's normalized bytes, as `merge(a, b)`
    returns `b` when `b` contains `a`; the state is the same as a load of
    `a ++ changes` would give, the change order in the bytes may differ.
  - Anything else (compressed change chunks, a save whose trailing changes
    depend on `a`) is loaded as `a ++ changes` (for an expanded `a`, a
    fresh save of it ++ `changes`).
  - Changes whose dependencies are in neither `a` nor `changes` raise
    `22P02` (`invalid automerge changes: missing 1 dependency that neither
    the document nor the input contains`), with the hashes in the DETAIL
    (`Missing changes: <hash>.`; at most five, then "and N more"). Nothing
    orphaned is ever stored.
  - Empty `changes`, or nothing new (heads unchanged): returns `a` unchanged
    (the datum as it arrived, as above), no re-save. When `changes` is bare
    change chunks that are all heads of `a` (a re-send of the latest
    changes), this is seen from the chunks' hashes and `a`'s heads, read
    from a prefix, without detoasting or loading `a` (see
    `automerge_contains(a, bytea)`); a re-sent save of `a`, from its
    header (case 1 above).
  - Malformed input, including decoder panics, is `22P02`. A changed result
    is marked unverified (unless it is a save's own encoding, case 3
    above): it gets the save-and-load check of normalization when it is
    first flattened (see
    [The deferred verification](#the-deferred-verification)).
  - A document chunk after the first chunk of `changes` whose heads `a`
    already has is parsed and checksum-checked by Automerge (as in case 1),
    and its changes are not reconstructed.
- Operators `automerge || automerge` and `automerge || bytea` → `automerge`
  are the two `merge`s. There is no `bytea || automerge`, so the second has
  no commutator.
- Aggregate `merge_agg(automerge) → automerge`: merges all non-null inputs;
  NULL when there are none. The state (`internal`, a `MergeAccumulator`)
  loads documents only when a merge needs them, each at most once, and
  saves the result at most once:
  - The first stored input is kept as bytes with its header heads, not
    loaded. A group of one row, or of rows it contains or is contained in
    by their heads (identical values, say), loads nothing, and that row is
    the result as stored.
  - When a later input cannot be decided by the heads (an older or newer
    version of a linear history, a concurrent one), the larger of the two
    is loaded first, since it usually contains the other, as a newer
    version contains an older one. Only if it does not is the other one
    loaded too and merged into the first input's document. A version and
    a newer one cost one load in either order.
  - Once a document is loaded, a later input is checked against it by its
    heads, read from a prefix of the value (`AutomergeArg::heads`) before
    it is detoasted, so an input that adds nothing is neither detoasted
    nor loaded. An input that is loaded and contains the whole state
    replaces it (no merge); otherwise its missing changes are applied.
    Expanded inputs are used without a load.

  While the state equals one input's stored value, that value is the
  result: no save. Like `merge`, the result is the same state (heads and
  jsonb) for any input order, but its bytes can depend on the order.
  There is no combine function, so it never runs as a
  parallel partial aggregate. The loaded document lives in the Rust heap,
  outside memory-context accounting, and without a serialfunc HashAgg
  cannot spill it; the aggregate declares `SSPACE = 1048576` so the
  planner's per-group estimate is realistic and it prefers sorted grouping.
  The transition function (`automerge_merge_agg_trans`) runs
  `CHECK_FOR_INTERRUPTS` before each input. When no
  single input contains all others, the result is an expanded value
  holding a copy of the state (the final function may run more than once,
  e.g. as a window function), so `merge_agg(doc)::jsonb` needs no save and
  re-load.

#### Overload resolution

`merge(automerge, bytea)` / `automerge || bytea` and
`automerge_contains(automerge, bytea)` keep every form unambiguous
(verified per expression in the `merge_overloads_resolve_unambiguously` and
`contains_bytea_overload` pg_tests, through the view dependencies and
result type):

| Expression | Resolves to |
|---|---|
| `merge(doc, doc)`, `doc \|\| doc` | `(automerge, automerge)` |
| `merge(doc, $1::bytea)`, `doc \|\| $1::bytea`, typed bytea parameter `$1` | `(automerge, bytea)` |
| `merge(doc, '\x..')`, `doc \|\| '\x..'` (untyped literal) | `(automerge, automerge)` |
| `merge(doc, NULL)`, untyped parameter (`PREPARE p AS ... merge(doc, $1)`) | `(automerge, automerge)` |
| `doc::jsonb \|\| '{..}'` | `jsonb \|\| jsonb` |
| `automerge_contains(doc, doc)`, `automerge_contains(doc, '\x..')`, `automerge_contains(doc, NULL)` | `(automerge, automerge)` |
| `automerge_contains(doc, $1::bytea)`, typed bytea parameter `$1` | `(automerge, bytea)` |

An untyped literal is assumed to have the type of the other, known argument
(function resolution step 4.f; for operators, the exact-match check with the
unknown side taking the other side's type); `automerge` and `bytea` are both
in type category `U`, so no category preference applies earlier. A
consequence: bare change chunks written as a literal need `'\x..'::bytea`,
otherwise the automerge input function rejects them as orphaned.
`doc || '{"a": 1}'` means merge (and fails to parse the literal as hex);
write `doc::jsonb || '{"a": 1}'` for jsonb concatenation. `merge($1, $2)`
with two typed bytea parameters matches nothing (bytea → automerge is not
implicit).

### Reading

- `automerge_to_jsonb(automerge) → jsonb`: the cast function (see
  [jsonb mapping](#jsonb-mapping)). Each call loads the document (an
  expanded value is used in place).
- `automerge_spans(doc automerge, path text[] [, heads text[]]) → jsonb`:
  the structure of the text object at `path` (text runs with their marks,
  and blocks), as Automerge's JavaScript `spans()` returns it; see
  [Rich text spans](#rich-text-spans).
- `automerge_heads(automerge) → text[]`: current heads as sorted lowercase hex
  change hashes. Read from the header without loading the document (see
  [Heads fast path](#heads-fast-path)).
- `automerge_contains(a automerge, b automerge) → bool`: whether every change
  in `b` is already in `a`, meaning `merge(a, b)` would be a no-op. Decided
  from the two headers when `heads(b) ⊆ heads(a)` (true) or
  `heads(a) ⊊ heads(b)` (false: a head of `b` missing from `a`'s heads has
  no successor in `b`, so it cannot be an ancestor of a head of `a`);
  otherwise, for a stored `a`, from the two change counts (see below);
  otherwise `a` is loaded and `b` never is.
  - Change counts: when the heads do not decide and `b` has at least as
    many changes as `a`, the answer is `false`. Containment means
    `changes(b) ⊆ changes(a)`; with `|b| ≥ |a|` the two sets would be
    equal, and equal histories have equal heads, which the first test
    would have seen. Both counts come from a prefix of each value (the
    change actor column, as for `automerge_change_count`; see
    [Heads fast path](#heads-fast-path)), or from the change graph of an
    expanded value. This decides the two common `false` cases without a
    load: an older version asked whether it contains a newer one, and two
    concurrent versions with as many changes each (the same edit count on
    both sides of a fork). A stored count is exact: stored values are
    saves of a loaded document, one change actor entry per change.
- `automerge_contains(doc automerge, changes bytea) → bool` (`doc` is
  `a` below): whether `a` already has every change in `changes` (the same inputs as
  `merge(automerge, bytea)`), i.e. whether `merge(a, changes)` returns `a`
  unchanged. Empty `changes` is contained. Changes whose dependencies are
  in neither `a` nor `changes` are not in `a`: `false` (where `merge`
  raises `22P02`). Malformed bytes are `22P02` when they have to be
  loaded, and changes that conflict with `a`'s (a reused actor id) are
  `22000` when `a ++ changes` has to be loaded, as in `merge`.
  - No-load path, when `changes` is only uncompressed change chunks: each
    chunk's hash is `sha256(type ‖ uleb128 length ‖ data)` (its checksum
    is the first 4 bytes, and must match) and its data starts with its
    dependency hashes (`header::change_chunks`). `a` is a complete
    document, so a chunk whose hash is a head of `a` is in `a`, and a chunk
    with a dependency that is a head of `a` (or a chunk of the input
    already known to be missing) is not in `a`: a head has no successor in
    `a`. Any change known missing gives `false`, all known present `true`.
    That decides the two cases a backend produces: new changes made on top
    of the heads it last saw (false), and a re-send of the changes that
    made the current heads (true).
  - For an expanded `a` and bare change chunks, each chunk's hash is looked
    up in the loaded document.
  - For exactly one document chunk (a save) with a valid checksum:
    `false` when the save's header lists other heads and at least as many
    changes as a stored `a` has (by the change-count argument above: a
    save that loads has exactly the changes of its heads' history, one
    change actor entry each, and a save whose header disagrees with its
    content, or does not parse, is rejected by `merge`). Otherwise, when
    the chunk passes Automerge's chunk parse (as in `merge`'s case 1),
    whether `a` has the heads its header lists: from `a`'s heads, else
    from `a`'s history (a stored `a` is loaded; the save never is). That
    is what the no-op check of `merge(automerge, bytea)`, and a load of
    `a ++ changes`, decide. A save that does not parse is loaded as
    `a ++ changes`, which rejects it (`22P02`).
  - Otherwise (older changes, a save plus change chunks, compressed chunks)
    `a ++ changes` is loaded once (no save) and the heads compared.
  - On the no-load path only the framing, checksums and dependency lists
    are read, and a save's `false` comes from its header alone, so `false`
    does not promise that `merge` accepts the input. `true` does: a
    header `true` needs the chunk to parse, which is all a load of
    `a ++ changes` checks of it.

### Heads fast path

A stored value is one uncompressed document chunk, whose header lists the
heads (automerge `src/storage/document.rs`):

```text
magic 85 6f 4a 83 | checksum (4) | chunk type (1, 0 = document) | uleb128 data length
data: uleb128 actor count, per actor uleb128 length + bytes
      uleb128 head count, 32 bytes per head
      change/op column metadata and data, head indices
```

`pg_automerge_core::header` parses just that far. It is exact for stored
values: loading a document chunk (`VerificationMode::Check`, the default)
fails with "mismatching heads" unless the listed heads equal the ones
derived from its changes, and every stored value was loaded that way on its
way in. The checksum is not recomputed (that would hash the whole value):
stored values were checksum-verified when written. Anything that is not
exactly one document chunk (wrong magic or type, trailing chunks, lengths
that do not add up to the value's size) falls back to a full load.

The change count comes from the same place: after the heads come the
change and op column metadata (uleb128 count, then uleb128 spec and
length per column) and then the column data, change columns first. The
change actor column (spec `0x01`) has one RLE-encoded entry per change
(signed LEB128 run count: `n > 0` repeats the next value `n` times, `n < 0`
is followed by `-n` literal values, `0` is a null run), and Automerge's
loader takes the number of changes from its length and rejects documents
whose other change columns disagree. `header::change_count_from_prefix`
counts it; an absent column means no changes; a deflated or repeated
actor column, a null run, or anything that does not add up falls back to
a load.

`automerge_heads`, `automerge_contains`, `automerge_change_count`, the
since-functions of [History](#history-read-only) and the notification
trigger take their document without detoasting it (`AutomergeArg`; an
expanded value is read in memory): they read the value's size with
`toast_raw_datum_size` and fetch only a prefix (4 kB, grown as needed) with
`pg_detoast_datum_slice`, which for an out-of-line value reads only the
TOAST chunks covering it and for a compressed value decompresses only that
far. `merge` and `merge_agg` also decide their no-op cases from such a
prefix before they detoast a value (see [Merging](#merging)).

Deciding containment for a linear history (newer contains older) still
needs a load of the newer document: the headers only list heads, and
change hashes are only available after reconstructing the changes. The
opposite answer (an older or a concurrent version does not contain the
other) usually needs none: `automerge_contains` compares the two change
counts, read from the same prefix (see [Reading](#reading)).

### History (read-only)

Automerge keeps a document as an op set plus a change graph: hash, actor,
seq, op range, time, message and deps of every change. Change *bytes* are
not stored; Automerge rebuilds them from the op set on request. So the
metadata functions cost one load, while the functions returning change
bytes cost a load plus re-encoding those changes (about as much again for
all changes of a document). Nothing here creates changes.

Two composite types describe a change:

```sql
CREATE TYPE automerge_change AS (
    hash text,          -- 64 lowercase hex digits
    actor text,         -- lowercase hex
    seq bigint,         -- 1, 2, ... per actor
    start_op bigint,    -- counter of the change's first op
    op_count bigint,    -- number of ops (0 for an empty change)
    "time" timestamptz, -- NULL when not set (0)
    message text,       -- NULL when not set
    deps text[],        -- sorted hashes of its dependencies
    change bytea        -- the change chunk (uncompressed, as save_after emits it)
);
-- automerge_change_meta: the same without `change`.
```

- `automerge_changes(doc automerge, since_heads text[] DEFAULT '{}') →
  SETOF automerge_change`: every change not reachable from `since_heads`
  (all changes for `'{}'`), in causal order: each change comes after its
  dependencies, and otherwise in Automerge's own order (which already is
  causal, so it is kept as is). Rebuilds the bytes of the returned changes.
- `automerge_changes_meta(doc, since_heads text[] DEFAULT '{}') → SETOF
  automerge_change_meta`: the same rows from the change graph only
  (`get_changes_meta`); no change is rebuilt. Use it whenever the bytes are
  not needed.
- `automerge_changes_bytes(doc, since_heads text[] DEFAULT '{}') → bytea`:
  those changes' chunks concatenated in the same causal order, i.e.
  Automerge's `save_after(since_heads)`. A replica at `since_heads` loads it
  with `loadIncremental` / `applyChanges`, and `merge(replica,
  automerge_changes_bytes(full, automerge_heads(replica)))` has the heads
  and jsonb of `full` (tested). Empty when there is nothing new.
- `automerge_get_change(doc, hash text) → automerge_change`: one change with
  its bytes, or NULL if the document does not have it.
- `automerge_change_count(doc) → bigint`: number of changes. Read from a
  prefix of the stored value (see [Heads fast path](#heads-fast-path)),
  without loading it.
- `automerge_to_jsonb(doc automerge, heads text[]) → jsonb`: the state as of
  `heads` (Automerge's `*_at` reads), with the same mapping as the cast.
  `'{}'` is the state before any change (`{}`); the current heads give the
  current state. Every hash must be a change of the document, otherwise
  `22023` (`automerge document does not contain change <hash>`).

"Since heads" semantics are Automerge's `get_changes(have_deps)`: the
changes that are neither in `since_heads` nor ancestors of them.
**Hashes the document does not have are ignored**, as Automerge does: a
replica that is ahead of the stored row (it has changes the row lacks)
gets everything that is not an ancestor of the heads the row knows, a
superset of what it is missing and never less, which it deduplicates on
load. (An error would make that sync case fail; a typo in a hash also
just returns more changes.) Automerge computes the set from the per-actor
sequence numbers of the known heads' ancestors; for histories written by
Automerge (each change depends on its actor's previous change) that is
exactly the set of non-ancestors, which a property test checks against a
walk of the dependency graph. `automerge_to_jsonb(doc, heads)` instead
rejects unknown hashes, since it cannot show a state it does not have.
When every current head of `doc` is in `since_heads`, the since-functions
return nothing without loading (heads read from the header).

Hash arguments must be exactly 64 hex digits, either case (`22P02`
`invalid automerge change hash "..."` otherwise); a NULL array element is
`22004` (`since_heads must not contain NULL`).

Time: Automerge change times are Unix **seconds** (as documented for the
Rust crate's `CommitOptions::with_time`; check what your producer writes).
`time` is NULL when the change has none (0) and when Postgres cannot
represent it; a producer writing milliseconds shows up as a date tens of
thousands of years ahead.

Set-returning functions compute all rows on the first call: the document
is loaded, the rows are copied into a Rust `Vec` and the document is
dropped before the first row is returned. pgrx keeps that `Vec` in the
SRF's multi-call memory context, which Postgres deletes when the scan ends
or is cut short (`LIMIT`, a closed cursor, an error), dropping it; tested
with `LIMIT` on a target-list SRF, closed cursors and errors mid-scan.
Memory: for `automerge_changes` the `Vec` holds the change bytes (about the
size of the document's history) until the scan ends; in `FROM`, Postgres
additionally materializes the rows in a tuplestore (spilling to disk past
`work_mem`). Rows are built with the function's declared result type
(looked up by the function's OID), so they do not depend on `search_path`.
The descriptor each row is built with is checked first: exactly the
attributes the extension created, none dropped, of exactly their types
(text, text, bigint ×3, timestamptz, text, text[], and bytea). The types'
owner can `ALTER TYPE automerge_change ALTER ATTRIBUTE seq TYPE text`, and
`heap_form_tuple` would then read the `bigint` datum as a text pointer and
crash the backend; an altered type is `XX000` (`type automerge_change has
been altered: its attributes must be those created by extension
pg_automerge`) instead.

### Change notifications

Backends keep their in-memory replicas in sync by listening for changes
other backends persisted, then fetching only what they lack:

```sql
CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id' [, more key columns]);
```

`automerge_notify()` runs `NOTIFY <channel>` (`Async_Notify`, what
`pg_notify` calls) with a compact JSON payload:

```json
{"table":"public.docs","op":"UPDATE","seq":17,"key":{"id":1},
 "columns":{"doc":{"heads":["79df.."],"prev_heads":["891e.."]}}}
```

- `table`: schema-qualified, each part quoted as needed
  (`public."Odd Name"`). For a partitioned table this is the partition
  holding the row (row triggers run on the partitions).
- `op`: `INSERT`, `UPDATE` or `DELETE`.
- `seq`: a per-backend counter, incremented for every notification the
  sending backend's trigger sends (all channels and tables; numbers of
  rolled-back work are skipped, so there are gaps). It exists to make every
  payload distinct: `NOTIFY` silently drops a notification whose channel
  and payload equal an earlier one of the same transaction, which without
  `seq` would lose events a listener needs, e.g. the second INSERT of
  INSERT, DELETE, INSERT of the same row in one transaction (the listener
  would drop a replica of a row that exists), or the third UPDATE of a key
  changing 1 → 2 → 1 → 2. Within a transaction `seq` increases in firing
  order. It is not a global order: another backend (or this one after a
  reconnect, as a new process) counts separately, and the numbers are not
  comparable across senders. Kept in every truncated form.
- `key`: the key columns named in the trigger arguments, in that order,
  each as `to_json(value)` (numbers stay numbers, uuids and text are
  strings, NULL is `null`); from the new row, or the old row for DELETE.
  `old_key`: the old row's key, only for an UPDATE that changed it.
- `columns`: every column of type `automerge` (or a domain over it):
  INSERT lists all with `heads`, DELETE all with `prev_heads`, UPDATE only
  those whose heads changed, with both. Heads are sorted hex hashes as in
  `automerge_heads`; `null` for a NULL value (`[]` is the empty document).
- `truncated: true`: see below.

When it fires: INSERT and DELETE always notify. An UPDATE notifies only if
the heads of some `automerge` column changed, or the key did (a listener
tracks rows by key). No-op merges (`merge` returning the stored value),
`SET doc = doc`, and updates of other columns do not notify. Change is
decided by heads, never by bytes: `merge(a, b)` and `merge(b, a)` may
differ in bytes with the same heads. An unchanged value whose raw datum is
identical (same inline bytes or same TOAST pointer, e.g. an update of
another column) is skipped without reading it; otherwise both heads are
read from a prefix of the stored values as in `automerge_heads` (4 kB,
only the TOAST chunks covering it), never a full load. An UPDATE that
changed no heads and whose key columns are raw-identical returns before
building any JSON.

Delivery is Postgres' `NOTIFY`: sent at commit (nothing for a rolled-back
transaction), in commit order, only to sessions connected and listening at
the time; notifications with identical channel and payload within one
transaction are collapsed, which `seq` prevents for this trigger's
notifications (so every row event of a committed transaction arrives, in
the order it fired). Still, a listener must treat notifications as hints:

1. On connect (and reconnect), `LISTEN docs_changed` first, then resync
   every replica it holds: `SELECT automerge_changes_bytes(doc,
   $replica_heads) FROM docs WHERE id = $1` and apply the result.
2. On a notification for a row it holds: if the payload's `heads` are all
   in its replica already (e.g. it wrote them), skip; otherwise fetch
   `automerge_changes_bytes(doc, $replica_heads)` for that key. The row
   may have moved on since the notification; that just returns more.
   `automerge_changes_bytes` with heads the row already has returns
   nothing without loading the document, so fetching on every
   notification (and on `truncated` ones) is cheap when there is nothing
   new.
3. DELETE: drop the replica. `old_key`: re-key it. Apply events in the
   order they arrive; a DELETE later followed by an INSERT of the same key
   arrives as both.

Payload size: `NOTIFY` rejects payloads of 8000 bytes or more
(`NOTIFY_PAYLOAD_MAX_LENGTH`); at 67 bytes per hash that is about 115 heads,
counting `heads` and `prev_heads` of all columns. The trigger
never fails a write because of that: it renders the full payload, and if
it has more than 7999 bytes drops, in this order, until it fits: the heads
(`"columns":{"doc":{}}`, column names kept), the `columns` object, the
`key`/`old_key`. Any dropped part adds `"truncated":true`; listeners then
fetch by key (or resync the table when the key is gone too, which needs a
key of several kB). Tested with a 150-head document.

Argument checks, when the trigger fires (Postgres does not validate
trigger arguments at `CREATE TRIGGER`): not `AFTER` or not `FOR EACH ROW` →
`39P01` (trigger protocol violated), and so is calling
`automerge_notify()` outside a trigger; fewer than two arguments, an empty
channel or one of 64 bytes or more, a key column listed twice, of type
`automerge` or a virtual generated column → `22023`; an unknown key column
→ `42703`. A virtual generated column (PG18) is computed when read and not
stored, so the rows an AFTER trigger sees hold NULL in it; reporting that
NULL as the key would leave a listener unable to find the row, so it is
refused (HINT: name a STORED generated column or the columns it is
computed from). A STORED generated column is computed before AFTER
triggers fire and works as a key. Errors about how
the trigger is declared carry a HINT with the correct `CREATE TRIGGER`
(an `automerge` key column, a HINT on which columns to name). These
errors fail the write, as trigger errors do. The channel is used verbatim,
like `pg_notify`: `LISTEN` folds unquoted names to lower case, so use a
lower-case channel (or `LISTEN "Name"`). The `automerge` type is looked up
in the function's own schema, so the trigger does not depend on
`search_path`.

Labels: `VOLATILE`, not `STRICT`, `PARALLEL UNSAFE` (the defaults; it sends
notifications and triggers never run in parallel workers).

Why a C (Rust) trigger and not PL/pgSQL: PL/pgSQL would have to find the
`automerge` columns in the catalog and read each through dynamic SQL
(`EXECUTE format('SELECT automerge_heads($1.%I)', col) USING NEW`) per row;
its `to_jsonb(NEW)` would convert every document to jsonb (a full load
each). The Rust trigger reads the raw datums (so an unchanged TOAST pointer
costs nothing and heads come from a prefix), builds the payload in
`pg_automerge_core::notify` (plain Rust, unit-tested including the size
limit and escaping) and gets the key values' JSON from Postgres' own
`to_json` machinery (`json_categorize_type` / `datum_to_json`, exported by
the server though not in pgrx's bindings). It is written like pgrx's
`#[pg_trigger]` expansion (V1 info record, guarded entry point, SQL in an
`extension_sql!` block) so that a call outside a trigger is a clean error.

#### Skipping no-op writes

A no-op `UPDATE docs SET doc = merge(doc, $1)` still writes a new row
version (with WAL and index entries unless it is a HOT update; a TOASTed
document is not rewritten, since `merge` returns the stored TOAST pointer
unchanged) and fires triggers (`automerge_notify` stays silent, since the
heads are unchanged).
To skip it:

```sql
UPDATE docs SET doc = merge(doc, $1) WHERE id = $2 AND NOT automerge_contains(doc, $1);
```

`$1` is the same bytea parameter in both places (`automerge_contains(automerge,
bytea)`; for a full save cast to `automerge` in both). Under concurrency this
stays correct in READ COMMITTED: the row qualifies against the statement's
snapshot; if a concurrent transaction updated it, the UPDATE waits for its
row lock, and once that transaction commits Postgres re-evaluates the whole
`WHERE` clause (EvalPlanQual) and the `SET` expression against the newly
committed row version. If that version already contains the changes, the
row is skipped (UPDATE 0, nothing lost: the changes are there); otherwise
`merge` applies them to that version. If the row fails the check against
the snapshot, it is skipped without waiting: history only grows under
`merge`, so a later version contains the changes too. That argument
assumes every writer merges; a plain `SET doc = ...` that replaces history
breaks it (and loses updates anyway). Tested with two blocking sessions in
`tests/concurrency.sh` (same changes: B updates no row; different changes:
B merges into A's version). REPEATABLE READ / SERIALIZABLE raise a
serialization error instead, as for any concurrent update.

UPDATE 0 means "row missing or nothing new"; check the row's existence
separately if the difference matters. For bare changes the check usually
needs no load (new changes built on the current heads, and re-sends of the
latest changes, are decided from the chunk hashes); for a full save it
loads the stored document, so a real change then costs one more load than
the plain `merge`.

### Expanded values

Postgres lets a varlena type have an in-memory "expanded" form
(`utils/expandeddatum.h`): a function returns a pointer to an object that
stays in memory, and the flat bytes are produced only when the value is
stored, sent or copied. For `automerge` the expanded form is the loaded
Automerge document, so chains of operations do not pay a save and a
re-load between steps.

Where it helps:

- `merge(merge(a, b), c)`, `a || b || c`: the inner result stays loaded; the
  outer merge applies into it in place.
- `merge(...)::jsonb`, `automerge_heads(merge(...))` and the history
  functions on a merge result read the document in memory (no save, no
  re-load).
- PL/pgSQL `d := merge(d, x)` in a loop: the variable holds the expanded
  document, merged into in place; the save (and its verification) happens
  once, when `d` is stored.
- Repeated reads of such a variable (`d->>'a'`, `d->>'b'`) convert from
  memory instead of loading each time. A variable filled from a table
  (`SELECT doc INTO d`) is flat until the first `merge` into it (PL/pgSQL
  expands only arrays by itself).
- `merge_agg(...)::jsonb` skips the final save and re-load (the inputs
  still need a load each).
- The `bytea → automerge` cast returns the document it loaded to validate
  the input, with its stored bytes already computed: `merge(doc,
  $1::automerge)` (or `merge(doc, (SELECT ..)::automerge)`) reads it in
  memory, so a newer full save costs one load in total instead of two, and
  storing the cast's value copies the bytes (no save).

Where it does not:

- Table columns always arrive flat, so
  `UPDATE docs SET doc = merge(doc, $1)` loads the stored value, applies,
  saves and verifies once (nothing to gain without a cross-statement cache).
- `EXCLUDED` in `INSERT .. ON CONFLICT DO UPDATE` is flat: `ExecInsert`
  materializes the proposed row (`ExecMaterializeSlot`, which flattens
  expanded values) before it checks for the conflict, and nothing in a
  type's control changes that. So the upsert of a full save costs two
  loads (the cast's, and `merge` loading `EXCLUDED.doc`), where
  `UPDATE .. SET doc = merge(doc, $save::bytea)` costs one.
- Query parameters are flat in the plans drivers normally get: for an
  unnamed statement (and the first five executions of a prepared one)
  Postgres plans with the parameter values as constants, and folding
  `$1::automerge` or a typed `automerge` parameter into a constant copies
  it flat (`datumCopy`). So `merge(doc, $1::automerge)` costs two loads
  there (one with a generic plan); `merge(doc, $1)` with a `bytea`
  parameter costs one either way (see `merge(automerge, bytea)`).
- The type's input and receive functions return flat values. Their
  results are copied flat almost everywhere (parser constants are
  detoasted, `COPY` stores them, parameters are folded as above), and an
  expanded value would keep its loaded document, many times the size of
  the bytes, alive until the calling memory context ends: for every
  element of an `automerge[]` literal or binary array parameter, or of a
  record, until the end of the statement. The `bytea` cast runs in an
  expression's per-row context instead, which is reset for every row (a
  pg_test checks that 400 rows of `INSERT .. SELECT` through the cast
  leave no document behind, and that at most one is alive at a time).

#### Implementation

- Object: `ExpandedAutomerge` (`src/expanded.rs`), the standard
  `ExpandedObjectHeader` followed by a pointer to a Rust
  `pg_automerge_core::loaded::LoadedDoc` (the `Automerge` document, its
  sorted heads, and its stored bytes once computed). It lives in its own
  memory context (`automerge expanded document`, a child of the calling
  context), so Postgres frees or re-parents it with the value
  (`TransferExpandedObject` when PL/pgSQL keeps it in a variable). The
  Rust document is dropped by a reset callback registered on that
  context, so it lives exactly as long as the object. The Rust heap
  memory is not counted in the context (as for `merge_agg`'s state);
  `automerge_memory_usage()` shows it (see
  [Memory observability](#memory-observability)).
- Flattening: `get_flat_size` computes the stored bytes
  (`save_nocompress()`), caches them in the `LoadedDoc` and returns their
  size; `flatten_into` copies the cached bytes (and checks the size it is
  given). The bytes are the same as the flat path's, byte for byte (a
  property test runs chains of merges both ways over generated histories:
  a document chunk lists changes in the order they were applied, and both
  paths apply them in the same order).
- The cache cannot go stale: a `LoadedDoc` is never modified. "In place"
  means the object's `LoadedDoc` is *replaced* by a new one (built from a
  clone of the old document plus the new changes, with an empty cache).
- Read-only vs read-write: only a read-write pointer
  (`VARTAG_EXPANDED_RW`) allows the replacement. With a read-only pointer
  (a variable passed to another target, a value in a slot, the other
  arguments of an in-place call) the function returns a new object and
  never touches the old one. New objects are returned read-write, as the
  protocol requires.
- Failure atomicity: the new document is complete before it replaces the
  old one, and nothing after the replacement can fail. An error (bad
  changes, missing dependencies, a reused actor id, a decoder panic) leaves
  the argument's document as it was. This is condition 1 of
  `SupportRequestModifyInPlace`, and it holds for every read-write call,
  not just PL/pgSQL's.
- `merge(x, x)` through a read-write and a read-only pointer to the same
  object (`d := merge(d, d)`, `d := d || d`): recognized by object identity
  and returned unchanged, before any document is borrowed.
  `d := merge(d, d::bytea)` reads a flattened copy (argument unboxing
  happens before the call). This is condition 2 of the support request.
- Support function: `automerge_merge_support(internal)` is attached to both
  `merge`s (and so to both `||`) with `SUPPORT`. For
  `SupportRequestModifyInPlace` it names the first argument when it is the
  assignment target's `PARAM_EXTERN` Param, otherwise NULL (also for every
  other request). PL/pgSQL then passes the variable read-write even when
  it is declared outside a `BEGIN .. EXCEPTION` block, where its value must
  survive an error; for a local variable referenced once PL/pgSQL 18
  already transfers ownership into the expression by itself. Both paths
  are tested (a test-only counter of in-place replacements).
- Inputs: every function taking `automerge` accepts both forms
  (`AutomergeArg`); the read functions (heads, jsonb, history,
  containment, change count) use an expanded argument's document in
  memory and never flatten it. Output, `send` and casts flatten it (once;
  the bytes are cached). Functions that receive `automerge` as `bytea`
  (`doc::bytea` is binary-coercible) get the flattened bytes through the
  normal detoast.
- No-op results: an input that already contains the other is returned as
  is, the datum itself: a flat input as it arrived (possibly compressed or
  a TOAST pointer), an expanded input as the same pointer, the way
  `COALESCE` passes an argument through (whoever keeps the result beyond
  the expression copies it: slots, PL/pgSQL assignment and SQL function
  results flatten a read-only pointer; storing a TOAST pointer of the same
  row keeps its TOAST value).
- `merge(automerge, bytea)` on an expanded document parses the change
  chunks (`Change::try_from`, the parser `Automerge::load` uses) and
  applies them to a clone, so it needs no load; a save is loaded on its
  own and merged like a document (see [Merging](#merging)); compressed
  chunks go through a strict load of a fresh save of the document plus
  the input.
- Sources of expanded values: `merge` and `||` results, `merge_agg`
  results, and the `bytea → automerge` cast (whose object starts out
  verified, with its stored bytes). Input and receive functions return
  flat values (see above).

#### The deferred verification

A result that contains changes from a `bytea` is marked unverified. Its
save is loaded back and must keep the heads (the safeguard of
normalization) when it is first flattened, not inside `merge`. So such a
value can never be stored unless it loads back, but the rare input that
loads, applies, and then does not survive a save and load (seen only in
fuzzing, as malformed input with recomputed checksums) fails with `22P02`
where the value is stored, sent or cast, e.g. at the `UPDATE`, not at the
`merge` call; a `BEGIN .. EXCEPTION` block around only the `merge` does
not catch it. Every other bad input (framing, checksums, change columns,
missing dependencies, a reused actor id, decoder panics) fails inside
`merge`.
Verifying eagerly would cost a load per merge and undo most of the gain.
The check runs only while `pg_automerge.verify_writes` is on (read when
the value is flattened; see [The `pg_automerge.verify_writes`
setting](#the-pg_automergeverify_writes-setting)). A save merged with
`merge(automerge, bytea)` that contains `a` and is its own canonical or
compressed encoding is the result as it is, verified by that encoding,
and needs no check.

The failure path is tested with a test-only hook
(`test_hooks::set_fail_reload_check`, feature `test-hooks`) that makes the
check fail: `merge(...)` and reads of its result succeed and run no check;
`UPDATE`/`INSERT` of it, `::bytea` and `automerge_send` fail with `22P02`;
the row is unchanged; a PL/pgSQL variable holding the result stays usable
after the failed `UPDATE` and stores fine once the check passes; on input,
canonical bytes and compressed saves take no check while other input
fails. The setting is tested the same way (`src/tests/loads.rs`): off, the
forced failure never runs and a real input whose re-save does not load is
stored and then fails to read with `XX000`; only superusers may change
it.

#### Pitfalls avoided (supabase/pg_crdt's expanded automerge)

1. Mutating through a read-only pointer: only `VARTAG_EXPANDED_RW` allows
   replacement; tested with a PL/pgSQL variable passed read-only to two
   merges and with an alias (`x := d; d := merge(d, y)` leaves `x`).
2. Stale flat bytes: the cache belongs to an immutable `LoadedDoc`, and a
   merge replaces the whole `LoadedDoc`; tested by storing, merging in
   place and storing again (and in the core tests).
3. Support function not attached: `SUPPORT automerge_merge_support` is in
   the generated `CREATE FUNCTION`s; a pg_test checks `pg_proc.prosupport`
   and that PL/pgSQL's in-place path is taken inside an `EXCEPTION` block.
4. Document memory outside the object's lifecycle: the document is owned
   by the object and dropped by its context's reset callback; a pg_test
   runs about 2,400 merges in PL/pgSQL loops and checks that
   `pg_backend_memory_contexts` shows at most the live variables' objects
   during the loop and none after, and that the number of live Rust
   documents returns to where it was.

### Naming

The SQL surface was reviewed as a whole before the first release (0.1.0;
until then renames needed no aliases or upgrade script, now every change
needs a version and an upgrade script, see [Versioning and
upgrades](#versioning-and-upgrades)). Principles:
objects that cannot collide because they take an `automerge` argument may
use generic names where that reads better (`merge`, `merge_agg`, `||`);
everything else carries the `automerge_` prefix; parameter names are the
same for the same role, so named-argument calls read consistently (`doc`
for the document, `changes` for bytes, `since_heads`/`heads`, `hash`).

Renamed:

- `merge_agg_trans` / `merge_agg_final` → `automerge_merge_agg_trans` /
  `automerge_merge_agg_final`. `merge_agg_final(internal)` has no
  extension type in its signature, so another extension (or a user
  function) of that name in the same schema would make `CREATE EXTENSION`
  fail, or ours block theirs; both support functions also show up in
  `\df`. The prefix removes the clash; the aggregate keeps its name.
- The first parameter of `merge(automerge, bytea)` and
  `automerge_contains(automerge, bytea)`: `a` → `doc`, as in every other
  function whose first argument is the document the bytes apply to
  (`merge(doc => d, changes => c)`). `merge(a, b)` and
  `automerge_contains(a, b)` keep `a`, `b`: a pair of documents.

Considered and kept:

- `merge`, `||`: required, and keyed on the `automerge` type (`merge` is an
  unreserved keyword; unqualified calls work, tested).
- `merge_agg` rather than `automerge_merge_agg`: Postgres identifies an
  aggregate by name and argument types, so another extension's
  `merge_agg(hll)` or `merge_agg(anyelement)` coexists with ours in the
  same schema, and a call on an `automerge` argument resolves to ours (an
  exact match beats a polymorphic or cast candidate). It pairs with `merge`
  the way `string_agg`, `json_object_agg` pair with their scalar forms.
- `automerge_changes` / `automerge_changes_meta` / `automerge_changes_bytes`:
  one family with the same `since_heads` parameter. `automerge_save_after`
  (Automerge's name) for the last was rejected: it would split the family,
  and "save" suggests a document, while the result is change chunks.
- `automerge_get_change` rather than `automerge_change(doc, hash)`: a
  function named like the composite type `automerge_change` would read as
  the function-call form of a cast to that type.
- `automerge_change_count`, `automerge_heads`, `automerge_contains`,
  `automerge_notify` (trigger functions are named for what they do, like
  `pg_notify`), `automerge_from_bytea` (the cast function),
  `automerge_merge_support`.
- `automerge_to_jsonb(doc, heads)` as an overload of the cast function
  rather than `automerge_to_jsonb_at`: same result, "the jsonb of `doc`",
  as of `heads`; the cast is bound by OID, so the overload cannot change
  what the cast calls, and `heads => '{}'` reads well in named form.
- The composite types' columns (`hash`, `actor`, `seq`, `start_op`,
  `op_count`, `time`, `message`, `deps`, `change`): Automerge's own field
  names in snake case. `time` is a non-reserved keyword and works unquoted
  as a column (`SELECT seq, time FROM ...`, in the regress examples);
  `change` (the change's bytes, for `decodeChange` / `applyChanges`) was
  kept over `bytes`, which names a representation, not what it is.
- The setting `pg_automerge.verify_writes`: custom settings are prefixed
  with the extension name.

## jsonb mapping

The root is always a map, so it maps to a jsonb object. For a map key with
conflicting concurrent values, Automerge's winner is used (what `get` returns).

| Automerge | jsonb |
|---|---|
| Map / Table | object |
| List | array |
| Text | string (`doc.text(obj)`; block markers are included as the characters Automerge's `text()` returns) |
| Str | string |
| (any string or map key containing U+0000) | U+0000 replaced by U+FFFD (Postgres text and jsonb cannot hold NUL) |
| Int, Uint, Counter | number (exact). A counter summing past the i64 range wraps, as in Automerge release builds |
| F64 | number. NaN/±Inf become `null` (jsonb cannot represent them); -0.0 becomes `0` (jsonb numeric has no negative zero) |
| Boolean | boolean |
| Null | null |
| Timestamp (ms since epoch) | string, ISO 8601 UTC with milliseconds, e.g. `"2024-01-02T03:04:05.678Z"`; years outside 0000–9999 use the six-digit signed form (`"+010000-01-01T00:00:00.000Z"`), like JavaScript's `toISOString` |
| Bytes | string, standard base64 with padding |
| Unknown | null |

The conversion builds the jsonb value directly: the core's walk
(`json::write_json_at`) emits events into a `JsonSink`, and the glue's
`JsonbBuilder` assembles from them the in-memory `JsonbValue` tree that
`jsonb_in` builds while parsing (the tree `pushJsonbValue` accumulates) and
finishes with one `JsonbValueToJsonb`, with the same values: strings and
keys with `jsonb_in`'s 256 MB string limit, `i64` through
`int64_to_numeric`, and `u64` above `i64::MAX` and floats through
`numeric_in` on the text `serde_json` writes for them (the shortest
round-trip form), exactly what the text path parsed. The tree is assembled
in Rust memory rather than through `pushJsonbValue`, which cost one
guarded FFI call and a few pallocs per event and doubled its arrays as they
grew: each container's elements or pairs are allocated once at their final
size, string bytes are copied into a chunked arena whose chunks never move,
and object pairs are sorted and de-duplicated as `uniqueifyJsonbObject`
does (by key length, then bytes; of equal keys the one added last wins),
with `pushJsonbValue`'s element and pair limits and errors. That memory is
freed when the builder is dropped, also when an error unwinds through it.
So the result is byte for byte what the earlier `serde_json::Value` → text
→ `jsonb_in` path produced; pg_tests compare the two on every scalar edge
case, conflicts, duplicate keys after NUL replacement, historical states,
generated documents, and wide objects with keys of every length, arrays
past jsonb's offset stride and strings larger than an arena chunk.
Postgres errors inside those calls unwind through the walk untouched (see
[Errors and panics](#errors-and-panics)).

The walk makes one sweep over the document with Automerge's document
iterator (`ReadDoc::iter_at`), unless the document has blocks (maps
inside a text; see [Deep blocks](#deep-blocks)), buffering each object's visible entries
(borrowed from the document), then emits them depth first from the root.
The entries of all maps and lists go to one vector (the iterator yields an
object's items together, so each object's entries are one range of it),
and an object is found by bisecting its id in an index of the visited ids
ordered by `ObjId`'s own order (counter, then actor bytes), not by hashing
it: Automerge visits objects in that order (its actor table is kept
sorted), so building the index costs one pass that checks the order, and
sorts only if it ever does not hold. One sweep avoids setting up a `map_range`/`list_range` iterator per object
(about 5 µs each). Text comes from the sweep's spans: string runs, and
U+FFFC for each block marker, which is exactly what `text()` returns (both
emit U+FFFC for anything in a text that is not a string and nothing for
marks). The per-object walk (`json::write_json_per_object`: a
`map_range` / `list_range` per object, `text()` per text) is taken for
documents with blocks, which the document iterator renders recursively
(see [Deep blocks](#deep-blocks)), and for documents with legacy `Table`
objects, which the sweep would read as lists; it is also the reference:
a core test checks that both walks, and the choice between them, emit
exactly the same events for current and historical states of generated
documents, text with blocks, marks and non-string elements, unreachable
objects, conflicts and deep nesting.

Nesting is capped at 1000 levels (error `54000` program_limit_exceeded,
like jsonb's own size limits): the walk itself uses an explicit stack, but
`convertToJsonb` recurses (it checks the stack depth, the cap gives a
clear error instead). The document is valid and can still be stored,
merged and read as `bytea`; only its jsonb view fails.

### Rich text spans

A text object maps to a plain string, so formatting (marks) and block
markers (paragraphs, headings, list items, embeds: maps inserted into the
text with `splitBlock`) are invisible in the jsonb view: marks leave no
trace and each block shows up as U+FFFC. `automerge_spans` returns that
structure:

```sql
automerge_spans(doc automerge, path text[]) → jsonb
automerge_spans(doc automerge, path text[], heads text[]) → jsonb  -- as of heads
```

Both are `IMMUTABLE STRICT PARALLEL SAFE`. The result mirrors the
Automerge JavaScript API's `spans(doc, path)`, so frontends and backends
can use it as they use `spans()`:

```json
[{"type": "block", "value": {"type": "heading", "parents": [], "attrs": {"level": 1}}},
 {"type": "text", "value": "Shopping tips"},
 {"type": "block", "value": {"type": "paragraph", "parents": [], "attrs": {}}},
 {"type": "text", "value": "Buy "},
 {"type": "text", "value": "fresh milk", "marks": {"bold": true}},
 {"type": "text", "value": "."}]
```

**Path.** As jsonb's `#>` takes it: from the root, map keys as strings
and list indices as integers (`'{notes,0,body}'`; negative indices count
from the end, and an index is parsed as `#>` parses it: optional leading
whitespace and sign, `int4` range, nothing after the digits). `'{}'` is
the root. Where `#>` would return NULL, so does `automerge_spans`: a
missing key, an index out of range or not an integer, a step into a
scalar or into a text (jsonb shows it as a string), a NULL path element.
A map key with conflicting values uses Automerge's winner, as the jsonb
view does. When the value at the path exists but is not a text object
(the root, any map, table, list or scalar, including a string scalar,
which jsonb also shows as a string), it is `22023` naming the path and
what is there: `automerge value at path {notes,0} is a map, not a text
object`.

**Heads.** The three-argument form reads the state as of `heads` with the
semantics and errors of `automerge_to_jsonb(doc, heads)` (see
[History](#history-read-only)): every hash must be a change of the
document (`22023`), `'{}'` is the state before any change (whose root is
empty, so any non-empty path gives NULL), malformed hashes are `22P02` and
NULL elements `22004`.

**Shape**, checked against the JavaScript implementation at the tag
`js/automerge-3.5.0` (whose `rust/automerge` is 0.12.0, the version the
extension pins): `spans()` in `javascript/src/implementation.ts` returns
the WASM `spans(obj, heads)` as is, and `export_span` in
`rust/automerge-wasm/src/interop.rs` builds each element from Automerge's
`Span`:

- `Span::Text` → `{"type": "text", "value": <text>}` plus `"marks": {name:
  value, ...}` when the mark set is non-empty (no `"marks"` key otherwise,
  never an empty object).
- `Span::Block` → `{"type": "block", "value": <the block's map>}`. The
  value is the block map exactly as stored (JS: `export_hydrate` of the
  hydrated map). Automerge adds nothing to it: the `type`, `parents`,
  `attrs` and `isEmbed` fields are a convention of editors such as
  automerge-prosemirror (JS `splitBlock(doc, path, index, block)` stores
  whatever map it is given), so they appear only if the writer stored
  them.
- Runs come from Automerge's own span iterator (`ReadDoc::spans` /
  `spans_at`), which the JS API uses too, so the split is identical: a
  new run wherever the set of marks changes; adjacent text with the same
  marks is one run; a block ends the run before it, so text is split at
  every block (the result is flat: blocks do not contain their text,
  they precede it).
- Marks whose value is null are removals (`unmark`, or a mark set to
  null), and Automerge's span iterator already drops them from each run's
  mark set: a run with only removed marks has no `"marks"`, and a null
  mark value never appears. Expand flags (`before`/`after`/`both`/`none`)
  only decide which inserted text a mark covers; they are not part of the
  output (nor of JS's).
- Other non-string elements in a text (a list, a nested text, a number
  inserted with `insert`) are not blocks: like `text()`, the span
  iterator puts U+FFFC for them in the surrounding text run. A literal
  U+FFFC typed by a user is text too.

Deliberate deviations, all from the jsonb scalar mapping (see the table
above), used for mark values and block contents, compared with what
`alloc_scalar` / `export_hydrate` give in JS:

- integers (int, uint, counter) are exact JSON numbers. JS gives a
  `number` for an int or counter within ±(2^53−1) and a `BigInt` outside
  it (which `JSON.stringify` refuses); a uint is always a `BigInt` there
  (automerge-wasm's safe range for uints, `0..MIN_SAFE_INTEGER as u64`,
  is empty);
- counters are their current value. JS gives a `Counter` object in block
  values (`registerDatatypes` wraps them; its JSON is the value) and a
  plain number (or `BigInt`) in mark values, which are not wrapped;
- NaN/±Infinity are `null`, timestamps ISO 8601 strings (JS: `Date`),
  bytes base64 strings (JS: `Uint8Array`), U+0000 in strings and keys
  (mark names too) U+FFFD.

Not deviations: strings in block values are `ImmutableString` objects in
JS (`registerDatatypes` wraps `str` too), whose JSON is the plain string,
as here; mark values that are strings are plain strings in both. Text
inside a block's map is a string, as in JS; conflicting keys in a block
show the winner, as in JS.

**Positions.** The result has none (neither does JS's), so no text
encoding is involved in it: a client that needs offsets sums the lengths
of the `value`s in its own unit, counting each block as one U+FFFC
character (JavaScript's `.length`, UTF-16 code units, gives exactly the
indices of the JS API, whose documents use UTF-16; U+FFFC is one code
point and one UTF-16 unit, three UTF-8 bytes).

**Protections.** The document is loaded as for any read (`with_doc`: an
expanded value in place, a stored one loaded; reading stored values is not
subject to `pg_automerge.max_load_memory`), under the panic guard (a panic
is `XX000` for a stored value). The span loop and the block walks check
for interrupts. The result is built with the same `JsonbBuilder` as the
cast (jsonb's string, element and pair limits, `54000`).

Automerge renders a block's value with `hydrate`, which recurses once per
level of nesting inside the block and needs about 13 kB of stack per
level in a release build: a block holding maps nested a few hundred
levels deep (a document of about 2 kB) overflows the backend's 8 MB stack
inside `spans()`, a crash (a segfault restarts the cluster) that no guard
can catch (see [Deep blocks](#deep-blocks) for the other paths). So before `spans()` is called, every block of the text is
found without hydrating (one `list_range` over the text, as of the heads;
the map objects among its elements are the blocks) and its depth is
measured with an iterative walk: a block nested more than 32 levels deep
(the block's map is level 1) is `54000` (`automerge text block is nested
more than 32 levels deep`), which leaves the stack below 0.5 MB. Real
blocks are 2 or 3 levels deep. The block values in the result are then
converted from the maps `spans()` hydrated (a recursion as deep as the
checked block, far smaller frames than `hydrate`'s) with the jsonb
mapping; keys are taken in Automerge's order, not the hydrated map's hash
order, so that two keys that become one after U+0000 → U+FFFD keep the
same one as `automerge_to_jsonb` does.

Cost: one load, one `list_range` over the text, the depth walk (one
`map_range` / `list_range` per map and list inside each block), and
Automerge's own span iteration, which hydrates every block. Measured with
`mise run bench-core` (document `rich20k`: 200,000 characters in 20,000
paragraphs, each block with `type`, `parents`, `attrs` and `isEmbed`,
every tenth paragraph bold; release build): `ReadDoc::spans` alone
0.81 s, `write_spans` on the loaded document 1.29 s, of which the depth
walk is about 0.45 s (some 7 µs per range call, three per block) and
finding the blocks 0.04 s; `spans_to_json` of the stored value (load
included) 1.79 s. So `automerge_spans` costs about 1.6 times Automerge's
own `spans()`, linearly in the number of blocks; the depth walk is the
price of not crashing on deep blocks. (An earlier version, which found
blocks with `text()` and one `get()` each and wrote block values with a
second walk of the objects, took 2.11 s.)

Tests: `crates/pg_automerge_core/tests/spans.rs` compares the result with
Automerge's own `spans()` / `spans_at()` rendered as `export_span` does
(plain and empty text, overlapping marks, marks expanding or not at their
boundaries, removed marks, every mark value type, nested and adjacent
blocks and embeds, block values of every type, emoji and other
multi-unit characters before blocks and marks with documents loaded in
each text encoding, literal U+FFFC and non-map objects in a text,
conflicting texts under one key, historical heads, and random concurrent
rich-text edits merged, at every intermediate heads), plus path
resolution, the depth guard (up to 20,000 levels, at heads before the
nesting, and at heads where a block since made shallow was deep), blocks
present only at some heads, NUL replacement, and block keys that collide
after it (the same winner as `automerge_to_jsonb`, whatever the hash
order); `src/tests/spans.rs` covers the SQL side
(paths against `#>`, `22023` messages, heads errors, NULLs, expanded
values, the labels and the depth error); the regress example shows the
output.

### Deep blocks

A block is a map inside a text object (`splitBlock` in JavaScript). Its
value can hold further maps and lists, nested as deep as the writer
likes: the regress fixture `deep_block` holds a block nested 2,000
levels deep in 4 kB, and nothing in Automerge or in the input checks
refuses it. Automerge renders a block's value with `hydrate`, which
recurses once per level and takes about 13 kB of stack per level in a
release build, so such a block overflows the backend's 8 MB stack
wherever Automerge renders it: a segfault, which no guard catches and
after which the postmaster restarts every session. Automerge renders blocks in two places:

- its span iterator (`ReadDoc::spans` / `spans_at`), for every block of
  the text: `automerge_spans` checks the depth of every block first
  (54000, see [Rich text spans](#rich-text-spans));
- its document iterator (`ReadDoc::iter_at`), whose items for a text are
  that text's spans. Until 2026-09-30 the jsonb walk used it for every
  document, so `doc::jsonb` / `automerge_to_jsonb` (both overloads) and
  everything built on them (`->`, `->>`, a stored generated `doc::jsonb`
  column, an expression index, which crashed the INSERT that stored such
  a value) crashed the backend. A text maps to a string in which a block
  is U+FFFC, so the walk never needed the rendered blocks at all.

**The audit.** Every path that could recurse in proportion to a
document's structure, for every SQL function:

| Path | Automerge calls | Recursion per level |
|---|---|---|
| input (`automerge_in`, `automerge_recv`, the `bytea` cast), `automerge_out` / `automerge_send` | `load`, `load_incremental`, `save_nocompress`, the save-and-load check | none |
| `doc::jsonb`, `automerge_to_jsonb(doc[, heads])`, `->`, `->>` | the walk: `iter_at` (sweep), or `map_range` / `list_range` / `text()` (per object); `get_missing_deps`, clocks for heads | `iter_at` renders blocks (fixed below); the rest none. The walks keep their own stack; nesting deeper than 1000 levels is 54000 before `convertToJsonb` (which recurses, with Postgres' `check_stack_depth`) |
| `automerge_spans` | `get`, `list_range`, `map_range`, then `spans()` | `spans()` renders blocks: only after the depth check (32 levels); the conversion of the rendered blocks recurses at most as deep |
| `automerge_changes`, `_meta`, `_bytes`, `automerge_get_change`, `automerge_change_count` | `get_changes`, `get_changes_meta`, `save_after`, `get_change_by_hash` | none |
| `merge`, `\|\|`, `merge(automerge, bytea)`, `merge_agg`, `automerge_contains` (both) | `get_changes_added`, `apply_changes`, `load_incremental`, `Automerge::merge`, `get_heads`, `get_missing_deps` | none |
| `automerge_heads`, `automerge_notify` | the chunk header, or `get_heads` | none |

Found by reading Automerge 0.12.0 (`hydrate_map` / `hydrate_list` are its
only recursions over document structure; they are called by the span
export of both iterators, which the extension reaches only as described
above, and by `ReadDoc::hydrate`, `Automerge::rescue` and `text_diff`
(`update_text`, `update_spans`), which it does not call) and checked by
measurement: `crates/pg_automerge_core/tests/deep_structures.rs` runs
every core entry point behind these functions
(normalize of a save, a compressed save and change chunks, loads, the
jsonb walks current and as of heads, spans, the history functions,
merges both ways stored and loaded, `merge(automerge, bytea)`,
containment, the `merge_agg` accumulator, saves of the results) on
documents nested 20,000 levels deep (maps; lists; a block holding maps;
a block holding lists of maps; texts in blocks in texts) and on a
history of 20,000 changes, on a thread with a 1 MB stack, which any
recursion of more than about 50 bytes per level overflows. Before the
fix the jsonb walks overflowed it on both block shapes; everything else
passes it (in a release build, everything else also passes on a 128 kB
stack at that depth, while the jsonb walks overflowed 1 MB).

**The fix.** The walk takes the per-object walk for any document that
has, or ever had, a block: Automerge's `text()`, `map_range` and
`list_range` do not render blocks, and the per-object walk already gave
exactly the sweep's output. Whether a document has blocks is read from
its stored bytes (`blocks::has_blocks`): the objects in which an op makes
a map, found run by run in the object and action columns of the
document chunk (`budget::any_map_parent`; ops are sorted by object, so
it costs about one step per run), each looked up in the loaded document
for its type (a text: blocks) until the first text. Deleted and
overwritten ops count too, so the answer holds for every historical
state. Bytes that are not one readable document chunk (never a stored
value) answer "has blocks", the safe side: the per-object walk gives the
same result. An expanded value without stored bytes yet (a merge result)
is saved once for this, and the save is kept as its stored bytes, which
it needs when it is written anyway; its answer is cached with it.

Cost (`mise run bench-core`, release build, medians): the check takes
1.1 ms on `items20k` (877 kB, 20,000 small maps in a list, so a single
object holds map-making ops), 0.1 ms on `items2k`, nothing measurable on
`text3mb` and `typed5k`, and stops at the first text on `rich20k`; a
read of `items20k` loads for 158 ms and walks for 65 ms. Documents with
blocks gain: the walk of `rich20k` (20,000 paragraphs, each after a
block) takes 15 ms instead of 917 ms, because the document iterator
rendered every block, and visited every block's contents, for nothing.
See [the benchmarks](#deep-blocks-2026-09-30).

**Not rejected on input.** Deep blocks are still accepted by the input
functions and merges:

- With the fix, every function reads such a document: the jsonb view,
  generated columns and indexes, history, merges, containment, output.
  Only `automerge_spans` of the very text that holds the deep block is
  refused (54000), and the rest of the document's spans are readable.
- They are valid Automerge documents that any client can write, and
  that the other clients of the document merge. Refusing them at input
  would make a row unwritable as soon as one client's document holds
  one (every later full save or change set of that document would fail),
  would break the restore of dumps whose values were stored before
  (restore goes through the input function), and would cost every write
  a walk of every block.

**No `check_stack_depth` calls.** What recursion is left in the
extension's own code is bounded: the conversion of rendered blocks in
`automerge_spans` (at most 32 levels, checked before Automerge renders
them), and dropping those rendered values (as deep). The jsonb walks and
the depth check use explicit stacks, and jsonb's own recursion
(`convertToJsonb`) checks the stack itself. A check before calling into
Automerge would not help: how deep Automerge recurses is decided inside
it, which is why the fix keeps such calls away from deep structures.

**Upgrading.** The fix changed no SQL object: it takes effect as soon as
the new library is loaded, before `ALTER EXTENSION .. UPDATE` (which 0.2.0
needs for `automerge_spans` only, see [0.1.0 to
0.2.0](#010-to-020)), and the jsonb result of every document is unchanged (expression indexes and generated columns
stay valid). Values stored by earlier builds read normally with it,
including documents whose jsonb view crashed those builds (a table
without a jsonb generated column or expression index could store them).

Tests: `crates/pg_automerge_core/tests/blocks.rs` (the block check against
a scan of every change's ops over generated documents with and without
blocks, blocks that were deleted, blocks in nested texts, compressed
saves and saves made on the spot, bytes that are not one document chunk,
and the reads of stored, loaded and merged values against the per-object
walk on a small stack), `tests/deep_structures.rs` (above),
`tests/json_walk.rs` (the sweep and the choice against the per-object
walk); pg_tests in `src/tests/blocks.rs` (a 5,000-level block: a stored
generated `doc::jsonb` column, a GIN and a B-tree expression index,
`doc::jsonb` of stored, expanded and merged values, `merge_agg`,
`automerge_to_jsonb` as of heads, `->>`, updates, and every other
function; `automerge_spans` still 54000); the regress example (the cast,
a generated column and `automerge_to_jsonb` of the 2,000-level
`deep_block` fixture); `tests/limits.sh` step 3 (a 5,000-level block
stored, merged and read through every path on a scratch cluster with an
8 MB stack, and no backend terminated by a signal; the build before the
fix segfaults there on the INSERT) and the same checks in the Docker
image (`tests/docker.sh`).

## Resource limits

Automerge input is deflated and run-length encoded, so its size says
little about what loading it takes: a 4 kB compressed save of a
4,000,000-character text takes 390 MB and 4.3 s to load, a 113-byte
change chunk can describe 10,000,000 operations (about 5 GB), and memory
grows with what the input describes (see [Input amplification
measurements](#input-amplification-measurements-2026-09-29)). Automerge
allocates with the Rust global allocator, and a failed Rust allocation
aborts the backend, after which the postmaster restarts every session of
the cluster. Anyone who can write an `automerge` value can send such input
(input functions are called without an `EXECUTE` check, so `REVOKE` does
not help), and a single load cannot be cancelled. So every path that takes
client bytes prices the load from what can be read cheaply, before
Automerge allocates anything, and refuses it with an ERROR when the
estimate exceeds a limit.

### The `pg_automerge.max_load_memory` setting

An integer in kB (like `work_mem`; `SET pg_automerge.max_load_memory =
'512MB'`), **2 GB** by default, `-1` for no limit, superuser-only
(`PGC_SUSET`, for the reason `verify_writes` is: one session with a higher
limit can abort a backend, which restarts the cluster for everyone; a
superuser can set it per role or database, or `GRANT SET ON PARAMETER`).
Like `verify_writes`, only the library defines it: `SHOW` works once the
library is loaded, and `SET` before that leaves a placeholder that
Postgres checks when the library loads.

`_PG_init` reserves the `pg_automerge` prefix (`MarkGUCPrefixReserved`,
which pgrx does not call): a misspelled name such as
`pg_automerge.max_load_memroy` is an error (`42602`, `"pg_automerge" is a
reserved prefix.`) once the library is loaded, and a placeholder set before
it loaded (from `postgresql.conf`, `ALTER SYSTEM`, `ALTER ROLE/DATABASE ..
SET` or a `SET` in the session) is removed with a WARNING when it loads.
Without that, a typo was accepted silently and the real setting kept its
default.

It bounds an *estimate* of the peak memory of one load (or merge), in
bytes. One setting, not separate caps on operations, changes or actors:
memory has a changes × actors term, and the same operation costs 5 to 10
times more as a bare change than inside a saved document, so separate caps
would either let through what the estimate refuses or refuse ordinary
documents.

The default admits about 4.4 million characters of plain text (whose real
peak is about 0.4 GB: the estimate over-counts plain text about five
times, see below); the tests' 3,000,000-character document estimates at
1.38 GB, so a 1 GB default would refuse it. Time follows the estimate at
up to about 8 s per GB on the test machine (release build; the core test
asserts it for every input of its battery when run with `--release`), so
at the default an uncancellable load takes at most about 16 s. A dev
build (`cargo pgrx install` without `--release`) does not optimize the
extension's own crates, and its scan is about ten times slower. Every session can use up to
the limit at once (plus the stored documents a merge loads, which are not
priced), so size it like `work_mem`. Every estimate includes 64 kB, so a
limit of 64 kB or less refuses every write (the tests use it for that).

The limit decides whether a write can be refused; it never changes a
result's bytes, so the functions stay `IMMUTABLE`, as with
`verify_writes` (a plan folded under one setting may raise, or not,
under another, like `statement_timeout`).

### What is checked where

| Path | Priced before Automerge runs | Then |
|---|---|---|
| Text input (literals, `COPY`, a restore, a logical replication subscriber) | A literal whose decoded length alone exceeds the limit at 10 bytes per byte, before its hex is decoded; then as binary input | |
| Binary input (`COPY .. BINARY`, binary parameters), the `bytea` cast | The load of the input on its own | The normalized document, as it will be stored |
| `merge(automerge, bytea)`, `\|\|` | Bare change chunks: parsing and applying them to the document. A save whose header heads the document has (the no-op answer): only parsing it (the bytes inflation produces, once; nothing for an uncompressed save). A save loaded on its own: its load; its missing changes: applying them. A save whose trailing changes need the document: loading it after the document | Every new result, as it will be stored |
| `automerge_contains(automerge, bytea)` | What it parses or loads, as for `merge` | |
| `merge(automerge, automerge)`, `\|\|`, `merge_agg` (every step), PL/pgSQL `d := merge(d, x)` | Applying the changes one side adds, counted from their change chunks (`Change::raw_bytes()`) | The result, as it will be stored |
| Reads (`::jsonb`, history, heads), the stored side of every merge, `merge_agg` inputs | never | |

Refusals are `53400` (configuration_limit_exceeded):

```text
ERROR:  estimated memory to load automerge input exceeds "pg_automerge.max_load_memory" (2048 MB)
DETAIL:  Loading it could take up to 9537 MB (10000001 operations, 1 change, 1 actor, 123 bytes uncompressed).
HINT:  A superuser can raise "pg_automerge.max_load_memory".
```

with "merged automerge document" for a merge result ("Loading it"), "for
applying automerge changes" for the changes a merge applies ("Applying
them"), "Loading the document it normalizes to" in the DETAIL of an input
whose normalized form is over the limit, and "at least" when the scan
stopped early (below). The limit is shown in MB when it is a whole number
of them, otherwise in kB.

Why `53400` and not `54000` (program_limit_exceeded): Postgres raises
`53400` when `temp_file_limit` is exceeded, the closest analogue, a limit
an administrator configures; `54000` is for fixed limits and stays for the
jsonb and nesting limits of the jsonb view. Retrying does not help unless
the limit is raised.

Bundle chunks (Automerge's experimental format that packs many changes
into one chunk) in client input are refused with `0A000`
(feature_not_supported) whatever the limit: pricing them would need a
parser of their own, and Automerge does not write them unless asked to.

### The estimate

In bytes, saturating, from counts read from the chunks:

- **A document chunk** (a save, or the loaded document a merge result will
  be saved as): 450 per op + 30 per successor entry + 600 × Gmax + 1600 per
  change + min(130 × (ops + successors), 1600 × changes) + 200 per
  dependency entry + 200 per actor + 0.3 × changes × actors + 3 × min(changes,
  (ops + successors) / 16) × actors + 5 per *rebuilt* byte of messages
  and actor ids and 3 per rebuilt byte of keys and mark names (below) +
  200 per *extra* column metadata entry (below).
- **Change chunks, or changes about to be applied** to a document with
  *base* changes and actors: 1000 per op + 80 per pred entry + 2500 per
  change + 200 per dependency + 200 per distinct actor + 100 per entry of
  a change's list of other actors (duplicates included) + 0.3 × (changes ×
  (base actors + new actors) + base changes × new actors) + 8 per
  *repeated* byte (below) + 200 per *extra* column metadata entry.
- **Plus** 10 per byte of the chunks with their columns inflated, and 64 kB
  (Automerge's fixed structures, which the per-unit costs do not cover for
  tiny documents).

**Rebuilt and repeated bytes.** A load rebuilds every change of a
document as a change chunk of its own, and applying changes turns their
ops into Automerge's structures; both copy strings that the input may
hold once for many rows, in a repeat run (`n` copies of a value, stored
once). The scan computes what such runs expand to from their headers,
without expanding them: a repeat run of `n` strings of `len` bytes
expands to `(n - 1) × len` bytes beyond the input's own (literal values
are input bytes, already charged 10 each).

- Rebuilt bytes of a document chunk: its change messages (every rebuilt
  change holds its message twice, in its bytes and as a `String`, and a
  load whose heads do not match clones every rebuilt change into its
  error: 4.0 bytes per byte measured, charged 5), and the actor ids
  longer than the 16 bytes Automerge's `ActorId` holds inline (every
  rebuilt change holds its own actor and the other actors its ops refer
  to, in its bytes and as `ActorId`s: 4.0 bytes per byte measured,
  charged 5). A change's own actor is counted from the change actor
  column, run length × length; the others from the object and key actor
  columns, each run at most once per change (min(run length, changes) ×
  length), plus three references of the longest actor per successor
  entry (the pred of a rebuilt op, or the delete Automerge rebuilds from
  it: its object, key and pred); a change holds every actor once at
  most, so its own and the others together are at most changes × the
  long actors' total length.
- Rebuilt key bytes of a document chunk: its ops' keys and mark names,
  which a rebuilt change holds once, in its bytes (2.0 bytes per byte
  measured with the error's clone, 1.6-1.7 for saves written by
  Automerge whose keys are overwritten many times; charged 3).
- Repeated bytes of changes: the keys and mark names of change chunks
  (importing a change's ops makes an owned `String` of every op's key and
  mark name, and the document holds a key literally wherever other keys
  come between its rows: 1.0 bytes per byte measured for one key, 2.0 for
  mark names, 4.0 for a key the document then holds literally, 5.9 with
  the save of the result that `normalize` makes; charged 8), and those
  of a document chunk turned into changes (its rebuilt bytes and rebuilt
  key bytes, which applying copies again: its keys can end up between
  the rows of the document they are applied to).

A load of input on its own is the first document chunk as a document plus
everything after it as changes applied to it (Automerge turns a later
document chunk into changes: it is charged both as a document and as its
changes). Loading after a document (`a ++ changes`) or applying chunks to
it charges every chunk of the input as changes, with the document as the
base.

**Extra column metadata entries.** A chunk lists its columns in
metadata blocks, an entry (spec, length) per column, and Automerge's
parse keeps every entry in vectors that double as they fill (and copies
the list as it checks the layout), while the entry of an empty column
takes two input bytes: 5,000,000 of them made a 10 MB document chunk
peak at 498 MB (it loaded) and a 9.7 kB compressed change chunk at
716 MB. Every entry beyond one per column Automerge writes (9 + 16 for
a document chunk, 14 for a change chunk, which the per-op and per-change
costs include) is charged 200: measured up to 120 per entry for a
document chunk, 168 for a change chunk and 174 for a compressed one, at
counts just past a power of two (4,200,000), where a doubling vector
overshoots most.

For a merge result, the rebuilt bytes of the bound (see [Merge
results](#merge-results)) add the bytes and the repeated bytes of the
changes applied: a rebuilt change is the change that was applied, so
this bounds what loading the result copies. It does not always bound
what a scan of the saved result counts: the actor term is not additive
(a run of the object actor column that new rows split counts up to
min(run, changes) twice), so, like the bytes term, a result can be
stored with a scanned estimate somewhat over the limit when its bound is
under it; its load copies no more than the bound says.

The counts: ops are the largest row count of the known op columns (not
the members of a group); Automerge sizes some allocations by one column's
length before it checks the columns against each other, so the largest
counts. Successors, preds and dependencies are the sums of their group
columns (or the member columns' rows, if larger). Ops of every action are
counted alike (deletes too; the flat 450 covers them). Actors of change
chunks are counted by name, once each, for the 200 (and the clock-cache
terms); besides, every entry of a change's list of other actors costs 100,
duplicates included: Automerge's parse keeps one 32-byte `ActorId` per
entry in a vector that doubles as it fills, and an entry (an empty actor
id) takes one input byte, so a 10 kB compressed change listing 10,000,000
empty actors peaked at 627 MB while pricing them by name and bytes said
100 MB (measured up to 73 per entry; 100 leaves room for the old and new
buffer of a growth step both being held). Dependencies (32 bytes each,
320 through the bytes) and heads already cost more per entry than their
vectors take. **Gmax** is the largest number of
successor entries in one (object, key) group of rows: a new group starts at
the first row, at an insert row, when the object changes, or when the key
changes and the previous row was not an insert (a key overwritten or
deleted many times, which Automerge resolves with a pending queue per
key).

The constants are the worst measured cost of each unit (the appendix);
every generated and crafted input tried stays below the estimate, at
most 0.80 of it (ops prepended to a list of maps; a document whose 2,000
changes share one 100 kB message or actor id). Known over-estimates:
plain text about 5 times (appended characters cost about 90 bytes, the 450
per op is forced by out-of-order ops at about 335; a tighter bound would
mean simulating Automerge's per-change reorder queue), and a key
overwritten many times in one change about 10 times (Gmax cannot tell
deletes from successors that are rows). The estimate is only as good as
its measurements: upgrading Automerge means re-running
`crates/pg_automerge_core/tests/memory_bounds.rs` and the fuzz harness
(below), which check it.

### What the estimate covers

Where a load or apply allocates in proportion to something the input
describes, the term that prices it, and the worst peak / estimate of the
inputs of `tests/memory_bounds.rs` (release build):

| Allocation (Automerge 0.12) | Priced by | Worst peak / estimate |
|---|---|---|
| The op set and the rebuilt changes' ops (document chunk) | 450 per op, 30 per successor, 600 × Gmax, 130 per row (at most 1,600 per change) | 0.80 (maps prepended to a list) |
| Rebuilt changes (document chunk) | 1,600 per change | 0.71 (20,000 empty changes) |
| Dependencies | 200 per entry | 0.68 (1,000 × 1,000) |
| Actors, the clock cache | 200 per actor, 0.3 per change × actor, 3 per large change × actor | 0.39 (1,000 conflicting forks) |
| Parsing and applying change chunks | 1,000 per op, 80 per pred, 2,500 per change | 0.67 (marks, as change chunks) |
| A change header's list of other actors | 100 per entry, duplicates included | 0.64 (2,200,000 empty actors) |
| Bytes, inflated (values, strings, the chunk buffers) | 10 per byte | 0.71 (1 MB of bytes, compressed save) |
| Change messages repeated by a run, copied into every rebuilt change (document chunk) | 5 per rebuilt byte | 0.80 (2,000 changes, one 100 kB message) |
| Keys and mark names repeated by a run, copied into every rebuilt change (document chunk) | 3 per rebuilt key byte | 0.67 (2,000 changes, one 100 kB key) |
| Actor ids over 16 bytes, copied into every rebuilt change that refers to them (document chunk) | 5 per rebuilt byte | 0.80 (2,000 changes by, or referring to, a 100 kB actor) |
| Keys and mark names repeated by a run, one owned copy per op when applied, and held literally by the document where other keys come between (change chunks) | 8 per repeated byte | 0.59 (500 maps, one 50 kB key interleaved) |
| Column metadata entries beyond one per column Automerge writes | 200 per entry | 0.79 (4,200,000 empty columns, compressed change chunk) |
| Merging inserts scattered over a large list (column slabs split as they are inserted into) | not priced beyond the per-op costs | open: under review |

The rebuilt, repeated and extra-column terms were added after the load
limit shipped (see [Rebuilt copies and column
metadata](#rebuilt-copies-and-column-metadata-2026-09-30)): before them, the same
inputs peaked at 63 to 167 times their estimate (5 to 9 times for the
metadata entries).

### The scan

`pg_automerge_core::budget::scan_input` reads the chunk headers, the
actors and dependency lists, and the column metadata and data of every
chunk:

- Deflated columns and compressed change chunks are inflated one at a
  time, and in total at most to a tenth of the limit: each inflated byte
  costs 10 in the estimate, so more than that exceeds it whatever else the
  input holds, and the scan stops there (a deflate bomb costs a tenth of
  the limit in scan memory, at most, and is reported "at least"). An
  uncompressed chunk is read in place.
- Columns are counted run by run, never expanded: a run of `n` values is
  one step whatever `n` is, and literal values are only stepped over,
  numbers by the last byte of each LEB128 value, eight bytes at a time,
  strings by their lengths. (An over-long number, at which Automerge's
  decoders stop, is stepped over like any other: for malformed input the
  scan can count more rows than Automerge reads, never fewer.)
  Decoding is lenient, as Automerge's streaming decoders are: a run that
  cannot be read ends the column.
- Gmax is first taken at its upper bound, the chunk's successor entries
  (0 when there are none). Only when that bound is what puts an estimate
  over the limit is the input scanned again with Gmax computed exactly:
  the object, key, insert and successor columns merged by runs (within a
  stretch where each is one run, the rows are either all groups of their
  own, being inserts or a key counter that changes every row, or all add
  to the current group). A property test checks it against the
  row-by-row definition. Documents far below the limit never pay for it
  (on the 877 kB list document the exact pass costs about as much as the
  rest of the scan).
- Column metadata is validated in place and read again entry by entry
  with the columns' data: nothing is allocated per entry. Only the
  columns the exact Gmax pass merges are held (the first of each spec;
  one listed twice makes Gmax its bound).
- The loops run the interrupt check (a cancel stops a scan), per run of
  a column and per entry of an actor list or a column metadata block.
- A change whose list of other actors is longer than the limit pays for
  (110 per entry, its byte and the 100) stops the scan before its entries
  are read, like a deflate bomb ("at least"): 20,000,000 empty actors
  deflate to 19 kB, and stepping through them would take seconds. So
  does a column metadata block whose extra entries alone exceed the
  limit (200 each): 12,000,000 empty columns deflate to 24 kB.
- It never fails. Framing it cannot parse (bad magic, lengths, column
  metadata, a column that does not inflate, an unknown chunk type) stops
  it with the counts of what came before, which is all Automerge can
  allocate before it fails on the same bytes (it parses a chunk
  completely before it reconstructs or applies it); the caller checks
  those and lets Automerge reject the input with its own message (`22P02`,
  as before). The fuzz harness checks that input Automerge loads is never
  taken for unparseable.

The same pass answers whether a single document chunk passes Automerge's
chunk parse (`header::document_parses`, the condition of the no-op
answers of `merge(automerge, bytea)` and `automerge_contains`), so those
paths inflate a compressed save once, as before. A compressed save that
is loaded (text or binary input, the `bytea` cast, `merge(automerge,
bytea)` with a save loaded on its own) keeps the columns the scan
inflated until the normalized save has been compared with them (see
[Invariants](#invariants)), so it is inflated twice, by the scan and by
Automerge's load, as it was before the limit (by the load and by the
comparison); what is held meanwhile is at most the inflated bytes, each
charged 10 in the estimate, and they are held only once that estimate
has been checked: `merge(automerge, bytea)` drops them before it loads a
stored `a` to look for the heads of a save with no more changes than `a`
(the save is priced only after that load, and may still be rejected),
and without a limit nothing is kept (nothing charges them), the
comparison inflates the input again. Without a limit, input
and the `bytea` cast walk only the chunk types (for bundles), merges of
documents count nothing, and `merge(automerge, bytea)` and
`automerge_contains(automerge, bytea)` still scan the input (without a
cap) for the parse check and the bundle check.

### Merge results

Each loaded document carries its counts (`LoadedDoc`, the `merge_agg`
state): exact when scanned from a save (a stored value, the input it was
loaded from), and for a merge result an upper bound, the target's counts
plus the changes applied (ops and dependencies added, every pred a
successor entry and possibly all on one key, so added to Gmax too, every
actor possibly new, the bytes added). Only when that bound exceeds the
limit is the result saved and scanned exactly, and refused if that is
over too; the save is kept and becomes the value's stored bytes (after the
check of `verify_writes`, for a result of client bytes). So PL/pgSQL
chains and `merge_agg` pay no save per step, and a merge that stays well
within the limit costs a scan of the changes applied (and of the stored
target, to know its counts).

Consequences: merging two documents that each fit can fail with `53400`
(the result would not), and so can `merge_agg`; repeated small writes
cannot grow a document past the limit. The bytes term of the bound is an
approximation (the merged columns may encode less compactly than the
sum), so a result can be stored somewhat over the limit when its bound is
just under it; every other term is an upper bound.

### Stored values and lowering the limit

Every stored value fit the limit when it was written (or was written
with a higher limit, or none), and loading a stored value is never
checked: reads peak at 1.0 to 1.5 times a load of the value, which was
accepted when it was written. A separate ceiling for reads was considered
and rejected: it would make data unreadable after the fact, which is worse
than the memory the read takes. So lowering the limit never makes data
unreadable, and writes that add nothing are not refused either: `UPDATE ..
SET doc = doc`, `merge(doc, doc)`, re-sent change chunks, an older save,
and a re-sent uncompressed save (parsed in place) load nothing new. A
re-sent *compressed* save costs its inflation (once), refused only when
that alone exceeds a tenth of the limit.

What does get refused under a lower limit is *writing* such a value
again as new input, which is what a restore does:

- **Dump and restore**: `pg_restore`, `psql < dump` and `COPY FROM`
  validate every value against the restoring session's limit. To restore
  documents written under a higher limit (or none), raise it for the
  restore: `PGOPTIONS='-c pg_automerge.max_load_memory=-1' pg_restore ...`
  as a superuser, or `ALTER ROLE restorer SET pg_automerge.max_load_memory
  = ...` beforehand.
- **Logical replication**: the subscriber validates every replicated
  value with the apply worker's setting; set it for the subscription's
  owner or the database (`ALTER ROLE .. SET`, `ALTER DATABASE .. SET`) at
  least as high as the publisher's, otherwise the apply worker fails on
  such a row and retries until the setting is fixed.

### What the limit cannot do

- Turn an allocation failure into an ERROR. `std::alloc::
  set_alloc_error_hook` is unstable in Rust 1.98, a `#[global_allocator]`
  must not unwind, and allocating through `palloc` would `longjmp` over
  Automerge's frames on failure (or abort anyway). The estimate before the
  load is the only defence; memory beyond it (a bug in the estimate, an
  Automerge upgrade that costs more) still aborts.
- Bound what concurrent sessions take together (each can use up to the
  limit), or the stored documents a merge loads besides the changes it
  applies.
- Make a load cancellable; it bounds its length (about 8 s per GB of the
  estimate).

### Tests

- `crates/pg_automerge_core/tests/memory_bounds.rs`: a counting allocator
  (per thread) measures the peak of every path (normalize of saves,
  compressed saves and change chunks, `merge_changes`, `contains_changes`,
  `merge` of two documents, the accumulator) on generated documents of
  every shape the estimate prices differently and on crafted chunks (RLE
  op, change, dependency, successor and pred bombs, many actors, change
  headers listing millions of empty or duplicate other actors and
  duplicate dependencies, plain and compressed, a save with 2,000
  trailing change chunks, strings repeated by a run (change messages,
  keys, mark names; interleaved keys the document then holds literally),
  long actor ids, millions of empty column metadata entries; the
  crafted document chunks again with a made-up head, after another save
  and through `merge_changes`, where a load rebuilds and applies them
  (without heads it skips them as already contained); and the same
  strings and actor ids in saves written by Automerge, on their own and
  after another save, through `normalize` and `merge_changes`), and
  ordinary documents with long keys overwritten many times or 32-byte
  actor ids: at the limit set to the estimate
  the input is accepted (or refused by Automerge, for crafted input that
  does not load) with a peak below it; one byte lower it is refused before
  loading with a peak that is a small fraction of it; when the document
  an input normalizes to is priced higher than the input, `normalize` at
  the input's own estimate stays below it and refuses the result as the
  normalized document; `merge_changes` of the crafted inputs into a
  stored document stays below the two estimates. Also deflate bombs
  (256 MB of zeros, as a deflated column and as a compressed change chunk)
  refused quickly with a small peak, a 19 kB change listing 20,000,000
  empty actors and a 24 kB one listing 12,000,000 empty columns refused
  at the default limit without their entries being read, the scan's
  counts against Automerge's
  `stats()` and its own change parser, Gmax against the row-by-row
  definition on random histories, merge results outgrowing the limit, the
  guarantees under a lowered limit, bundles, and the messages.
- The fuzz harness (`tests/fuzz.rs`): input Automerge loads is never
  marked unparseable by the scan, `normalize`'s measured peak stays below
  the estimate whether Automerge accepts the input or refuses it, and it
  refuses input only when the estimate is over. Its seeds include change
  chunks with repeated header list entries.
- `src/tests/limits.rs` (pg_tests): the setting's default, unit and
  privileges; `53400` with its message, DETAIL and HINT on every SQL path
  (text input, `COPY` text and binary, the `bytea` cast, `merge` and `||`
  with a save, a compressed save and change chunks, PL/pgSQL,
  `automerge_contains`, a literal refused before decoding, a deflate
  bomb); merge results over the limit through `merge`, `||`, `merge_agg`
  and a PL/pgSQL chain (whose variable survives the failed step); stored
  values readable and no-op writes accepted under a lowered limit; `-1`;
  bundles refused whatever the limit.
- `tests/limits.sh` (`mise run limits`, part of `mise run test`): a scratch
  cluster with its address space capped (`ulimit -v`, about 1 GB). With
  the limit off, the 12 kB compressed save of a 12,000,000-character text,
  a 113-byte crafted change chunk and a 19 kB compressed change chunk
  listing 20,000,000 empty other actors each abort the backend ("memory
  allocation of 1543503872 bytes failed", signal 6) and the cluster
  restarts, which shows the inputs and the cap reproduce the crash; so do
  a 1 kB compressed save of 6,000 changes sharing one 100 kB message, a
  100 kB document chunk of 6,000 changes by a 100 kB actor id, a 100 kB
  change chunk of 16,000 puts of one 100 kB key, and a 24 kB compressed
  change chunk listing 12,000,000 empty columns. With
  the default limit, the same inputs through text input, the `bytea` cast,
  `merge`, `||`, `automerge_contains`, `INSERT` and `COPY` get `53400`
  with DETAIL and HINT, no backend is terminated by a signal, a session
  opened before is still connected, `pg_postmaster_start_time()` is
  unchanged and ordinary writes work.
- `tests/extension.sh`: the setting stays superuser-only for a
  non-superuser database owner (as `verify_writes`).
- The pg_regress example shows the error.

## Memory observability

Automerge allocates with the Rust global allocator, so every loaded
document, the buffers around it, the rows a set-returning function holds
and a `merge_agg` state live in the backend's `malloc` heap, outside
Postgres' memory contexts: `pg_backend_memory_contexts`,
`pg_log_backend_memory_contexts()`, `work_mem` and HashAgg's accounting do
not see them, and a 3 MB document takes about 400 MB loaded (see
[Input amplification
measurements](#input-amplification-measurements-2026-09-29)). Since 0.3.0
two functions show it, for the current backend:

| Column of `automerge_memory_usage()` | Meaning |
|---|---|
| `allocated_bytes bigint` | Bytes the library's Rust code holds now: exact, counted by the global allocator |
| `peak_allocated_bytes bigint` | The most it held since the backend started or the last `automerge_memory_reset()`, including documents a call loads and drops |
| `live_documents bigint` | Loaded documents alive: `LoadedDoc`s (expanded values: `merge` results, the `bytea` cast's) and `merge_agg` states. A document a read loads and drops within one call is not counted (it shows in the peak and in `loads`) |
| `loads bigint` | `Automerge::load` calls: every load of a stored value, of client input, and every save-and-load check, failed ones included |
| `load_time double precision` | Their total time in milliseconds (wall clock, `Instant`) |

`automerge_memory_reset()` sets the peak to the current allocation and
`loads` and `load_time` to zero (the two others are current values). Both
are `VOLATILE` and `PARALLEL RESTRICTED` (in a parallel worker they would
report the worker's own heap; restricted keeps them in the leader), and
`STRICT` like every pgrx function (they have no arguments). They are
executable by `PUBLIC`: a session sees and resets only its own backend's
counters. `automerge_memory_usage()` returns a set (`RETURNS TABLE`, one
row), pgrx's way to return named columns; `SELECT * FROM
automerge_memory_usage()` or `(automerge_memory_usage()).loads`.

### Counting allocator or bookkeeping

Two designs were compared:

- **Bookkeeping around documents**: count `LoadedDoc`s and `merge_agg`
  states, and attribute an estimated size to each (the `budget` model of
  [Resource limits](#resource-limits)). Free for everything else, but an
  estimate (plain text is over-counted about five times), known only for
  documents whose counts were scanned, and blind to the biggest users:
  the document every read loads and drops within one call (`doc->>'a'`
  loads the whole document), the rows of a set-returning function, the
  jsonb builder's tree, merge intermediates.
- **A counting `#[global_allocator]`** (`src/alloc.rs`): the system
  allocator plus two atomic counters. Exact, and covers everything the
  library's Rust code allocates, at a cost on every allocation.

Measured cost of the allocator (release builds, one CPU pinned, median of
7 runs, minimum of 2 runs per variant, 2026-10-01; bench-core's documents,
see [Performance](#performance)):

- A 16 to 80-byte allocation and free: 36.9 ns without, 38.0 ns with
  counting (two uncontended atomic read-modify-writes, about 1.1 ns).
- `mise run bench-core`: `Automerge::load` and `normalize` of every
  document within noise (-0.8% to +0.6%; text3mb 2583 → 2563 ms,
  items20k 157.8 → 158.1 ms); the jsonb walks +0.4% to +2.3%
  (items20k one-sweep walk 63.2 → 64.6 ms); the allocation-heaviest case,
  the per-object walk of many small maps, +3.6% (items20k 144.5 →
  149.7 ms) and +6% (items2k 13.1 → 13.9 ms); `save_nocompress` and the
  load memory scan unchanged.
- `mise run bench-sql` (release build installed in the pgrx Postgres,
  each SQL path timed alone, median of 3; runs with, without and again
  with the allocator): no measurable cost. The run without it was the
  slower one, by 2% in the geometric mean over the cases above 5 ms
  (R1 text3mb 2979 ms without, 2898 and 2902 ms with; W1 items20k 373,
  361 and 363 ms; W4 text3mb 5689, 5595 and 5564 ms; the largest
  difference either way within 5%, except `A1` items20k at 9 ms), so the
  machine's drift between runs exceeds the allocator's cost.

So the allocator was chosen: its cost is at most a few percent on paths
that allocate per object and within noise elsewhere, and it answers the
question that matters ("how much memory does this backend's Automerge
hold, and how much did it take at most") exactly. Bookkeeping is kept
only where it is exact and nearly free: the live document count (a
token in each `LoadedDoc` and `merge_agg` document, `stats::Live`) and the
load count and time (`stats::timed_load` around the one
`Automerge::load` call site, `load_bytes`), in the core crate
(`stats`; no unsafe code).

Soundness of the allocator: every method passes its arguments to
`std::alloc::System` and returns its result unchanged; the counters are
`AtomicUsize` statics updated with `Relaxed` read-modify-writes (exact
under any number of threads; a backend has one) that never allocate,
panic or re-enter the allocator. A failed allocation or reallocation
changes no counter (the old block stays counted). The peak is raised
with `fetch_max` only when the new total exceeds it, and read as at least
the current total.

What the numbers mean:

- Bytes are what Rust asks for (`Layout::size`). `malloc` adds its chunk
  headers and rounding, and keeps freed memory for reuse (glibc returns
  it to the system only at the top of the heap or for large blocks it
  mapped separately), so a backend's RSS can stay high after
  `allocated_bytes` fell back, until the end of the transaction, where
  the library returns it once enough was freed (see
  [Returning freed memory](#returning-freed-memory-pg_automergetrim_threshold)).
- A `#[global_allocator]` covers the Rust code linked into this library:
  Automerge, the core, and pgrx's own Rust allocations (16 to 123 bytes
  between statements in the tests). Another Rust extension in the same
  backend has its own allocator and is not counted, nor is Postgres'
  `palloc` memory (the jsonb results, detoasted values), which its memory
  contexts show.
- With the library in `shared_preload_libraries`, a backend inherits the
  postmaster's counters along with its memory (a copy of what `_PG_init`
  allocated, if anything; a fresh backend reports 16 bytes).
- In a `FROM` clause that does not depend on the outer row the function
  scan runs once and is rescanned from its stored result, so to watch the
  counters change per row call it in the target list (the pg_tests do).

### Returning freed memory (`pg_automerge.trim_threshold`)

The [soak test](#soak-test) found it (2026-10-01): with eight long-lived
connections writing and reading documents of up to 1 MB for 70 minutes,
every backend's anonymous RSS stayed between 200 and 355 MB for the whole
run while `automerge_memory_usage()` showed 16 bytes allocated between
statements. Not a leak (the high-water mark did not grow: 342 MB, the
peak of loading the largest document, a 1.26 MB rich text), but `malloc`
keeping the freed document for reuse: glibc gives memory back only from
the top of the heap (`M_TRIM_THRESHOLD`) or for blocks it mapped
separately, and it raises its mmap threshold (up to 32 MB) after the first
large block is freed, so a loaded document ends up as holes in the heap.
The container's anonymous memory was 2.4 to 2.6 GB of its 4 GB limit
throughout: each connection holding the most it ever needed, whether
idle or not. For an application pool that is `connections × the largest
document's load`, permanently.

Since 0.3.0 a transaction callback (`RegisterXactCallback` in
`_PG_init`; no `shared_preload_libraries` needed) calls glibc's
`malloc_trim(0)` at the end of every top-level transaction (commit,
abort, prepare; and in parallel workers) in which the library's Rust
allocation fell by at least `pg_automerge.trim_threshold` from its high
point since the last trim. `malloc_trim` returns the top of the heap and,
with `madvise(MADV_DONTNEED)`, every free page inside it. The high point
is a third counter in the allocator (`alloc::HIGH`, a relaxed load and a
rare store per allocation, no read-modify-write); it starts over at each
trim. Inside a transaction nothing is trimmed, so a loop over documents
in one transaction reuses the memory.

Measured on the pgrx Postgres (dev build, its dependencies optimized; one
backend reading `doc::jsonb` of the soak's templates, each read its own
transaction, median of 7 after one warm-up):

| Document (load peak) | RssAnon after, `-1` | after, 64MB | Read, `-1` | Read, 64MB | Read, `0` |
|---|---|---|---|---|---|
| 1.26 MB rich text (342 MB) | 274 MB | 3 MB | 1695 ms | 1995 ms | 1998 ms |
| 1.07 MB list of maps (22 MB) | 9 MB | 9 MB | 685 ms | 678 ms | 705 ms |
| 209 kB list (5 MB) | 9 MB | 9 MB | 135 ms | 135 ms | 141 ms |
| 4 kB board (< 1 MB) | 2 MB | 2 MB | 3.0 ms | 2.7 ms | 2.9 ms |

(RssAnon of the backend after a second read, from 2 MB before the first;
glibc returned the list's memory by itself: its large blocks were mapped
separately.)

The cost is on the transaction after a trim that needs the memory again:
the trim itself took 30 ms at `COMMIT` (342 MB freed), and the next read
of the same document 260 ms more (+15%): the pages are faulted in and
zeroed again, and on this host (transparent huge pages `always`) the
fresh heap is no longer backed by huge pages. Transactions that freed
less than the threshold pay nothing; with `0` (trim after every
transaction that freed anything) the medium documents pay 3 to 5%.

Default: 64 MB. A backend then keeps at most about that much of freed
memory between transactions, and only transactions that used more than
64 MB of Rust heap (documents of roughly 200 kB of rich text or 1 MB of
small maps and more) pay for the trim, about 15% of their load time when
the next transaction needs the memory again. A backend that keeps
reloading the same large documents (and has the memory for it) can
disable it (`-1`) or raise it; the setting is `SUSET` like the others,
since a session that turns it off holds memory every other session of
the container competes for.

Alternatives considered:

- glibc tunables (`GLIBC_TUNABLES=glibc.malloc.mmap_threshold=...`, or
  `mallopt(M_MMAP_THRESHOLD)` in `_PG_init`) fix the mmap threshold, so
  large blocks are always mapped and unmapped: the same page-fault cost on
  every large allocation, in every transaction, and no help for the
  holes left by blocks below the threshold.
- Trimming after every statement that freed a lot (an `ExecutorEnd`
  hook): more often than needed inside transactions, and statements
  outside the executor (utility commands) are missed.
- Trimming when a backend goes idle: the right moment, but Postgres has
  no hook there; a timer's signal handler cannot call `malloc_trim`.
- Only the Rust allocation is watched. `palloc` memory freed by
  Postgres (a detoasted 1 MB value, the jsonb result) goes back to the
  same heap and is returned by the same trim, but does not trigger one.
- Other C libraries: `malloc_trim` is glibc's (the Docker image and the
  supported distributions); elsewhere the setting does nothing.

Tests: a pg_test (`trim_returns_freed_heap_memory_once_enough_was_freed`)
frees 32 MB of heap blocks below a block that stays and checks that RSS
keeps them, that `trim_if_freed` trims at 16 MB but not at 64 MB nor
twice, and that RSS drops by the 32 MB; another checks the setting
(`kB`, superuser, `-1`). `tests/memory.sh` reads `RssAnon` from
`/proc/self/status` through `pg_read_file` in a real session (so also
against the Docker image): after reads of a 400,000-character text (34
MB peak) with `-1` the backend keeps 36 MB; with `16MB` it is back within
a few MB of its start after each transaction, but not inside one. The
soak test with the trim: see [Soak test](#soak-test-2026-10-01).

### Cluster-wide view: not provided

A view over every backend would need shared memory: a slot per backend
reserved at startup (`shmem_request_hook`), which works only with the
library in `shared_preload_libraries`. The Docker image and ordinary
installs load it on first use, so the view would be empty exactly where
it is not configured, and each allocation would have to publish to shared
memory (or each backend at the end of each statement). The alternative of
showing the heap as a memory context (so that `pg_backend_memory_contexts`
and `pg_log_backend_memory_contexts(pid)` report it) is not possible:
since PostgreSQL 16 a context's methods come from the fixed
`mcxt_methods[]` table, indexed by the 4-bit method ID in every chunk
header (`MemoryContextMethodID`, `utils/memutils_internal.h`), so an
extension cannot define a context type whose statistics it reports. What to do instead (README,
[Monitoring](README.md#monitoring)): sample `automerge_memory_usage()`
from the application's own connections (a pooled backend is reached only
through its connection), and from outside, a backend's RSS, which
includes this heap.

### Tests

- Core (`tests/stats.rs`, and a unit test in `stats.rs`): the live count
  follows `LoadedDoc`s and a `merge_agg` accumulator's document (and its
  finished copy), reads count a load and keep nothing, failed and
  panicking loads are counted, `reset()` zeroes only the cumulative
  counters.
- pg_tests (`src/tests/memory.rs`), reading the SQL function: one row,
  labels `VOLATILE`/`PARALLEL RESTRICTED`, reset; a read of a
  200,000-character document counts one load and time, its peak shows the
  loaded document, and nothing stays; a PL/pgSQL variable holding a merge
  result is one live document of more than the document's size while it
  lives; a window `merge_agg` holds its state's document and the frame
  result's while it runs, nothing after; a target-list
  `automerge_changes` behind a cursor that fetched one row holds its
  2,000 rows until `CLOSE`, and one stopped by `LIMIT` frees them; after
  errors in the middle of operations (a reused actor id inside `merge`
  and inside `merge_agg` with its state loaded, the jsonb nesting limit
  part way through a walk, a division by zero while a set-returning
  function holds its rows, malformed input, the forced failure of the
  deferred check while flattening an expanded value in an `UPDATE`), three
  times over, no document and no byte (within 64 kB of slack) stays;
  and 200 rounds of in-place merges, jsonb conversion, history rows,
  `merge_agg`, updates, a `LIMIT`ed set-returning function and caught
  errors leave the allocation where it was (within 8 kB). That test was
  checked against a deliberate leak: a 512-byte `mem::forget` per jsonb
  conversion made it fail with exactly 102,400 bytes over the 200 rounds;
  without it the drift is 0 bytes.
- `tests/memory.sh` (`mise run memory`, part of `mise run test`, and run
  against the Docker image by `mise run docker-test`): one session runs
  100 rounds (after 3 of warm-up) of top-level statements: an upsert with
  `merge`, incremental changes, a newer save through the cast, jsonb
  reads and containment, heads, `merge_agg`, history rows, a `LIMIT`ed
  target-list set-returning function, a cursor left open until `COMMIT`,
  a rolled back update, PL/pgSQL in-place merges; and errors part way:
  malformed input, changes with missing dependencies, an exception raised
  while a PL/pgSQL variable holds a merge result, and `statement_timeout`
  cancelling a `merge_agg` whose state holds a merged document. Every
  expected error happens in every round and no other; afterwards the
  allocation is where it was (measured: 123 → 123 bytes), no document is
  alive, and at least 10 loads per round were counted. Then the reset,
  and a new session's counters starting at zero.
- `tests/upgrade.sh` and `tests/docker_upgrade.sh`: the functions after
  `ALTER EXTENSION .. UPDATE` from every earlier version, relocated too,
  and an update over a user's `automerge_memory_usage()` in the
  extension's schema failing.

## Error codes

Every error the extension raises, by SQLSTATE. The core crate's `Error`
enum carries the class (`InvalidInput`, `MissingDependencies`,
`ConflictingChanges`, `InvalidParameter`, `LimitExceeded`, `LoadLimit`,
`Unsupported`, `Internal`),
and `src/error.rs` maps it; errors that only the glue can detect (NULL
array elements, trigger usage, jsonb limits) are built there directly.

| SQLSTATE | When | Message (DETAIL / HINT) |
|---|---|---|
| `0A000` feature_not_supported | A bundle chunk (Automerge's experimental format) in client input, whatever the limit | `automerge bundle chunks are not supported` |
| `22P02` invalid_text_representation | Bad text input; bytes that are not a loadable Automerge save or change sequence (including decoder panics); a result that does not survive a save and load | `invalid input syntax for type automerge: ...`, `invalid automerge document: ...`, `invalid automerge changes: ...` |
| `22P02` | `merge(automerge, bytea)` with changes whose dependencies are in neither input | `invalid automerge changes: missing N dependencies that neither the document nor the input contains` (DETAIL: `Missing changes: <hashes>.`) |
| `22P02` | A change hash that is not 64 hex digits | `invalid automerge change hash "...": expected 64 hexadecimal digits` |
| `22000` data_exception | Two histories that disagree about one actor's changes (a reused actor id): two different changes with the same (actor, seq), or a second author assignment, met by `merge`, `\|\|`, `merge_agg`, `merge(automerge, bytea)`, `automerge_contains(automerge, bytea)` when it loads, or one input holding both (text input, the `bytea` cast) | `conflicting automerge changes: actor <hex> has two different changes with seq N`, or for a second author assignment (Automerge writes an actor's author only on its seq 1 change, so this takes a writer that reuses an actor id across authors or crafts changes) `conflicting automerge changes: actor <hex> is assigned an author again at seq N`; both with DETAIL: `An actor's changes form one sequence; these inputs hold two different ones, which cannot be merged.` HINT: `Each writer must use its own actor id. Automerge picks a random one for every document instance unless the application sets it.`) |
| `22003` numeric_value_out_of_range | A change's `seq`, `start_op` or `op_count`, or the change count, above `bigint`'s range (Automerge rejects such changes on load, so this is not expected to occur) | `automerge seq N is out of range for type bigint` (DETAIL: the change) |
| `22004` null_value_not_allowed | A NULL element in `since_heads` / `heads` | `since_heads must not contain NULL` |
| `22023` invalid_parameter_value | `automerge_to_jsonb(doc, heads)` or `automerge_spans(doc, path, heads)` with a head the document lacks; `automerge_spans` with a path to something other than a text object (`automerge value at path {notes,0} is a map, not a text object`); bad `automerge_notify` arguments (count, channel length, key column listed twice or of type `automerge`) | `automerge document does not contain change <hash>`, `automerge_notify(): ...` (HINT on how to declare the trigger, or which columns to name) |
| `39P01` trigger_protocol_violated | `automerge_notify()` not fired `AFTER ... FOR EACH ROW` for INSERT/UPDATE/DELETE, or called outside a trigger | `automerge_notify() must be fired ...`, `automerge_notify() can only be called as a trigger` (HINT: the correct `CREATE TRIGGER`) |
| `42703` undefined_column | `automerge_notify()` key column that does not exist | `automerge_notify(): key column "x" does not exist in table ...` |
| `53400` configuration_limit_exceeded | Client input, changes a merge applies, or a merge result whose estimated load exceeds `pg_automerge.max_load_memory` (see [Resource limits](#resource-limits)) | `estimated memory to load automerge input exceeds "pg_automerge.max_load_memory" (2048 MB)`, `estimated memory to load merged automerge document exceeds ...`, `estimated memory for applying automerge changes exceeds ...` (DETAIL: `Loading it could take up to N MB (X operations, Y changes, Z actors, B bytes uncompressed).`, "at least" when the scan stopped early; HINT: `A superuser can raise "pg_automerge.max_load_memory".`) |
| `54000` program_limit_exceeded | A document nested deeper than 1000 levels, or with more elements, pairs or a longer string than jsonb allows, read as jsonb; a text block nested deeper than 32 levels read with `automerge_spans` | `automerge document is nested more than 1000 levels deep`, `automerge text block is nested more than 32 levels deep`, `number of jsonb array elements exceeds the maximum allowed (N)`, `string too long to represent as jsonb string` (DETAIL, as jsonb's) |
| `XX000` internal_error | A stored value that does not load (corruption, or a value stored with `pg_automerge.verify_writes` off that does not survive a save and load), broken invariants (bugs), `automerge_change`/`automerge_change_meta` altered by their owner | `corrupt stored automerge value: ...`, `automerge failed on a stored value: ...`, others naming the invariant |

### Why `22000` for a reused actor id

Merging two histories that hold different changes with the same (actor,
seq) used to fail with `XX000` when both were `automerge` values and with
`22P02` when the changes came as `bytea`. Neither fits: `XX000` says the
extension is broken (and monitoring pages someone for it), `22P02` says the
bytes are malformed, but each input is a valid document. What is wrong is
the combination, caused by a writer bug: two writers (or two copies of a
document) used the same actor id. Candidates, against PostgreSQL's
errcodes table (Appendix A) and how drivers map classes:

- `23505` unique_violation / class `23` integrity_constraint_violation:
  (actor, seq) is a key, but not a table constraint. Drivers map class 23
  to "integrity error" exceptions (psycopg's `IntegrityError`, JDBC's
  `SQLIntegrityConstraintViolationException`) that applications routinely
  catch in an upsert-or-ignore path, which would silently drop the
  write; clients also expect a constraint name with it. Rejected.
- `55000` object_not_in_prerequisite_state: for an object whose state
  forbids the operation (a sequence not yet used, a replication slot in
  use); retrying later can succeed. Here neither document is in a wrong
  state and no retry helps. Rejected.
- `40001` serialization_failure: drivers and ORMs retry it automatically,
  which would loop. Rejected.
- `22023` invalid_parameter_value: the extension uses it for an argument
  that does not fit the document (unknown heads); the (actor, seq) clash
  is not about one argument, and in a merge either side can be the
  "wrong" one. Rejected.
- `22000` data_exception, chosen: the class for data values that are
  unacceptable, with no more specific code in class `22` for this.
  Drivers map class `22` to a non-transient data error (psycopg's
  `DataError`, JDBC's `SQLDataException`): not retried, not confused with
  a constraint, and distinct from `22P02` so a client can tell "these
  bytes are corrupt" from "these two histories cannot be combined" by
  SQLSTATE alone.

Every path reports it the same way, since the cause is the same: the
Automerge errors `DuplicateSeqNumber` and `DuplicateAuthor` become
`Error::ConflictingChanges` wherever Automerge raises them (a load of
concatenated input, applying change chunks, merging two documents); other
Automerge errors keep their class. So input that holds both histories on
its own (a save followed by the other writer's change chunk, through text
input or the `bytea` cast) is also `22000` now, where it was `22P02`.
Both variants are tested in the core crate (`merge_changes.rs`:
`reused_actor_id_is_a_conflict_on_every_path` for `DuplicateSeqNumber`,
`reassigned_author_is_a_conflict` for `DuplicateAuthor`, with a change
crafted by adding an author footer to a seq 2 change); the SQL tests
check the SQLSTATE, DETAIL and HINT for the first, which share one mapping
in `src/error.rs`.

### Other reclassifications (before 0.1.0)

- Nesting deeper than 1000 levels: `XX000` → `54000`. The document is
  valid; the jsonb view has a limit, like jsonb's own element and string
  limits, which are already `54000`.
- A change field above `bigint`: `XX000` → `22003`, as for any value out
  of `bigint`'s range.
- `automerge_notify()` called outside a trigger: `0A000` → `39P01`, the
  code of every other misuse of the trigger and the one Postgres' own C
  trigger function `suppress_redundant_updates_trigger` raises for the
  same mistake (PL/pgSQL's compiler uses `0A000` for it; a C function is
  closer to the former).
- The `automerge_notify()` error for an `automerge` key column moved its
  advice ("name the columns that identify the row") from the message into
  a HINT.

### Message style

Following PostgreSQL's error message style guide:

- The primary message is short, factual, lowercase and without a trailing
  period; it names the specific object (the hash, the actor and seq, the
  key column, the trigger) after a colon or in quotes, as Postgres' own
  messages do (`invalid input syntax for type ...: ...`).
- The DETAIL carries supporting facts as complete sentences (the hashes
  of missing dependencies, the change holding an out-of-range value, why
  conflicting changes cannot be merged).
- The HINT carries advice as complete sentences: how to declare the
  notification trigger, which key columns to name, that each writer needs
  its own actor id.
- Messages of Automerge's own errors are embedded after a colon (`invalid
  automerge document: <Automerge's message>`), and the message of a
  caught decoder panic in parentheses; their wording is Automerge's.

Tests: `reused_actor_id_is_a_conflict_on_every_path` (core) and
`reused_actor_id_is_a_data_exception_with_a_hint` (pg_test: SQLSTATE,
message, DETAIL and HINT for every SQL path, and that the failed writes
changed nothing), the pg_regress example showing the error, and the
existing per-code tests (`notify_trigger_validates_usage`, the nesting and
hash tests).

## Performance

Loading dominates: `Automerge::load` rebuilds and hashes every change,
while saving, cloning and applying small changes are cheap. Cost of the
primitives (Rust, release build; `load` is `Automerge::load` of the stored
bytes, `apply` one small change set to a clone, `walk` the jsonb walk of a
loaded document into a sink that discards the events; `mise run
bench-core` measures load, normalize, save and walk):

| Document | Stored | load | save_nocompress | clone | clone + apply | walk |
|---|---|---|---|---|---|---|
| 3,000,000-character text, 1 change | 3.0 MB | 2465 ms | 7.1 ms | 0.7 ms | 1.6 ms | 247 ms |
| 20,000 list items, 401 changes | 877 kB | 161 ms | 3.5 ms | 1.3 ms | 1.8 ms | 72 ms |
| 2,000 list items, 41 changes | 83 kB | 16 ms | 0.3 ms | 0.1 ms | 0.3 ms | 6.4 ms |

Building the jsonb value from the walk's events adds about 30 ms on the
877 kB list document (see [the appendix](#jsonb-built-in-rust-memory-2026-09-29));
a `serde_json::Value`, for comparison, takes 89 ms and 8 ms in total with
the walk on the two list documents.

Since 0.3.0 every Rust allocation is counted (for
`automerge_memory_usage()`): about 1 ns per allocation, within noise on
loads and at most a few percent on the allocation-heavy per-object walk
(see [Memory observability](#memory-observability)); the numbers above
were measured before.

What costs a load, per call:

- Every `automerge → jsonb` evaluation loads the document. Several
  accessors on one column in one statement (`doc->>'a', doc->>'b',
  doc->>'c'`) are several casts, so several loads: three accessors took
  1275 ms against 414 ms for one. Convert once instead, with a `LATERAL`
  subquery (`FROM docs, LATERAL (SELECT doc::jsonb AS j OFFSET 0) x`, then
  `x.j->>'a'`; the `OFFSET 0` keeps the planner from inlining the cast
  back into every reference) or, for read-heavy tables, a stored generated
  column, which the IMMUTABLE cast allows:

  ```sql
  ALTER TABLE docs ADD COLUMN data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED;
  CREATE INDEX ON docs USING gin (data jsonb_path_ops);
  ```

  Expression indexes also work: `CREATE INDEX ON docs USING gin ((doc::jsonb));`
- `automerge_heads`, `automerge_change_count` and `automerge_notify()`
  never load a stored document; `merge`, `merge_agg`, `automerge_contains`
  and the since-functions skip loads where the headers decide. On a 3 MB
  stored document (one head; release build, warm cache), where a load
  takes about 2.7 s: `automerge_heads` 0.3 ms;
  `automerge_contains(doc, ''::bytea::automerge)` 0.5 ms;
  `merge(doc, ''::bytea::automerge)` 1 ms (only a prefix of the 3 MB
  argument is detoasted);
  `merge` with a newer version of it, in either argument order, one load
  (2.6 s), as is a `merge_agg` over both. `merge_agg` over a single row
  loads nothing (22 ms, reading the 3 MB value).
- Writing a value: a full save, canonical (what `doc::bytea` or another
  stored value give) or compressed (`Automerge.save()`, when it inflates
  to the canonical encoding, which it does for saves by this Automerge
  version) is loaded once to validate it (3 MB: 2.6 s; 877 kB: 180 ms;
  83 kB: 20 ms). Any other input (a save plus trailing change chunks, a
  save by an implementation whose encoding differs) costs two loads
  (validate, then verify the normalized re-save; one with
  `pg_automerge.verify_writes` off).
- `merge(doc, $changes::bytea)` sends and parses only the new changes, but
  on a stored `doc` still loads it, saves the result and loads it once more
  to verify it (on the 3 MB document: 5.2 s for a one-change update; 2.7 s
  with `pg_automerge.verify_writes` off). Changes that are already there
  are usually seen from the chunk hashes without a load (185 ms, the
  detoast and the row rewrite).
- A newer full save of the stored document, `merge(doc, $save::bytea)` or
  `merge(doc, $save::automerge)` with a cast in the query, costs one load
  (of the save; 2.8 s on the 3 MB document), and the stored document is
  never loaded. The upsert (`INSERT .. ON CONFLICT DO UPDATE SET doc =
  merge(docs.doc, EXCLUDED.doc)`) and a typed `automerge` parameter in a
  custom plan cost two (see [Expanded values](#expanded-values)). A
  concurrent save (neither contains the other) costs a load of each side
  and the verification load.
- `automerge_contains(doc, changes bytea)` and the no-op check of
  `merge(doc, changes bytea)` need no load for re-sent latest changes, a
  re-sent save of the document, and (for `automerge_contains`) new changes
  on top of the current heads, and a newer or concurrent save (its header
  lists at least as many changes as the document has); for an older save
  `automerge_contains` loads the stored document, never the save.
  `automerge_contains(a, b)` of two `automerge` values likewise answers
  `false` without a load when `b` has at least as many changes as `a` and
  other heads (bench-sql C1/C2, 877 kB list / 3 MB text: 167 / 2568 ms →
  1.2 / 9 ms for a newer save as `bytea`, where 9 ms is the save's
  checksum; 164 / 2549 ms → 1.2 / 1.1 ms for a newer stored version).
- History (the 3 MB document plus 200 small changes, 201 changes):
  `automerge_change_count` 2 ms; `automerge_changes_meta` 2.8 s (one load);
  `automerge_changes` and `automerge_changes_bytes` for all changes 5.1 s
  (load plus rebuilding 3 MB of changes); since the 100th change, or one
  `automerge_get_change`, 2.7-2.8 s; `automerge_to_jsonb(doc, old heads)`
  3.5 s against 3.0 s for the current state; any since-function given the
  current heads 1 ms.
- `automerge_notify()` on 20,000 small rows (release build):
  `UPDATE t SET n = n + 1` 451 ms without the trigger, 746 ms with it (no
  notifications); merging a new change into every row 7.2 s without, 7.8 s
  with (20,000 notifications); re-sending that change 709 ms with a plain
  `merge` (a no-op rewrite of every row, no notifications) and 46 ms with
  the `WHERE NOT automerge_contains` pattern.
- TOAST compression: stored values are uncompressed saves, which
  Postgres compresses (`pglz` stores the 877 kB list in 190 kB; the 3 MB
  text of random letters stays uncompressed, `pglz` gains nothing on it).
  With the default `pglz`, profiles of the release build on the 877 kB
  list put about 9% of an `INSERT` of a save and 5% of a `merge` of new
  changes in `pglz_compress`, and about 1% of a jsonb read in
  decompression; the rest is Automerge. `lz4` (`ALTER TABLE .. ALTER
  COLUMN doc SET COMPRESSION lz4`, or `default_toast_compression`, on a
  server built with it) compresses and decompresses much faster than
  `pglz`, so it can trim that part of writes; it is not measured here (the
  pgrx-managed Postgres is built without lz4) and the default is left
  alone.
- Pricing a write (see [Resource limits](#resource-limits)) costs a scan
  of the input and, for a merge, of the stored target: 1.7 ms for the
  877 kB list document stored, 3.7 ms for its compressed save, 0.2 / 0.4
  ms at 83 kB; the 3 MB text document scans in under 0.1 ms stored and
  22 ms compressed (inflating its columns, which the load does again).
  Against a load of 160 ms and 2.5 s, about 1 to 2%.
- Merge results are expanded values: nested merges, `merge(...)::jsonb`,
  `merge_agg(...)::jsonb` and PL/pgSQL loops doing `d := merge(d, x)` keep
  the document loaded between steps. Measured with `mise run
  bench-expanded` (release build, warm cache, mean of three runs,
  milliseconds; change sets are one small commit each, `merge_agg` over
  the document and 8 concurrent forks; the first rows are writes and a
  plain read for comparison):

  | Workload | 3.0 MB | 877 kB | 83 kB |
  |---|---|---|---|
  | `bytea` → `automerge` of a compressed save | 2617 | 182 | 20 |
  | `bytea` → `automerge` of stored bytes | 2581 | 178 | 21 |
  | `doc->>'status'` (a stored row) | 2816 | 314 | 33 |
  | `UPDATE .. SET doc = merge(doc, c1)` | 5246 | 377 | 41 |
  | `automerge_heads(merge(doc, c1))` | 2543 | 178 | 21 |
  | `merge(doc, c1)::jsonb->>'status'` | 2827 | 320 | 34 |
  | `automerge_heads(merge(merge(merge(doc, c1), c2), c3))` | 2552 | 181 | 22 |
  | PL/pgSQL `d := merge(d, c)`, 20 change sets | 2584 | 217 | 28 |
  | the same, each merge in a `BEGIN .. EXCEPTION` block | 2596 | 216 | 28 |
  | the same, then `UPDATE .. SET doc = d` | 5356 | 413 | 50 |
  | 2 merges into `d`, then 10 reads `d->>'status'` | 5209 | 1541 | 153 |
  | `merge_agg(doc)::jsonb` over 9 versions | 23136 | 1629 | 168 |

  A loop costs one load in total (plus one save and one verification load
  when the result is stored). A per-backend cache of documents loaded
  from tables (and of converted jsonb) is future work: a table column
  always arrives flat.

## Implementation notes

- `automerge` is a custom varlena type, not a `#[derive(PostgresType)]`
  (whose serde/CBOR storage we don't want). It is defined in SQL in an
  `extension_sql!` block (shell type, I/O functions declared by hand with
  `#[pg_extern(sql = false)]`, `CREATE TYPE`). Three Rust types map to SQL
  `automerge` with `TypeOrigin::ThisExtension`, all listed in that block's
  `creates`, so pgrx orders every other function after the type:
  `AutomergeDatum` (a new flat value: the input and receive functions),
  `AutomergeArg` (an argument, flat or expanded) and `AutomergeValue`
  (other results: new flat bytes, or a datum passed through, i.e. an
  unchanged argument or a new expanded object, e.g. the `bytea` cast's).
- `AutomergeArg` is not detoasted up front: the heads fast path fetches a
  prefix with `pg_detoast_datum_slice`, and `AutomergeArg::detoast()`
  returns a `Detoasted` guard holding either the datum and its detoasted
  varlena (`pg_detoast_datum_packed`: the datum itself when inline and
  uncompressed, else a palloc'd copy the guard frees when dropped; its
  bytes are the core `Input`, read in place; an unchanged result is the
  original datum) or the loaded document of an expanded one. Neither
  outlives the call. (Until 0.2.0 the bytes were copied again into a Rust
  `Vec` and the palloc'd copy stayed until the per-call context reset, so
  a flat argument held about twice its size; a pg_test measures the
  context before and after.)
  `merge`, `merge(automerge, bytea)` and `automerge_contains` first decide
  what they can from the heads (a prefix of a flat value), before
  anything is detoasted.
- Errors go through one `raise()` (`src/error.rs`) taking a core `Error`
  or a `PgError` (code, message, optional DETAIL and HINT). `raise` and
  `or_raise()` are `#[track_caller]`, so the error's LOCATION is the line
  that raised it.
- Interrupts: the core's long loops (the jsonb walk, building history rows
  and ordering them, the load memory scan) call an interrupt check every
  1024 steps, which the
  extension registers in `_PG_init` as `CHECK_FOR_INTERRUPTS()`
  (`set_interrupt_check`); `automerge_merge_agg_trans` checks between inputs. A
  cancel or `statement_timeout` raises its ERROR there, which unwinds
  through the core's guard untouched. Single Automerge calls (a load, a
  merge, a save, `text()`) have no hook, so a cancel waits for the one
  running to return (for a 3 MB document, a load takes 2.5 s). Running
  them on a helper thread would make them abandonable, but that is a
  larger design change and not done.
- Release profile: `panic = "unwind"` is mandatory (pgrx turns panics into
  ERRORs by unwinding, and the guard catches decoder panics); `strip =
  "debuginfo"` keeps the symbol table for backtraces and profiles.
- Extension objects that pgrx does not generate (the type, casts,
  operators, aggregate, trigger function and every `COMMENT`) are written
  in `extension_sql!` blocks next to the Rust code they belong to; the
  blocks' names (`automerge_type`, `automerge_casts`,
  `automerge_merge_operator`, `automerge_merge_agg`,
  `automerge_change_types`, `automerge_notify`, ...) are what other blocks
  `require`.
- Build profile: dev builds compile dependencies optimized and Automerge
  without debug assertions or overflow checks (Cargo.toml). Its debug
  assertions make large documents quadratic, and its overflow checks turned a
  counter past i64::MAX into a panic on read in dev builds only.
- Test instrumentation (a counter of in-place merges, the list of sent
  notifications, the core's per-thread count of `Automerge::load` calls,
  the forced failure of the save-and-load check and an override of
  `pg_automerge.verify_writes`, behind the `test-hooks` feature) is
  compiled only into test builds. The count of live documents
  (`loaded::live_count`, which the pg_tests use for leak checks) is the
  always-on counter of `stats` since 0.3.0.
- `pg_automerge.verify_writes` is defined in `_PG_init` and registered with
  the core (`set_verification_check`), which asks it whenever it would run
  the save-and-load check (`verification_enabled`).
  `pg_automerge.max_load_memory` likewise (`budget::set_limit_source`,
  read as bytes by `budget::limit` at every check; the core alone, as in
  its tests, uses the 2 GB default).

## Unsafe code

All `unsafe` is in the pgrx glue (`src/`); the core crate has none of its
own. Every Postgres function called from Rust goes through pgrx's
`pg_guard_ffi_boundary` (the generated `pg_sys` bindings, and explicitly
for the two hand-declared `jsonfuncs.h` functions), so an ERROR inside
becomes a Rust panic that unwinds through Rust frames (destructors run)
and is re-raised at the function's `#[pg_guard]` boundary; every callback
Postgres calls (the expanded-object methods, the memory-context reset
callback, the trigger entry point, the transaction callback) is `#[pg_guard]` or wrapped in
`pgrx_extern_c_guard`. Automerge calls run under the core's
`catch_unwind` guard, which turns Automerge's panics into errors and
passes pgrx's ERROR panics on untouched (they have a non-`String`
payload). Each area and the invariant it relies on:

| Area | What is unsafe | Invariant relied on |
|---|---|---|
| `datum.rs`: `AutomergeArg` | Reading a `Datum` as a varlena or expanded pointer | pgrx calls it with a non-null datum of type `automerge` (functions are `STRICT`; the trigger passes only non-null column values). The 1B_E tag is read before anything else, and an expanded object counts as ours only if its `eoh_methods` pointer is our method table; any other is flat (flattened by its own methods). |
| `datum.rs`: `flat_prefix`, `flat_len` | `pg_detoast_datum_slice`, `toast_raw_datum_size` | Only on flat (non-expanded) datums. The slice is always a fresh palloc'd copy in PG18 (`detoast_attr_slice`), copied out and freed only when it is not the input. |
| `datum.rs`: `Detoasted` | A `&[u8]` into a detoasted varlena | `pg_detoast_datum_packed` returns the datum itself or a fresh palloc'd copy; the slice borrows the guard, which frees the copy (not the datum) when dropped, after every borrow has ended. While unwinding it leaves the copy to the context reset. |
| `datum.rs`: `automerge_type_in` | `GetSysCacheOid(TYPENAMENSP, ..)` with a C-string key | The same call pattern as PG's `TypenameGetTypid`; a NUL-terminated literal. |
| `expanded.rs` | Creating, reading, replacing and flattening the expanded object | The object lives in its own memory context; the `Box<LoadedDoc>` is moved in only after every fallible allocation, and the reset callback (in the object's own chunk) drops it exactly once. The document is replaced only through a read-write pointer (`with_result`), after the caller's last read of it, with a document built completely beforehand, so a failure leaves it untouched. `get_flat_size` and `flatten_into` agree because the save is cached only on success and re-checked. |
| `merge.rs`: support function | Walking the planner's `SupportRequestModifyInPlace` node | Node tags are checked before each cast; only a top-level `PARAM_EXTERN` with the requested `paramid` is returned (PG18 `List` layout). |
| `merge.rs`: `merge_agg` state | `Internal` state as a `MergeAccumulator` | The state is created only by the transition function, in `AggCheckCallContext`'s context, with a drop-on-reset callback; `internal` cannot be built from SQL, so no other value reaches these functions. |
| `jsonb.rs` | Building a `JsonbValue` tree and `JsonbValueToJsonb` | Arrays are boxed slices (addresses stable when the vectors grow) and strings live in an arena whose chunks never reallocate; all of it outlives `JsonbValueToJsonb`, which copies everything. Counts are bounded by jsonb's own limits before they are built. |
| `history.rs`: `change_tuple` | `heap_form_tuple` via `PgHeapTuple::from_datums` | The descriptor the row is built with is checked first: exactly the attributes created by the extension, none dropped, each of exactly its type (`XX000` otherwise). pgrx checks only the count, and an owner can `ALTER TYPE .. ALTER ATTRIBUTE`. |
| `notify.rs`: trigger | `TriggerData`, the relation's `TupleDesc`, `heap_getattr`, raw datum comparisons | `PgTrigger::from_fcinfo` checks `CALLED_AS_TRIGGER`; the relation is open and locked for the call; dropped columns are skipped; raw comparisons are only between non-null values of the same attribute (by value, fixed length, or varlena bytes / TOAST pointer). Virtual generated columns are not stored and are refused as keys. |
| `notify.rs`: `json_categorize_type`, `datum_to_json`, `Async_Notify` | Hand-declared C functions | Signatures match PG 18.6's `utils/jsonfuncs.h` (the enum passed as `c_int`); called inside `pg_guard_ffi_boundary`. Channel and payload are NUL-free `CString`s within `NAMEDATALEN` and the 8000-byte payload limit. |
| `io.rs`: `automerge_recv` | Reading the `StringInfo` | Postgres passes a valid buffer; the rest of it is consumed and copied before it is used. |
| `lib.rs`: `_PG_init` | `MarkGUCPrefixReserved`, `RegisterXactCallback` | A NUL-terminated literal, copied by Postgres; a `#[pg_guard]` callback with Postgres' signature, registered once per backend (`_PG_init` runs once), which reads a setting and calls `alloc::trim_if_freed`, nothing that can raise an error at commit or abort. |
| `alloc.rs`: `malloc_trim` | A hand-declared glibc function (`target_env = "gnu"` only) | Takes no pointer; called from the transaction callback, between allocator calls, never inside one, so malloc's lock is not held and nothing re-enters it. |
| `alloc.rs`: the global allocator | `GlobalAlloc` for the counting allocator | Every method passes its arguments to `System` and returns its result unchanged; the counters are atomics, which never allocate, panic or re-enter the allocator (see [Memory observability](#memory-observability)). |

What `unsafe` cannot protect against, by design: a Rust stack overflow or
a failed Rust allocation aborts the backend, and Postgres then restarts
every session. They are prevented, not caught: the jsonb walk is iterative
with a 1000-level limit, Automerge never renders blocks (see
[Deep blocks](#deep-blocks)), and client input is priced before it is
loaded (see [Resource limits](#resource-limits)). A new use of a recursive
Automerge API needs the same gating. pgrx internals relied on
(`pgrx_extern_c_guard`, the callconv traits, ERROR panics with a
non-`String` payload, `SetOfIterator`'s memory context) are re-checked
whenever the pinned pgrx version changes.

## Testing

- Core (`cargo test -p pg_automerge_core`): unit tests in the modules, and
  in `crates/pg_automerge_core/tests/`: `basics.rs` (normalization, merge,
  the accumulator), `edge_cases.rs` (odd documents, scalars, encodings,
  corrupted bytes), `merge_changes.rs` (`merge(automerge, bytea)` and
  `automerge_contains(automerge, bytea)`), `heads_fast_path.rs` (the header
  parser against `Automerge::load().get_heads()` over hundreds of generated
  documents: empty, up to 12 actors, random forks and merges, merge and
  `merge_agg` outputs, arbitrary prefixes, and the heads and change-count
  shortcuts of `merge` and `automerge_contains` against the loaded
  history, for stored values and saves), `history.rs` (checked against
  Automerge's `fork_at` / `get_changes` and a dependency-graph walk, and the
  change count against the loaded change graph), `loaded.rs` (loaded
  documents are byte-identical to the flat path), `memory_bounds.rs` (the
  load memory limit: measured peaks against the estimate, see
  [Resource limits](#resource-limits)), `blocks.rs` and
  `deep_structures.rs` (the block check, and every entry point on
  documents nested 20,000 levels deep on a 1 MB stack, see
  [Deep blocks](#deep-blocks)), `stats.rs` (the live document and load
  counters, see [Memory observability](#memory-observability)). `common/`
  holds the
  random-history generator and stored-bytes wrappers around the
  `Input`-based API.
- `#[pg_test]`s (`cargo pgrx test pg18`) for SQL behaviour, in
  `src/tests/{io,merge,history,notify,expanded,hardening,loads,spans,blocks,memory}.rs`
  (`loads.rs` counts the `Automerge::load` calls of each write path and of
  `automerge_contains`, and
  covers the release of the cast's expanded values and the
  `pg_automerge.verify_writes` setting; `limits.rs` the load memory limit,
  see [Resource limits](#resource-limits); `memory.rs`
  `automerge_memory_usage()`, see
  [Memory observability](#memory-observability)). They are `include!`d
  into the `#[pg_schema] mod tests` in `lib.rs` rather than declared as
  submodules, because pgrx runs each test as a function of the `tests`
  schema and only items of that exact module go there. `cargo pgrx test`
  installs its `pg_test` build into pgrx's Postgres, so a server started
  afterwards (or a new backend of a running one) runs that build until the
  next `cargo pgrx install`; `mise run test` installs a plain build after
  the pg_tests for the scripts. Test documents are
  built in Rust with `automerge::AutoCommit` and passed in as `bytea`.
- `tests/pg_regress` (`mise run regress`): user-facing examples. Its
  fixtures are generated by `crates/pg_automerge_core/examples/gen_regress.rs`,
  and `mise run regress` first checks that they match
  (`tests/check_regress_fixtures.sh`). `--resetdb` is passed because a
  reused regress database would keep the extension objects of an earlier
  build.
- `tests/concurrency.sh` (`mise run concurrency`, also part of
  `mise run test`) runs two real psql sessions against one row to check
  the EvalPlanQual claim above, the upsert path and REPEATABLE READ, two
  sessions persisting only incremental changes with
  `merge(doc, $1::bytea)` (plus the orphaned-changes rejection), the
  `WHERE NOT automerge_contains(doc, $1)` pattern under EvalPlanQual, and a
  pg_dump/restore round trip.
- `tests/notify.sh` (`mise run notify`, also part of `mise run test`)
  checks `automerge_notify()` with a real `LISTEN` session: pg_tests run
  inside one transaction that is rolled back, so NOTIFY never delivers
  there (the pg_tests instead read what the trigger sent from a test-only
  per-backend list).
- `tests/dump.sh` (`mise run dump`, part of `mise run test`): plain and
  custom-format `pg_dump`/`pg_restore --exit-on-error` of a database with
  the extension in its own schema (not on the restore's `search_path`), a
  stored generated `doc::jsonb` column, GIN and btree expression indexes,
  an `automerge_notify()` trigger, a check constraint, views with `merge`,
  `||` and `merge_agg`, and a TOASTed value; checks fingerprints (bytes,
  heads, jsonb), that the indexes are valid and used and that the trigger
  fires after the restore; binary and text `COPY` round trips, and a
  corrupt value failing `COPY FROM` with `22P02`; a restore into a
  database with a lower `pg_automerge.max_load_memory` failing with
  `53400`, and succeeding with `PGOPTIONS='-c
  pg_automerge.max_load_memory=-1'`. A restore and `COPY
  FROM` validate every value (one load each; the generated column is
  recomputed, one more conversion).
- `tests/upgrade.sh` (`mise run upgrade`, part of `mise run test`) and
  `tests/docker_upgrade.sh` (`mise run docker-upgrade-test`, Docker): see
  [Upgrade tests](#upgrade-tests).
- `tests/extension.sh` (`mise run extension`, part of `mise run test`):
  the control-file flags, see
  [Installation, schema and privileges](#installation-schema-and-privileges):
  `CREATE EXTENSION .. SCHEMA`, `ALTER EXTENSION .. SET SCHEMA` under
  dependent objects, a dump and restore of the moved extension, a
  non-superuser's refused `CREATE EXTENSION`, and
  `pg_automerge.verify_writes` and `pg_automerge.max_load_memory` staying
  superuser-only, and misspelled `pg_automerge.*` settings being removed
  when the library loads (the prefix is reserved).
- `tests/limits.sh` (`mise run limits`, part of `mise run test`): the load
  memory limit against a real crash, in a scratch cluster whose address
  space is capped (see [Resource limits](#resource-limits)), and a
  document with a block nested 5,000 levels deep read through every path
  there without a crash (see [Deep blocks](#deep-blocks)). In its
  container mode (`LIMITS_CONTAINER`, used by `tests/docker.sh`) the
  capped server is a container of the Docker image started with
  `--memory` (see [Docker image](#docker-image)).
- `tests/memory.sh` (`mise run memory`, part of `mise run test`):
  `automerge_memory_usage()` across top-level statements, transactions
  and errors, see [Memory observability](#memory-observability).
- `tests/replication.sh` (`mise run replication`, not part of `mise run
  test`: it runs its own scratch cluster with `wal_level = logical`):
  logical replication in text and binary mode (see the README's
  limitations).
- Fuzzing: `crates/pg_automerge_core/tests/fuzz.rs` mutates real
  Automerge output at the chunk level (bits, bytes, insertions, deletions,
  dropped, duplicated and reordered chunks) and recomputes lengths and
  checksums, so the mutations reach the decoders. For every input:
  nothing panics out of the core, `normalize` agrees exactly with a plain
  load-save-load, its results are stored values (load, header heads,
  idempotent), `merge(automerge, bytea)` results load back with their
  heads, the load memory scan never takes input Automerge loads for
  unparseable, and `normalize`'s peak (a counting allocator) stays below
  the estimate, also when Automerge refuses the input; the block check
  (see [Deep blocks](#deep-blocks)) never panics on any of the bytes, and
  answers for a stored value as for a save of its loaded document. The seeds include
  change chunks whose header lists repeat entries. 1,500 inputs per `cargo test`; `mise run fuzz` runs 200,000 (or
  `FUZZ_ITERS`) and can save findings (`FUZZ_SAVE_DIR`) for
  `tests/corpus/`, which runs first. In 300,000 inputs it caught 4,903
  decoder panics (all `22P02`) and 19 inputs that load but whose re-save
  does not ("mismatching heads" on the reload; all of them several
  chunks, so never candidates for the compressed-input shortcut), all
  rejected by the save-and-load check, and found no property violation.
  Two of the latter are in the corpus; the deferred check of `merge`
  results is tested with the test hook (see
  [The deferred verification](#the-deferred-verification)).
- `tests/spans.rs`: `automerge_spans` against Automerge's own spans (see
  [Rich text spans](#rich-text-spans)).
- `tests/json_walk.rs`: the one-sweep jsonb walk, and the choice between
  the walks, against the per-object walk; `tests/normalize.rs`: the compressed-input shortcut against the
  full check; `tests/interrupts.rs`: the interrupt hook runs in the loops
  and its errors pass the guard; `tests/format_fixtures.rs`: values saved
  by every shipped automerge version (see
  [Versioning and upgrades](#versioning-and-upgrades)).
- `tests/bench_expanded.sh` (`mise run bench-expanded`, not part of
  `mise run test`) installs a release build and times the SQL workloads
  of [Performance](#performance) on three generated documents (and a
  rich text, `rich20k`, when `BENCH_DOCS` names it);
  `tests/bench_sql.sh` (`mise run bench-sql`) times the everyday paths
  (reads, inserts, the merge and upsert forms of a write, `merge_agg`,
  `automerge_contains`) one statement at
  a time through psql, writes in `BEGIN .. ROLLBACK`, median of three
  runs; `mise run bench-core` times the Rust primitives
  (`examples/bench_core.rs`).
- CI (`.github/workflows/ci.yml`) runs `mise run ci` (lint, test, regress)
  on every push and pull request, and the benchmarks nightly (uploaded as a
  workflow-run artifact). Runs are grouped by event and ref, so a newer
  push cancels an older push run on the same ref but never the nightly
  run, and the other way round. On tags it installs the PGDG Postgres 18,
  runs `PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config mise run package`,
  unpacks the tarball into `/` and runs `CREATE EXTENSION` in that server,
  then uploads the tarball as a workflow-run artifact (not a release).
- `mise run package` (`scripts/package.sh`) requires `PG_CONFIG` and
  refuses one under `$PGRX_HOME` (also through a symlink) or of another
  major version: `cargo pgrx package` mirrors that pg_config's install
  paths, so a package built for pgrx's development Postgres would unpack
  into `~/.pgrx/...`. It builds with `--locked` (a stale `Cargo.lock`
  fails). It then checks that the tarball holds
  `pg_automerge.so` under `--pkglibdir` and the control file under
  `--sharedir/extension`. `tests/check_ci.sh` (part of `mise run lint`)
  checks these refusals and the workflow's concurrency group and package
  step, the Docker packaging statically (see [Docker image](#docker-image)),
  and the workflow with actionlint and its docker and publish jobs'
  invariants (see [Docker image in CI](#docker-image-in-ci)).
- `mise run docker-test` (`tests/docker.sh`, not part of `mise run test`)
  builds the Docker image and tests it and `compose.yaml` against
  throwaway containers, including the regress examples, five of the
  multi-session scripts, `limits.sh` and a soak smoke run (see [Docker image](#docker-image));
  CI's `docker` job runs it on every push and pull request (see [Docker
  image in CI](#docker-image-in-ci)).
- The multi-session shell scripts share `tests/lib.sh`; they start the pgrx-managed
  Postgres if it is not running and stop it again only if they started it.
  With `PG_AUTOMERGE_TEST_HOST` (and `_PORT`, `_USER`, `_PASSWORD`) set
  they run against that server instead (external mode): no install, no
  start or stop, the connection arguments of every `psql`, `pg_dump` and
  `pg_restore` taken from there (the client tools are `PG_CONFIG`'s: pgrx's
  pg18 build by default, PGDG's `postgresql-client-18` in CI's docker job).
  `concurrency.sh`, `notify.sh`, `dump.sh`, `extension.sh` (its plain role
  gets a password, for servers that do not trust TCP connections),
  `memory.sh` and `bench_sql.sh` work in both modes; `upgrade.sh` (it copies SQL scripts
  into the server's extension directory) and `replication.sh` (its own
  scratch cluster) refuse external mode rather than test something else.
- `mise run lint`: `tests/check_ci.sh` (with actionlint and shellcheck, pinned in `mise.toml`), rustfmt, clippy with `-D warnings` for the default, the
  `pg_test` and the core-only builds, and rustdoc with `-D warnings`.
- `tests/soak.sh` (`mise run soak`, not part of `mise run test`; a
  60-second smoke run is part of `mise run docker-test`): a long
  concurrent load against the Docker image, see [Soak test](#soak-test).

### Soak test

What the other tests cannot show is what builds up over many thousands
of statements in long-lived connections: memory that grows (in the Rust
heap, in Postgres' memory contexts, or only in RSS), latency that
drifts, tables and TOAST that bloat. `tests/soak.sh` runs a realistic
load for as long as asked and samples all of it.

```sh
mise run soak                                   # 60 minutes, 8 clients, documents up to 1 MB
SOAK_DURATION=2h SOAK_CLIENTS=16 mise run soak  # longer, more connections
SOAK_DOC_SIZE=200 SOAK_DURATION=10m mise run soak
SOAK_SMOKE=1 bash tests/soak.sh                 # 60 s, 4 clients, 200 kB (docker-test runs this)
```

Other knobs (`tests/soak.sh` lists them all): `SOAK_ROWS` (rows before
the run, default 200), `SOAK_LANE_CHANGES` (250), `SOAK_MEMORY` (the
container's limit, 4g), `SOAK_MAX_LOAD_MEMORY` (512MB),
`SOAK_SHARED_BUFFERS` (256MB), `SOAK_TRIM_THRESHOLD`,
`SOAK_SAMPLE_INTERVAL` (30 s), `SOAK_VACUUM_INTERVAL` (600 s),
`SOAK_WEIGHTS` (`"write=30 spans=0"`), `SOAK_OUT` (default
`target/soak/<UTC time>`), `SOAK_KEEP=1` (keep the container to look
around afterwards).

Setup: one container of the image (labelled `pg-automerge-test`) with
`--memory` and no swap, `default_toast_compression=lz4`,
`log_autovacuum_min_duration=0`. Fixtures
(`examples/gen_soak.rs`, deterministic): 20 template documents, from
1 kB boards (a map with a list of small maps, a counter, a status) and
2-8 k-character notes (paragraphs, headings, bold and link marks) to
lists of small maps of a twentieth to a fifth of `SOAK_DOC_SIZE`, a rich
text of a tenth, and a list and a rich text of about `SOAK_DOC_SIZE`. Row
`id` of `soak_docs` is a copy of template `id % 20`; each row has three
writer "lanes", each a chain of single-change edits by its own actor
(`save_after` of the change, so change `k` depends on change `k - 1`),
plus the compressed full save every 25 changes. `soak_cursor` holds each
lane's position, so a write applies exactly the next change and the
lanes of a row are concurrent writers. The table has a generated
`data jsonb` column with a GIN (`jsonb_path_ops`) index (`fastupdate =
off`, see the results) and an `automerge_notify` trigger.

Load: pgbench (the image's own, in a second container sharing the
server's network namespace, so the host needs only `psql`) with
long-lived connections and prepared statements (`-M prepared`), a
weighted mix of one-transaction scripts
(`tests/soak/*.sql`): incremental writes (`UPDATE .. SET doc = merge(doc,
changes)`, `write` for the small and medium rows, `write_big` for the big
ones), the same from every client to the four newest rows
(`write_hot`: writers of different lanes wait for the row lock and merge
into the committed version), upserts of full saves (`upsert`), new rows
(`create`, so the table grows through the run), re-sent changes skipped
by `WHERE NOT automerge_contains` (`resend`), jsonb operators
(`read_ops`, `read_big`), the stored bytes (`fetch`), GIN containment
queries and reads of the generated column (`read_gin`),
`automerge_spans` (`spans`), the history functions (`history`),
`merge_agg` over four copies of a template (`merge_agg`), and
self-sampling (`monitor`: the backend inserts its own
`automerge_memory_usage()` and `pg_backend_memory_contexts` totals, as
the README suggests for pooled connections). A `psql` session `LISTEN`s
throughout; `VACUUM (ANALYZE)` runs every 10 minutes on top of
autovacuum.

Samples every 30 seconds: every postgres process's RSS (anonymous, file,
shared; `/proc` in the container), the container's cgroup memory, table
heap/TOAST/index sizes, live and dead tuples, `pgstattuple_approx` free
and dead space of the heap and the TOAST table, autovacuum counts, WAL
bytes, the notification queue, commits, rollbacks and deadlocks, the
average stored size and change count of each size class, and the
notifications received. pgbench logs every transaction.
`tests/soak_report.py` turns it into `report.txt`: latency percentiles
per script for each sixth of the run (and their drift), transactions per
second, each backend's anonymous RSS over time (the high of each half,
slopes), the counters and memory contexts per window and per backend, the
container's memory, the table, TOAST, bloat and WAL per window.

Checks (the exit status): no failed transaction or aborted client; no
`ERROR` (but an autovacuum cancelled by the manual `VACUUM`), crash,
panic or restart in the server log, and no OOM kill; every
row has exactly the changes its lanes wrote (`automerge_change_count` =
the template's + the lane positions: no write lost or applied twice under
concurrency); the generated column equals `doc::jsonb`; at most three
heads per row; exactly one well-formed notification per write that
changed a document (none for the re-sends); no live document and at most
64 kB allocated between statements in every memory sample; no backend's
memory contexts more than 1 MB higher in the second half of its samples
than in the first; and, for runs of 20 minutes or more, no backend's
anonymous RSS reaching a new high in the second half more than 64 MB
above its high in the first (after a 5-minute warm-up): a leak keeps
raising the high-water mark, memory `malloc` keeps for reuse moves below
it (and a sample can catch a load's Rust peak and its `palloc` memory at
once). Don't query the server while it
runs (an `ERROR` of your own fails the log check); `SOAK_KEEP=1` keeps it
for afterwards.

Results: [Soak test (2026-10-01)](#soak-test-2026-10-01).

## Installation, schema and privileges

`pg_automerge.control` sets:

| Flag | Value | Why |
|---|---|---|
| `relocatable` | `true` | `CREATE EXTENSION .. SCHEMA x` and `ALTER EXTENSION pg_automerge SET SCHEMA y` both work (below) |
| `superuser` | `true` | the functions are `LANGUAGE c` |
| `trusted` | `false` | a database owner must not be able to install it without a superuser (below) |

### Relocation

`relocatable = true` needs every member object in the extension's one
schema, and nothing that names that schema or resolves a member by name
when it runs. What was checked:

- **The install script** names no schema (no `@extschema@`, which
  Postgres does not substitute for relocatable extensions anyway, and no
  qualified name). `CREATE EXTENSION` runs it with `search_path` set to the
  target schema (then `pg_temp`, with `pg_catalog` implicitly first), so
  every member lands there; `tests/extension.sh` checks that no member
  (type, array type, composite type, function, aggregate, operator) is
  anywhere else. Casts have no schema.
- **Catalog references are OIDs**: the type's I/O functions, the casts'
  functions, the operators' functions and `COMMUTATOR` (the `||` on two
  documents is its own commutator), `merge_agg`'s transition and final
  functions, `merge`'s `SUPPORT` function, the composite types' columns and
  every `COMMENT`. `ALTER EXTENSION .. SET SCHEMA` changes only the
  namespace of the members, and each of these follows.
- **The C code never finds its own objects through `search_path`**: the
  history functions build rows with their declared result type (looked up
  by the function's OID); `automerge_notify()` looks the `automerge` type
  up in the schema of its own function (`tgfoid`), so it finds the
  `automerge` columns wherever the extension lives and whatever the
  session's `search_path` holds (tested with a foreign type named
  `automerge` first on the path); the OID that the Rust types report for
  `automerge` (`IntoDatum::type_oid`) is looked up in the extension's
  current schema (`pg_extension.extnamespace`). The last one used to be
  `regtypein('automerge')`, which resolves through `search_path`; no
  current code path calls it, but it would have returned a same-named type
  earlier on the path (a pg_test pins the new behaviour).
- **Objects that depend on the extension** store parsed expressions with
  OIDs: column types, domains, views, expression indexes, stored generated
  columns, check constraints, triggers and SQL functions with a
  `BEGIN ATOMIC` body. They keep working after a move (their definitions
  then print the new schema, e.g. `OPERATOR(ext2.||)`), and indexes and
  generated columns need no rebuild: the functions are the same.
  `tests/extension.sh` moves the extension under all of these and compares
  bytes, heads, jsonb and the generated column, uses the indexes, writes
  through the moved `merge`, runs an in-place PL/pgSQL merge and
  `automerge_changes`, and checks the trigger's notification.
- **What a move does break**, as for any relocated extension: everything
  that resolves names when it runs — application queries, string-bodied
  SQL and PL/pgSQL functions, `SET search_path` clauses of functions,
  `ALTER ROLE/DATABASE .. SET search_path`. Unqualified `automerge`,
  `merge(..)` and the `||` operator are found only through `search_path`
  (or as `schema.merge`, `OPERATOR(schema.||)`); the casts to `jsonb` and
  from `bytea` are found without it.
- **Dump and restore**: `pg_dump` writes `CREATE EXTENSION .. WITH SCHEMA
  <current schema>` and qualifies every reference, so a moved extension
  restores into the schema it was moved to (tested with a custom-format
  dump, which is then moved again).
- pg_test builds put the tests in a schema `tests` as members of the
  extension, so `SET SCHEMA` fails there ("not in the extension's
  schema"); release and dev builds have no such members.

### Why the extension is not trusted

A trusted extension can be installed by any role with `CREATE` on the
database; its script runs as the bootstrap superuser. The review, per the
PostgreSQL documentation's "Security Considerations for Extensions":

- **Input amplification: was the blocker, now bounded.** A few bytes of
  Automerge input can describe a document that takes orders of magnitude
  more memory and time to load. A compressed save of one 4,000,000-character
  text is 4 kB (Automerge deflates the columns; about 1,000 characters per
  byte); the bytea cast validates it (load and save) in 4.3 s (one
  uninterruptible Automerge call, see
  [Implementation notes](#implementation-notes)) with a peak of 390 MB of
  backend memory, outside Postgres' memory accounting, and it scales
  linearly. Automerge allocates with the Rust global allocator, and a
  failed Rust allocation aborts the process: reproduced with a server
  limited to 700 MB of address space and a 12 kB input, the backend died
  with `memory allocation of 768000064 bytes failed` / signal 6, and the
  postmaster restarted every session of the cluster. The panic guard
  cannot catch an abort, and an OOM kill does the same. Type input
  functions are called without an `EXECUTE` check, so `REVOKE` cannot
  close this: anyone who can write to an `automerge` column can send such
  input. Every path that takes client bytes (text and binary input, `COPY`,
  the `bytea` cast, `merge(automerge, bytea)`,
  `automerge_contains(automerge, bytea)`) and every merge result is now
  priced from its chunk headers and column metadata before Automerge
  allocates, and refused over `pg_automerge.max_load_memory` with `53400`
  (see [Resource limits](#resource-limits)); `tests/limits.sh` reproduces
  the crash in a memory-capped cluster with the limit off and gets clean
  errors with it on. The setting is `PGC_SUSET`, which Postgres enforces
  whoever installed the extension, so a database owner could not raise it
  in a trusted install either.
- **Why it stays `trusted = false` for now.** The bounds rest on a cost
  model of Automerge 0.12 made of the worst measured cost of each unit
  (every generated, crafted and fuzzed input stays at or below 0.81 of the
  estimate), not on a proof: an input shape the battery does not cover
  could cost more than estimated, and then a failed allocation still
  aborts the backend. Memory stays outside Postgres' accounting, every
  session can use up to the limit at the same time (plus the stored
  documents a merge loads, which are not priced), and a load still cannot
  be cancelled (up to about 16 s at the default). Those are acceptable
  when a superuser decides to install the extension into a database;
  `trusted = true` would let any database owner decide it. Revisit when
  the model has had more exposure (and loads can be cancelled).

The rest of the review found nothing that would stop it:

- No `SECURITY DEFINER` function; every function, including the trigger,
  runs as the calling role (for the trigger: the role doing the write).
- `automerge_notify()` calls `Async_Notify` directly with its channel
  argument (checked to be 1 to 63 bytes), so there is no SQL to inject
  into; the payload is JSON built and escaped in Rust, the table name
  quoted, the key values rendered by `to_json`'s machinery (the key types'
  output functions, as the caller). It finds the `automerge` type by OID
  (above), not through `search_path`.
- Functions with `internal` arguments (`automerge_recv`,
  `automerge_merge_support`, `automerge_merge_agg_trans`, `automerge_merge_agg_final`) cannot
  be called from SQL, and only a superuser can create an aggregate with
  an `internal` state or attach a `SUPPORT` function, so no other role can
  hand them a foreign state.
- The install script creates every object (no `CREATE OR REPLACE`, nothing
  pre-existing is altered), references only its own objects and
  `pg_catalog` types, and an object of the same name already in the
  target schema makes it fail rather than be adopted (`CREATE TYPE
  automerge` of an existing shell type fails too). In a trusted install
  the members would belong to the bootstrap superuser, so the installing
  role could not, for example, add a `WITHOUT FUNCTION` cast from `bytea`
  that skips validation (that needs ownership of a type).
- `pg_automerge.verify_writes` and `pg_automerge.max_load_memory` are
  `PGC_SUSET`, which Postgres enforces whoever installed the extension.
  Tested for a non-superuser that owns the database (both settings): `SET` before the library is loaded leaves a placeholder
  that Postgres discards with a WARNING when the library defines the
  setting; `SET` afterwards, `ALTER ROLE .. SET` and `ALTER DATABASE ..
  SET` fail with `42501`; `GRANT SET ON PARAMETER` still delegates it.
- Recursion: the jsonb walk is iterative and stops at a nesting depth of
  1000 (`json::MAX_DEPTH`); the history functions and the header parser do
  not recurse. Set-returning functions materialize their rows in Rust
  memory (for `automerge_changes`, the rebuilt change bytes), bounded by
  the document's history, which fit the load memory limit when it was
  written (reads peak at 1.0 to 1.5 times a load).

## Versioning and upgrades

The extension version is the crate version (`default_version =
'@CARGO_VERSION@'` in `pg_automerge.control`; both crates carry it),
currently 0.3.0; pgrx generates the install script
`pg_automerge--X.Y.Z.sql` for the build, and the Docker image's version
label and tag come from it (`scripts/versions.sh`). Released: 0.1.0 and
0.2.0 (see `CHANGELOG.md`).

Policy:

- A release's generated script is committed as
  `sql/snapshots/pg_automerge--X.Y.Z.sql` (`cargo pgrx schema pg18 -o
  ...`) and never edited again.
- Any change to the SQL surface afterwards bumps the version, and comes
  with a hand-written `sql/pg_automerge--A--B.sql` from the previous
  version (`cargo pgrx install` and `package` ship every
  `sql/pg_automerge--*--*.sql`, so the Docker image does too; the
  snapshots below `sql/snapshots/` are not shipped). C symbols that an
  older version's script references stay exported: the new library
  serves the old catalog from the moment it is installed until the
  `ALTER EXTENSION .. UPDATE`.
- A library-only change (a bug fix, a faster path) needs no new version
  as long as no result changes; one that changes a result of an
  `IMMUTABLE` function (the jsonb view) is called out in the changelog
  with the `REINDEX`/rewrite it needs (see below).

### What 0.1.0 shipped

The version was 0.1.0 for the whole development up to the bump, and
`sql/snapshots/pg_automerge--0.1.0.sql` was regenerated when
`automerge_spans` was added (720c5f0), which the policy above forbids once
a version is out. It had been: the Docker image of 0.1.0 (built from
f061a3b plus uncommitted changes that did not touch the SQL surface)
installs a script without `automerge_spans`. The snapshot is now that
image's `/usr/share/postgresql/18/extension/pg_automerge--0.1.0.sql`,
byte for byte (read from the image with a throwaway container). It has
the same statements as the snapshot committed at 0918f56 (the last
commit before `automerge_spans`); only pgrx's order differs, the
`automerge_notify()` block sitting before instead of after the one
of the history types `automerge_change` and `automerge_change_meta` (pgrx does not order unrelated objects
deterministically across builds). Its library exports every symbol of
that script, and so does 0.2.0's.

Images built from the tree after 720c5f0 and before the bump were still
labelled 0.1.0, but their install script created `automerge_spans`
(identical to 0.2.0's, statement for statement). Their library has the
deep-block crash ([Deep blocks](#deep-blocks)) if it was built from a
commit up to 555110e (the last commit before the fix, ee574e1), but not
necessarily if it was built from a working tree: the fix was made
uncommitted on top of 555110e, and an image built from that tree is
labelled revision `555110e-dirty` and has it (7c6d11f23a6f, skjera's
`pg-automerge:0.1.0` at the time of the bump: the 2,000-level deep block
of the regress fixtures reads as jsonb there, and crashes the backend of
the released b946069dce9a). A `-dirty` label cannot tell which, so the
docs do not promise either for such images; reading that document as
jsonb in a throwaway container does. Databases created by these images
exist, and their catalog is the same whatever the library, so it is kept
as `sql/snapshots/variants/pg_automerge--0.1.0+spans.sql` (the script of
555110e; ee574e1, the one commit after it before the bump, changed only
the library), a variant of 0.1.0 that the tests update with 0.1.0's
upgrade scripts like the release.

### 0.2.0 to 0.3.0

The install scripts differ only by `automerge_memory_usage()`,
`automerge_memory_reset()` and their comments (see
[Memory observability](#memory-observability)); no 0.2.0 object's
definition, labels, symbol or comment changed, and the library's other
changes (the altered-change-types check, the reserved settings prefix,
the virtual key column refusal, detoasting in place) return the same
results, so `sql/pg_automerge--0.2.0--0.3.0.sql` creates the two
functions and nothing else. Plain `CREATE FUNCTION`: no 0.2.0 catalog
(nor 0.1.0 or its variant) has them, and a function of the same
signature that someone put into the extension's schema makes the update
fail ("already exists") rather than being replaced; `tests/upgrade.sh`
checks that for every older snapshot. From 0.1.0, `ALTER EXTENSION ..
UPDATE` runs 0.1.0 → 0.2.0 → 0.3.0 in one transaction.

### 0.1.0 to 0.2.0

The install scripts differ only by the two `automerge_spans` functions
and their comments: no 0.1.0 object's definition, labels, symbol or
comment changed (the deep-block fix is inside the library and returns
the same jsonb), so `sql/pg_automerge--0.1.0--0.2.0.sql` creates those
and nothing else, and no dependent object (index, generated column,
trigger, view) is touched. It uses `CREATE OR REPLACE FUNCTION`: it
creates the functions on a released 0.1.0, and redefines them identically
in place (same signature, same OID, so views on them survive) on the
`0.1.0+spans` variant. That is safe in an extension script because
PostgreSQL refuses to replace an object the extension does not own ("is
not a member of extension"): a function of the same signature that
someone else put into the extension's schema makes the update fail
rather than being adopted. Names stay unqualified, as in pgrx's install
script, so the extension stays relocatable: `ALTER EXTENSION .. UPDATE`
runs the script with `search_path` set to the extension's current schema
and then `pg_temp`, with `pg_catalog` searched first, so `jsonb` and
`text` are the built-in types and `automerge` the extension's own. The
script starts with the usual `\echo .. \quit` guard against running it
with `psql`.

### Upgrade tests

- `tests/upgrade.sh` (part of `mise run test`) installs every snapshot
  and variant under a scratch version name (with the first hop of each
  of its version's upgrade paths, which must be installed byte for byte as
  in `sql/`), creates what a deployed database has on it (a STORED
  generated `doc::jsonb` column, GIN and B-tree expression indexes, a
  view and a SQL function over the extension's functions, an
  `automerge_notify()` trigger, documents including rich text and a
  2,000-level deep block, and on a catalog with `automerge_spans` a view
  on it), reads them all before the update (the old catalog on the new
  library), runs `ALTER EXTENSION pg_automerge UPDATE`, and compares the
  extension's catalog with a fresh `CREATE EXTENSION` (`tests/catalog.sql`:
  member objects and their comments, the members' dependencies, function
  definitions with their labels, symbols and ACLs, aggregates, types,
  casts, operators, the extension row). Then the documents' fingerprints
  (bytes, heads, the cast, the generated column, the view, the SQL
  function), the indexes (valid, used, and agreeing with a sequential
  scan), the trigger's notification and `automerge_spans`. It repeats the
  update on an extension relocated with `SET SCHEMA` first (every member
  ends up in that schema), and, from a version without `automerge_spans`,
  over a user's `automerge_spans` in the extension's schema (the update
  fails and the version stays). A snapshot of the current version is
  required and compared as is, so an SQL change without a version bump
  fails.
- `tests/docker.sh` does the snapshot part again with the image's own
  scripts and library (see [Docker image](#docker-image)).
- `tests/docker_upgrade.sh` (`mise run docker-upgrade-test`, not part of
  `docker-test`) upgrades a real 0.1.0 deployment: a container of the 0.1.0
  image (`PG_AUTOMERGE_OLD_IMAGE`, a tag or an image ID, or by default
  built from 0918f56 with that commit's own `scripts/docker-build.sh`;
  its install script must be the 0.1.0 snapshot or a variant) creates
  tables with a generated `doc::jsonb` column, GIN and B-tree expression
  indexes, a view, a notify trigger and documents; it is stopped and the
  new image started on the same volume (init not re-run, still 0.1.0, the
  same data, and the deep block, which crashes the released 0.1.0 library in the
  generated column, stored fine); `ALTER EXTENSION pg_automerge UPDATE`;
  then the same data, the indexes valid, used and agreeing with a
  sequential scan, the trigger's notification, `automerge_spans` (54000
  on the deep block), the catalog equal to a fresh install's in the same
  container, and clean server logs. It follows the README's compose
  steps as written: the cluster's superuser is an app's own
  `POSTGRES_USER`, not `postgres` (as in skjera, which has no `postgres`
  role), the dump of step 1 and the `ALTER EXTENSION` and version check
  of step 5 are the README's commands (read from it, `docker compose
  exec` run as `docker exec`, `<user>` and `<db>` filled in), and a
  service with `image:` and a `build:` section with `args:
  PG_AUTOMERGE_VERSION` (skjera's shape) fails to build with the old
  version (the Dockerfile's version guard) and builds after step 3's
  edit. Run against the released image
  (b946069dce9a), against an image of the `0.1.0+spans` variant, and with
  the image built from 0918f56: all pass.

Data compatibility:

- Stored values are Automerge's `save_nocompress()` format, which any
  later Automerge must load. `crates/pg_automerge_core/tests/fixtures/
  automerge-<version>/` holds documents saved by each automerge version
  the extension shipped with (written by `examples/gen_format_fixtures.rs`;
  0.12.0 now), and `tests/format_fixtures.rs` checks that they still load
  with the same heads and jsonb, and that normalizing them still gives the
  same bytes.
- The automerge crate is pinned (`=0.12.0`). Upgrading it, or changing the
  jsonb mapping, must be called out in the release notes:
  - A different jsonb result for the same document changes what the
    `IMMUTABLE` cast returns, so expression indexes on `doc::jsonb` must
    be rebuilt (`REINDEX`) and stored generated columns rewritten (e.g.
    `UPDATE t SET doc = doc`), or they disagree with fresh computations.
  - A different canonical encoding (the format fixture test fails) means
    re-saved values differ from stored ones: `doc::bytea` of old rows
    stops matching new saves of the same history (heads do not change),
    and each row is re-encoded on its next write.
- Text output (`\x` + hex of the stored bytes) is the dump format and is
  stable, so dump and restore works across versions.

## Docker image

`docker/Dockerfile` builds the official `postgres:18` image plus the
extension; `compose.yaml` runs it for local development. Usage is in the
README's Docker section. Packaging decisions:

- **One pinned base for every stage.** The `versions`, `builder` and
  `runtime` stages are all `FROM ${PG_IMAGE}`, by default
  `postgres:18-trixie@sha256:<index digest>` (Debian 13, Postgres 18.6 at
  the time of writing). Building in the runtime's own image means the
  library is linked against exactly the glibc it runs with, and compiled
  against the headers of the very Postgres it is loaded into: the builder
  installs `postgresql-server-dev-18=$PG_VERSION` (the official image's own
  package version) from the PGDG repository the image already configures,
  and `cargo pgrx init --pg18 /usr/lib/postgresql/18/bin/pg_config`, so
  pgrx never builds a Postgres of its own (whose assertions and paths
  differ). If PGDG has dropped that exact version the newest 18.x headers
  are used, with a warning: minor releases keep the server ABI. The digest
  (rather than the tag) makes a rebuild reproducible and a base update a
  visible one-line change; the price is that security fixes arrive only
  when someone updates it (the README says how). The tag stays next to the
  digest for readers. `tests/check_ci.sh` checks that every stage uses
  `PG_IMAGE` and that it is a digest-pinned `postgres:18-*`.
- **No hand-copied versions.** `scripts/versions.sh` reads Rust and
  cargo-pgrx from `mise.toml`, checks cargo-pgrx against the pgrx crate in
  `Cargo.lock`, and the crate version from `Cargo.toml`; the `versions`
  stage runs it and hands the builder only `/toolchain.env`. BuildKit keys
  that `COPY` by content, so editing `mise.toml` or `Cargo.toml` does not
  rebuild the toolchain layers unless a toolchain version changed.
  `check_ci.sh` fails if the Dockerfile spells out any of those versions.
  The crate version is also a required build argument
  (`PG_AUTOMERGE_VERSION`, for the OCI version label, which a Dockerfile
  cannot read from a file); the build fails if it disagrees with
  `Cargo.toml`. `mise run docker-build` passes it, plus the git remote
  (empty while there is none) and commit for the `source` and `revision`
  labels. The remote goes through `scripts/oci-source-url.sh` first: the
  label is public (every archive and pushed image carries it), so an
  https remote's credentials are stripped, an ssh or scp-style remote
  becomes its https URL, and anything else (a local path) gives an empty
  label; `check_ci.sh` tests those cases.
- **Toolchain from Debian's rustup.** `rustup` comes from the signed
  Debian archive (no `curl | sh`) and installs the pinned toolchain
  (minimal profile); `cargo install --locked cargo-pgrx`. The package is
  built by `scripts/package.sh`, the script behind `mise run package` and
  the tag CI job, which now passes `--locked` to cargo, so a stale
  `Cargo.lock` fails the build instead of being updated. BuildKit cache
  mounts keep the cargo registry and `target/` between builds; since
  `target/` is a cache mount, the package is copied out to `/out` in the
  same step. The build fails if the package holds anything besides the
  library, the control file and SQL scripts.
- **Runtime: files only.** The runtime stage copies `pg_automerge.so`
  (to `pg_config --pkglibdir`), the control file and SQL scripts (to
  `--sharedir/extension`; the builder asserts these are the paths the
  `COPY` uses), `LICENSE` as `/usr/share/doc/pg_automerge/copyright` (the
  MIT notice must travel with copies), the version, and the init script.
  The entrypoint, `CMD`, user and volume are the official image's
  (`check_ci.sh` rejects `ENTRYPOINT`/`CMD`/`USER`/`VOLUME` in the
  Dockerfile). The library links only `libc` and `libgcc_s`. The build
  targets the generic CPU of its architecture (no `target-cpu=native`),
  so the image runs on any host of it. Nothing in the Dockerfile is
  architecture-specific: the base index, PGDG's `postgresql-server-dev-18`
  at the base's exact version and Debian's `rustup` all exist for arm64,
  and CI builds and tests the arm64 image on tags (see [Docker image in
  CI](#docker-image-in-ci)).
- **Init script.** `10-pg-automerge.sh` runs `CREATE EXTENSION IF NOT
  EXISTS pg_automerge` in `POSTGRES_DB` on first initialization, unless
  `PG_AUTOMERGE_CREATE_EXTENSION=0`; any other value than `0`/`1` fails
  initialization rather than guessing. It is executable, so the entrypoint
  runs it in its own process instead of sourcing it into its shell. Its
  `psql` connects like the entrypoint's own `docker_process_sql`, over the
  socket with `PGHOST` and `PGHOSTADDR` cleared: set in the container's
  environment for other tools, they would send it over TCP to the
  temporary server, which listens only on the socket. A bind mount over
  `/docker-entrypoint-initdb.d` (the usual way to add init files) hides
  it, and the extension would silently be missing; the README says to
  mount single files instead, and the same script is installed as
  `/usr/local/bin/pg-automerge-initdb` for a mounted directory's own
  script to run. It
  does not touch `template1`: the extension is not trusted (see [Why the
  extension is not trusted](#why-the-extension-is-not-trusted)), and a copy
  in the template would install it into every database a `CREATEDB` role
  creates.
- **Settings are suggested, not baked in.** The image keeps Postgres'
  defaults; `compose.yaml` passes `default_toast_compression=lz4` (the
  image's Postgres is built `--with-lz4`; stored values are uncompressed
  saves, see [The `automerge` type](#the-automerge-type)) and
  `pg_automerge.max_load_memory=1GB` on the command line, which the README
  explains. A memory-limited container makes the limit matter more: a
  failed allocation or an OOM kill restarts the cluster.
- **Compose.** Named volume at `/var/lib/postgresql` (the 18 images'
  `PGDATA` is `/var/lib/postgresql/18/docker`, so a later major's data
  directory can sit next to it for `pg_upgrade --link`), port on
  `127.0.0.1` only, `shm_size` above Docker's 64 MB for parallel query,
  and a `pg_isready` healthcheck over TCP, which fails during first
  initialization (the entrypoint's temporary server listens only on the
  socket), so the service is healthy only once the extension is created
  and the real server runs. `pull_policy: never`: the image is local.
- **Tests.** `tests/docker.sh` (`mise run docker-test`, after
  `docker-build`; not part of `mise run test` because it needs Docker and
  a release build; CI's `docker` job runs it) checks the labels, the
  version file and license, that no toolchain is in the image, lz4
  support, the exact set of extension files (the install script, the
  control file, the library, and every `sql/pg_automerge--*--*.sql`
  upgrade script the repository has) and a `source` label without
  credentials. Then, against fresh
  containers and volumes (labelled `pg-automerge-test`, port 5432
  published on a free port of 127.0.0.1, all removed on exit; helpers in
  `tests/docker_lib.sh`):
  - default init: installed in `POSTGRES_DB` at the crate version, not in
    `postgres` or `template1`, `debug_assertions` off;
  - SQL through the container's own `psql`: text and bytea casts
    round-trip, `||` (documents and bare changes), `merge` and
    `merge_agg` of the regress fixtures' concurrent edits agree (heads and
    content: `merge_agg` in another order may lay out the bytes
    differently), jsonb operators on the type, `22P02` for corrupt input,
    lz4 on an `automerge` column; a GIN `jsonb_path_ops` index on
    `(doc::jsonb)` that `doc @> ...` uses (the implicit cast matches the
    index expression) and a STORED generated `doc::jsonb` column; the
    history functions against each other (change counts, changes since
    the base's heads, their bytes merged onto the base give the same
    heads, jsonb at the base's heads is the base, `automerge_get_change`,
    `automerge_contains`);
  - `automerge_notify()` with a separate `LISTEN` session (it holds a
    statement open until the writer is done, so the delivery is
    deterministic): payload table, op, key, heads and prev_heads;
  - the settings' defaults, and a plain role's `SET` of either refused;
  - `pg_dump -Fc` in one container, `pg_restore --exit-on-error` into a
    second one initialized with `PG_AUTOMERGE_CREATE_EXTENSION=0` (the
    dump's `CREATE EXTENSION` installs it): same bytes, heads, jsonb and
    generated columns, column compression, the index valid and used, the
    trigger present;
  - a restart on the same volume (data kept, init not re-run, `ALTER
    EXTENSION .. UPDATE` a no-op), `PG_AUTOMERGE_CREATE_EXTENSION=0` (then
    a manual `CREATE EXTENSION .. SCHEMA`), an invalid value failing init;
  - the upgrade path with the image's own scripts and library, as
    `tests/upgrade.sh` does against pgrx's Postgres: each
    `sql/snapshots` version and variant is copied into the container under a scratch
    version name, with the first hop of each upgrade path the image ships
    (so an image missing an upgrade script fails), documents are stored,
    `ALTER EXTENSION .. UPDATE` reaches the current version and the
    documents read the same (the catalog comparison stays in
    `upgrade.sh`; a real 0.1.0 deployment moved to the image is
    `tests/docker_upgrade.sh`, see [Upgrade tests](#upgrade-tests));
  - `PGHOST=localhost` and `PGHOSTADDR` in the container's environment
    with an init file mounted as a single file next to ours (the
    extension is created, the file runs after it), and a whole directory
    mounted over `/docker-entrypoint-initdb.d` whose own script runs
    `pg-automerge-initdb`;
  - the regress examples through `pg_regress --use-existing` (the same
    expected output as `mise run regress`), and `concurrency.sh`,
    `notify.sh`, `dump.sh`, `extension.sh` and `memory.sh` in
    `tests/lib.sh`'s external mode, against one container;
  - `limits.sh` in container mode, against a container started with
    `--memory=1g --memory-swap=1g` (`DOCKER_TEST_MEMORY`) and the default
    limit: without the limit, six of the seven crafted inputs get the
    backend killed by the cgroup's OOM killer (signal 9) and the
    postmaster restarts every session, which is what a production
    container would do; with the limit, every path gets `53400` and
    nothing restarts (no new "terminated by signal", the sentinel session
    alive, `pg_postmaster_start_time()`, the container's start time and
    restart count unchanged). The seventh input, a compressed change
    chunk listing 20,000,000 empty other actors, gets Automerge's own
    `22P02` without the limit instead: it reserves a large table that it
    barely touches before rejecting the chunk, which an address-space cap
    (`ulimit -v`, the scratch cluster) refuses and a used-memory cap (the
    cgroup) does not. The limit refuses it first either way.
  - a 60-second smoke run of `tests/soak.sh` (`SOAK_SMOKE=1`: 4 clients,
    documents up to 200 kB, 60 rows, its own container with
    `--memory=2g`), with all its checks (see [Soak test](#soak-test));
  - every container's log (`docker logs`) is free of `TRAP:`, `PANIC:`,
    Rust panics and backends terminated by a signal or a non-zero exit
    (not any process's: the logical replication launcher exits with 1 at
    every shutdown, including the init server's); the limits container
    may show its deliberate crashes only;
  - `compose.yaml` (`up --wait` healthy, settings applied, port on
    127.0.0.1, `down -v` leaves no volume);
  - the release scripts: `scripts/docker-archive.sh` names the archive by
    version and architecture, `scripts/docker-push.sh --dry-run` loads it
    back as the same image (the ID it prints) and prints the pushes it
    would make; loading an archive whose tag (e.g. `pg-automerge:<version>`)
    points to another image locally, or to none, leaves that tag as it
    was; and it
    refuses a repository without a registry host (Docker would send it to
    Docker Hub), another version's archive and a duplicate architecture.

  The whole run takes about 6 minutes once the image is built (about 2
  of them the suites, 1.5 the soak smoke run, up to 1 the archive).
  `mise run docker-bench-sql` (`tests/docker_bench.sh`) runs
  `tests/bench_sql.sh` against a container of the image (see the
  appendix).

Measured 2026-09-30 on an 8-core Intel Atom C3758R (2.4 GHz), Docker 29.8
with BuildKit:

| | |
|---|---|
| Image size | 460 MB (`postgres:18` is 457 MB: the extension adds 3.5 MB, of which the library is 3.47 MB) |
| Cold build (fresh builder, base pulled) | 462 s: apt 51 s, Rust + cargo-pgrx 168 s, the extension (fat LTO) 205 s |
| Warm build, nothing changed | 1.3 s |
| Warm build, one source file changed | 65 s (dependencies cached in the `target/` mount; the release profile's fat LTO and one codegen unit dominate) |

### Docker image in CI

The `docker` job of `.github/workflows/ci.yml` runs on every push and pull
request, in parallel with the `ci` job (it needs neither pgrx nor its
Postgres): it builds the image with `scripts/docker-build.sh` (the same
build arguments as locally) on a `docker-container` BuildKit builder, and
runs `tests/docker.sh` in full, the suites included, with PGDG's
`postgresql-client-18` as the client tools
(`PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config`; `tests/lib.sh` takes
any Postgres 18 client in external mode) and Rust from `mise.toml` for the
suites' fixture generators. The nightly run also benchmarks the image.

- **Layer cache in the GitHub Actions cache** (`type=gha`, one scope per
  architecture, `mode=max` so the builder stage's layers are exported, not
  only the final image's; `ignore-error=true` so a cache hiccup does not
  fail a build). BuildKit's cache *mounts* (cargo registry, `target/`) are
  not exported, so on a runner a source change recompiles the
  dependencies; the apt and toolchain layers (Rust, cargo-pgrx: 3.5 of the
  cold build's 7.5 minutes) come from the cache. Persisting the mounts too
  (e.g. a cargo-chef-style dependency layer, or copying the mounts in and
  out of the cache) would save most of the rest, at the price of a more
  involved Dockerfile; not done. `docker build` in a `run:` step reaches
  the cache through `crazy-max/ghaction-github-runtime`, which exposes the
  runtime token that `docker/build-push-action` would otherwise handle;
  the build stays in the one script that local builds use.
- **Tags: amd64 and native arm64.** On a `v*` tag (checked against the
  crate version first) the job is a matrix of `ubuntu-24.04` and
  `ubuntu-24.04-arm`; each leg builds and tests its own image natively and
  uploads it as a workflow artifact (`scripts/docker-archive.sh`: a gzipped
  `docker save`, 177 MB for amd64). The repository variable
  `PG_AUTOMERGE_ARM64=false` drops the arm64 leg where GitHub's arm64
  runners are not available. Alternatives considered for arm64:
  - *QEMU* (`docker buildx build --platform linux/arm64` on an amd64
    runner): no change to the Dockerfile, but every compiler process of
    the builder stage runs emulated. The cold build is about 6 minutes of
    compilation (cargo-pgrx, then the extension with fat LTO); user-mode
    emulation typically slows `rustc` by an order of magnitude, so that
    becomes an hour or more per cold tag build, and a source change still
    recompiles everything that is not in the layer cache. Not measured
    here: that would need QEMU's binfmt handlers registered in this
    machine's kernel, a host-wide change.
  - *Cross-compilation* (an amd64 builder targeting
    `aarch64-unknown-linux-gnu`). Tried on 2026-09-30 in a throwaway
    container of the builder stage (adding `gcc-aarch64-linux-gnu`,
    `libc6-dev-arm64-cross` and the Rust target): it works with pgrx
    0.19.3, but only with hand-made flags. bindgen needs
    `--sysroot=/usr/aarch64-linux-gnu` and then fails on ICU's headers,
    which `pg_locale.h` includes, until the amd64 system headers are added
    after the sysroot (`-idirafter /usr/include`); the linker needs
    `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER`. The library built
    (an AArch64 ELF, 4 min 36 s for a dev build, the same as native) and
    `cargo pgrx package --target aarch64-unknown-linux-gnu` generated the
    SQL (28 entities, as natively). But the bindings come from the amd64
    `pg_config.h` (the arm64 `postgresql-server-dev-18` cannot be
    co-installed with the amd64 one; its headers would have to be
    unpacked by hand), and the result could not be run or tested on the
    build host. A second, fragile build path for an image nobody tested.
  - *Native arm64 runner* (chosen): the unchanged Dockerfile, no
    emulation, and the whole of `tests/docker.sh` (memory-limit crashes,
    `pg_regress`, the suites) runs on the architecture it ships for. Its
    cost is the dependency on GitHub's arm64 runner and a second cold
    build per tag, in parallel.
- **Publishing is prepared, not enabled.** The `publish` job runs on tags
  after both `ci` and `docker` passed, downloads the tested archives and
  pushes them with `scripts/docker-push.sh`: each archive is loaded (the
  local tags it carries are put back afterwards, so a by-hand run with
  the arm64 archive on an amd64 host does not repoint
  `pg-automerge:<version>`), checked (version label equals the crate version, architecture equals
  the file name's), tagged `<image>:<version>-<arch>` and pushed, then
  `docker buildx imagetools create` makes `<image>:<version>` and
  `<image>:latest` an index of them. Pushing the tested bytes rather than
  rebuilding for the registry means what is published is what passed.
  Every step after the first is gated on the secret
  `PG_AUTOMERGE_REGISTRY_TOKEN` (plus the variables for the image and
  user): without it the job logs a notice and succeeds. The workflow's
  token is `contents: read`; `tests/check_ci.sh` fails if any step outside
  `publish` logs in or pushes, if a `publish` step is not gated, if
  anything asks for write permissions or `push: true`, or if the job stops
  depending on both test jobs. The repository name must start with a
  registry host, so a typo cannot send the image to Docker Hub.
- **Linting the workflow.** `mise.toml` pins `actionlint` and `shellcheck`
  (prebuilt binaries); `check_ci.sh` runs actionlint on the workflow
  (syntax, expression types, runner labels, and shellcheck on every
  `run:` script) and shellcheck (warnings and errors) on the scripts CI
  runs, then the job invariants above. Each invariant was checked by
  breaking it once.

Measured 2026-09-30 on the same machine, with a fresh `docker-container`
builder per build and a local cache export standing in for the GitHub
Actions cache (the same export format; the upload to GitHub is not
included):

| | |
|---|---|
| Cold build, exporting the cache | 9 min 42 s: 7 min 30 s building (apt 52 s, Rust + cargo-pgrx 163 s, extension 210 s), 2 min 5 s exporting it |
| Exported cache (`mode=max`) | 693 MB per architecture (the GitHub Actions cache holds 10 GB per repository) |
| Fresh builder, cache imported, nothing changed | 34 s |
| Fresh builder, cache imported, one source file changed | 5 min 36 s (221 s compiling the extension and its dependencies) |
| `tests/docker.sh` with PGDG's client tools | 239 s, including 35 to 60 s for the archive and the push dry run |

GitHub's standard runners have 4 vCPUs (this machine: 8 slower cores), so
the absolute times there will differ; the proportions should not.

## Future work

### Native `->` / `->>` operators (evaluated, not implemented)

The performance work found that `automerge -> text` / `automerge ->> text`
operators reading one field without converting the whole document could
roughly halve single-field reads. They are not implemented; this is the
evaluation, for a decision before the first release.

- **Benefit.** Today `doc->>'status'` resolves to `jsonb ->> text` through
  the implicit cast: load the document, convert all of it to jsonb, take
  one key. A native operator would load the document and convert only the
  value at that key (`doc.get(ROOT, key)` and the walk of that subtree).
  The load itself stays: Automerge has no partial load. The gain is the
  walk and jsonb build of everything else, from the primitives in
  [Performance](#performance): on the 877 kB list document about 100 ms
  (72 ms walk, about 30 ms build) of roughly 260 ms, close to half with
  the per-call overhead; on the 3 MB text document about 0.3 s (the text's
  conversion) of 2.8 s, since the load dominates there. The more of the
  document lies outside the requested field and the smaller its history,
  the larger the share.
- **Expression indexes and plans.** An existing index on `(doc->>'x')`
  was created when that text resolved to `(doc::jsonb) ->> 'x'`, and
  stores that expression. Once a native `->>(automerge, text)` exists,
  the same query text resolves to it (an exact match on the left type
  beats the implicit cast), the expressions differ, and the planner no
  longer matches the index: queries silently fall back to scans until the
  index is recreated. Views, rules and `BEGIN ATOMIC` bodies keep their
  stored (jsonb) resolution. The same applies to generated columns and
  check constraints written as `doc->>'x'`. After a release, adding the
  operators would therefore be a plan-breaking change needing release
  notes (`REINDEX` is not enough; the index must be dropped and created
  from the new expression). Before the first release it costs nothing.
- **Semantics to match exactly.** The operators must return the same
  types as jsonb's (`->` jsonb, `->>` text) and the same values as the
  cast's mapping (counters, timestamps, NaN, bytes, conflicts), so that
  `doc->'a'->>'b'` chains and mixing with jsonb operators stay coherent;
  the subtree conversion must reuse the core's walk, and a test would
  compare `doc->k` with `doc::jsonb->k` over the generated documents.
  `#>` / `#>>` (paths) and array indexes would follow the same pattern;
  `@>`, `?`, jsonpath and the rest keep going through the cast.
- **Naming.** To be a drop-in, the operators must be `->` and `->>`;
  anything else is a new API anyway, which argues for functions instead.
- **Alternative: functions.** `automerge_extract_path(doc, VARIADIC path
  text[]) → jsonb` and `automerge_extract_path_text(doc, VARIADIC path
  text[]) → text`, named after `jsonb_extract_path` / `_text` so jsonb
  users recognize them (a shorter `automerge_get(doc, VARIADIC path
  text[])` was considered; the jsonb-parallel names say what the result
  is). Opt-in: nothing resolves differently, no index stops matching,
  and hot queries are rewritten deliberately; `IMMUTABLE`, so indexable
  too. Their cost is discoverability.
- **Recommendation.** If faster single-field reads are wanted, add the
  functions (no compatibility hazard). 0.1.0 is released without the
  operators; adding them now is an SQL change like any other (a new
  version and upgrade script) and, if at all, needs a test that an index on
  `(doc->>'x')` created afterwards is used. For read-heavy tables the
  stored generated jsonb column stays the fastest option either way
  (no load at read time).

## Appendix: benchmarks

Recorded 2026-09-28 with `mise run bench-expanded` (release build, warm
cache, mean of two runs, milliseconds), before and after expanded values
were introduced (commit 6a5f40f). "Before" is the flat path: every merge
result saved, and loaded again by the next operation.

| Workload | 3.0 MB before | after | 877 kB before | after | 83 kB before | after |
|---|---|---|---|---|---|---|
| `UPDATE .. SET doc = merge(doc, c1)` | 5435 | 5452 | 388 | 379 | 43 | 42 |
| `automerge_heads(merge(doc, c1))` | 5258 | 2642 | 358 | 180 | 40 | 23 |
| `merge(doc, c1)::jsonb->>'status'` | 8141 | 2895 | 784 | 448 | 81 | 48 |
| `automerge_heads(merge(merge(merge(doc, c1), c2), c3))` | 15758 | 2645 | 1034 | 187 | 112 | 24 |
| PL/pgSQL `d := merge(d, c)`, 20 change sets | 105772 | 2697 | 6852 | 217 | 691 | 31 |
| the same, each merge in a `BEGIN .. EXCEPTION` block | 105827 | 2693 | 6868 | 229 | 686 | 30 |
| the same, then `UPDATE .. SET doc = d` | 106136 | 5584 | 6890 | 425 | 699 | 52 |
| 2 merges into `d`, then 10 reads `d->>'status'` | 38960 | 5154 | 4987 | 2805 | 494 | 271 |
| `merge_agg(doc)::jsonb` over 9 versions | 26488 | 24111 | 2006 | 1759 | 210 | 179 |

Heads fast path, recorded the same day on the 3 MB document (before: a
full load for every call): `automerge_heads` 2745 ms → 0.3 ms;
`automerge_contains(doc, ''::bytea::automerge)` 2738 ms → 0.5 ms;
`merge(doc, ''::bytea::automerge)` 2743 ms → 16 ms; `merge` of the document
with a newer version of it 5.3 s → 2.6 s.

### Hardening and performance stage (2026-09-28)

Measured before (commit e4eeb26) and after, same machine, release builds.
`mise run bench-core` (Rust, median of five, milliseconds):

| Operation | 3.0 MB text before | after | 877 kB before | after | 83 kB before | after |
|---|---|---|---|---|---|---|
| `normalize` of a compressed save | 5280 | 2530 | 324 | 169 | 32.7 | 16.7 |
| `normalize` of stored bytes | 2637 | 2488 | 163 | 164 | 16.2 | 16.1 |
| jsonb walk (no-op sink) | 194 (per object) | 247 (one sweep) | 144 | 72 | 13.4 | 6.4 |

(5,000 one-character changes, 5 kB: `normalize` of a compressed save
45.5 → 22.5 ms.) The compressed-save shortcut halves writes of
`Automerge.save()` output; the one-sweep walk halves the walk of
documents made of many small objects and is a quarter slower on one huge
text.

`mise run bench-expanded` (SQL, mean of three runs, milliseconds):

| Workload | 3.0 MB before | after | 877 kB before | after | 83 kB before | after |
|---|---|---|---|---|---|---|
| `bytea` → `automerge` of a compressed save | 5199 | 2617 | 341 | 182 | 36.0 | 20.4 |
| `bytea` → `automerge` of stored bytes | 2594 | 2581 | 178 | 178 | 19.8 | 20.6 |
| `doc->>'status'` | 2845 | 2816 | 439 | 314 | 44.3 | 32.6 |
| `jsonb_array_length(doc->'items')` | 2830 | 2840 | 451 | 317 | 46.1 | 32.7 |
| `merge(doc, c1)::jsonb->>'status'` | 2837 | 2827 | 446 | 320 | 45.5 | 33.9 |
| 2 merges into `d`, then 10 reads `d->>'status'` | 5117 | 5209 | 2816 | 1541 | 278 | 153 |
| `merge_agg(doc)::jsonb` over 9 versions | 23583 | 23136 | 1755 | 1629 | 179 | 168 |
| `UPDATE .. SET doc = merge(doc, c1)` | 5320 | 5246 | 374 | 377 | 39.8 | 41.2 |

The jsonb reads gain from building jsonb directly (no JSON text and
`jsonb_in`: about 60 ms on the 3 MB and 877 kB documents) and, on the
list documents, from the one-sweep walk; on the 3 MB text the two
roughly cancel out.

### SQL paths: redundant loads removed (2026-09-28)

`mise run bench-sql` (release build, median of three, milliseconds; one
load of the 3 MB text takes about 2.5 s, of the 877 kB list about 160
ms). "newer" is the stored document plus one change by another actor as a
compressed save; "changes" its `save_after(stored heads)`. Loads counted
by the pg_tests of `src/tests/loads.rs`.

| Path | loads before → after | 877 kB before | after | 3.0 MB before | after |
|---|---|---|---|---|---|
| R1 `doc->>'status'` | 1 → 1 | 310 | 313 | 2817 | 2793 |
| R2 three accessors in one `SELECT` | 3 → 3 | 908 | 921 | 8423 | 8383 |
| R3 `automerge_heads(doc)` | 0 → 0 | 0.5 | 0.5 | 0.5 | 0.5 |
| I1 `INSERT` of newer (bytea) | 1 → 1 | 210 | 218 | 2778 | 2744 |
| W1 `merge(doc, changes::bytea)` | 2 → 2 | 362 | 375 | 5217 | 5205 |
| W2 `merge(doc, newer::bytea)` | 3 → 1 | 505 | 210 | 7390 | 2772 |
| W3 `merge(doc, newer::automerge)` | 2 → 1 | 395 | 209 | 5311 | 2775 |
| W4 upsert of newer, `merge(docs.doc, excluded.doc)` | 2 → 2 | 381 | 373 | 5342 | 5288 |
| W5 W1 again (already there) | 0 → 0 | 35 | 1.0 | 185 | 0.9 |
| W6 W3 with a text parameter (`\bind`, custom plan) | 2 → 2 | 391 | 380 | 5365 | 5344 |

With `pg_automerge.verify_writes = off`, W1 takes one load (207 ms and
2739 ms); the other paths are unchanged (their writes need no check).
W2 no longer loads the stored document plus the save (as `a ++ save`)
and then verifies the result: the save is loaded on its own and, since
it contains the stored document, is the result, verified by its own
encoding. W3's cast hands its loaded document to `merge`. W5 decides from
the heads in a prefix of the stored value and returns the TOAST pointer
it was given, which the `UPDATE` keeps: no detoasting, compressing or
writing of the document. W4 and W6 stay at two loads because Postgres
flattens `EXCLUDED` and constant-folded parameters (see
[Expanded values](#expanded-values)).

The pgrx-managed Postgres these numbers come from is an assert-enabled
build (`randomize_mem` shows up in profiles of the jsonb paths), so the
jsonb part of R1 and R2 is somewhat slower than on a production build.

### jsonb built in Rust memory (2026-09-29)

`JsonbBuilder` assembles the `JsonbValue` tree itself instead of feeding
every event to `pushJsonbValue` (see [jsonb mapping](#jsonb-mapping)); the
jsonb bytes are unchanged. `mise run bench-sql`, release build, median of
seven runs, milliseconds, before and after in one session:

| Path | 877 kB before | after | 83 kB before | after |
|---|---|---|---|---|
| R1 `doc->>'status'` | 313 | 265 | 31 | 27 |
| R2 three accessors in one `SELECT` | 911 | 810 | 91 | 80 |

On the 3 MB text (one string) the build was never the cost: 2812 → 2787
ms for R1 (median of five), within noise. Writes do not build jsonb and
are unchanged. After the change a profile of R1 on the 877 kB list puts
about 47% in the load, 15% in Automerge's document iterator, 4% in the
walk's own bookkeeping and about 7% in building the jsonb
(`JsonbValueToJsonb` 2.4%, `int64_to_numeric` 1%), against about 15% for
the `pushJsonbValue` path before.

### `merge_agg` loads lazily (2026-09-29)

`merge_agg` used to load its first input right away and merge every later
input that its document lacked into it. Its state now keeps the first
stored input as bytes, loads the larger of two inputs first when the heads
cannot decide, and lets a loaded input that contains the whole state
replace it (see [Merging](#merging)). It also skips inputs it already has
by a prefix of their heads, before detoasting them. `mise run bench-sql`,
release build, median of five runs, milliseconds:

| Path | loads before → after | 877 kB before | after | 3.0 MB before | after |
|---|---|---|---|---|---|
| A1 `merge_agg` over one row | 1 → 0 | 170 | 8.5 | 2559 | 22 |
| A2 `merge_agg` over the row and a newer version | 2 → 1 | 335 | 176 | 5061 | 2533 |

A1 is now the detoast of the value and a copy of it into the result. In
A2 the newer version is loaded, contains the older one and is the result
as stored: no merge and no save. The other paths are unchanged. A
profile of R1 on the 877 kB list puts about 77% of the time in Automerge
and another 11% in glibc's `malloc`/`free`, mostly for Automerge's
allocations; linking mimalloc as the Rust global allocator instead was
tried with the core benchmark and rejected (the 877 kB load about 5%
faster, the 3 MB text load about 6% slower).

### One-sweep walk without per-object vectors and hashing (2026-09-29)

The sweep's bookkeeping (a `Vec` of entries per object and a SipHash
`HashMap` from object id to content, about a third of the walk on the
877 kB list) became one entry vector and a sorted id index searched by
bisection (see [jsonb mapping](#jsonb-mapping)); the events are
unchanged. `mise run bench-core`, release build, median of three,
milliseconds: jsonb walk (no-op sink) 74.3 → 64.3 on the 877 kB list,
6.6 → 5.7 on the 83 kB list, 250 → 250 on the 3 MB text (one object).
`mise run bench-sql`, median of seven, two before/after pairs in one
session:

| Path | 877 kB before | after | 83 kB before | after |
|---|---|---|---|---|
| R1 `doc->>'status'` | 268, 268 | 257, 261 | 28, 26 | 26, 25 |
| R2 three accessors in one `SELECT` | 807, 809 | 782, 777 | 80, 77 | 76, 80 |

### Containment decided by change counts (2026-09-29)

`automerge_contains` loaded the stored document to answer `false` for a
newer or concurrent version, as a stored value or as a save in a `bytea`
(one load, 164-167 ms on the 877 kB list, 2.5 s on the 3 MB text). When
the heads do not decide, it now compares change counts read from a
prefix of each value (or a save's header): a version with at least as
many changes as `a` and other heads is not in `a` (see
[Reading](#reading)). Older versions still cost one load (true needs the
history). `mise run bench-sql`, release build, median of five, ms:

| Case | 877 kB before | after | 3 MB before | after |
|---|---|---|---|---|
| C1 `automerge_contains(doc, newer save bytea)` | 167 | 1.2 | 2568 | 9.0 |
| C2 `automerge_contains(doc, newer stored version)` | 164 | 1.2 | 2549 | 1.1 |

C1 on the 3 MB text is the save's checksum (a SHA-256 over 3 MB) and the
parameter's detoast. No other path changed.

### Header no-ops check that the save parses (2026-09-29)

A save whose header lists heads the document already has was a no-op
after only its framing and checksum were checked, so `merge(doc, bytea)`
and `automerge_contains(doc, bytea)` accepted a checksummed chunk with
known heads and a malformed body, which a load of `doc ++ bytes` (and so
the earlier `merge`) rejects with `22P02`. The header answers that say
"contained" now first run `header::document_parses` (see
[Merging](#merging), case 1), which inflates any deflated columns.
Answers of "not contained" and paths that load the save are unchanged.
Release build, median of seven, ms:

| Case | 877 kB before | after | 3 MB before | after |
|---|---|---|---|---|
| `merge(doc, own compressed save)` (no-op) | 1.2 | 2.9 | 8.9 | 32 |
| `automerge_contains(doc, own compressed save)` | 1.1 | 3.1 | 8.9 | 31 |
| `merge(doc, doc::bytea)` (uncompressed, no-op) | 5.8 | 5.8 | 15.6 | 14.1 |
| W2 `merge(doc, newer save)` | 206 | 214 | 2787 | 2767 |
| C1 `automerge_contains(doc, newer save)` | 1.2 | 1.3 | 9.6 | 9.0 |

The added time is the inflation of the save's columns; loading the save
instead would cost 200 ms / 2.8 s.

### Input amplification measurements (2026-09-29)

What a load of client bytes costs, measured to design the size limits
(release build; peak bytes allocated through the Rust global allocator,
counted by a counting allocator in a scratch harness; "doc" is
`Automerge::load` of a document chunk, "chunks" is the same history
loaded as bare change chunks, as `merge(doc, bytea)` applies them).

Small inputs that describe a lot. Automerge's columns are run-length
encoded, so a run of `n` identical (or equally spaced) values costs a
few bytes whatever `n` is, before any deflate:

| Input | Bytes | Describes | Peak | Time |
|---|---|---|---|---|
| one change chunk, loads fine (a list of nulls) | 104 | 1,000,001 ops | 561 MB | 3.4 s |
| save of a list of 400,000 nulls (`save_nocompress`) | 174 | 400,001 ops | 36 MB doc, 231 MB chunks | 0.4 s, 1.6 s |
| save of 200,000 one-op changes | 196 | 200,000 changes | 194 MB | 1.3 s |
| compressed save of a 1,000,000-character repetitive text | 1,165 | 1,000,001 ops | 93 MB | 1.0 s |
| crafted document chunk: 1,000 changes with 10,000 deps each | 66 | 10^7 deps | 1.41 GB | 3.4 s, then rejected |
| crafted document chunk: one op with 10^6 successors | 134 | 10^6 succ entries | 328 MB | 3.7 s, then rejected |
| crafted document chunk: 100,000 empty changes | 71 | 100,000 changes | 128 MB | 0.3 s, then rejected |
| crafted change: 2 ops, one with 10^6 preds | 71 | 10^6 preds | 50 MB | 0.2 s |
| crafted document chunk: `max_op` 10^8, 3 ops | 115 | nothing | 14 kB | - |

So neither the stored size (174 bytes for any number of nulls), nor the
inflated size, nor a declared `max_op` bounds memory. The crafted
documents fail only after reconstruction ("mismatching heads"), with the
memory already spent. Memory is linear in each of these quantities
(checked at 10^7 ops: 910 MB and 11.3 s as a document, 6.5 GB and 49 s
as change chunks), with these costs per unit (peak bytes):

| Unit (readable from headers) | doc | chunks |
|---|---|---|
| op, appended in counter order (text typed or pasted, list append) | 89-91 | 530-650 |
| op out of counter order, object creation, distinct map key, mark | 280-372 | 550-810 |
| change | 900-1,275 | 1,400-2,300 |
| dependency entry | 141 | - |
| successor entry (text deletes: one per element) | 13 | - |
| successor entries pending on one key (put/delete toggling, crafted) | 328-590 | - |
| pred entry | - | 50 |
| inflated byte (string, bytes values) | 5-7 | 7 |
| change × actor (Automerge's clock cache every 16 changes) | 0.25-0.3 | 0.25-0.3 |
| change with more than ~16 ops × actor (reconstruction) | 3 | - |

Most of a load's peak is transient: after loading, a 1,000,000-character
text holds 1 MB (the op set is columnar); the peak comes from rebuilding
every change. Reads of a stored value stay within 1.0-1.5 times its
load's peak (`automerge_changes`, `automerge_changes_meta`, the jsonb
walk into a `serde_json::Value`), and a merge of two concurrent
1,000,000-character documents peaks at 566 MB, almost all of it applying
the other document's changes (the chunks cost). Time follows memory: at
most about 8 s per GB of the estimate below on the test machine.

A linear estimate over these quantities with the worst measured cost of
each (450 bytes per op, 1,600 per change and an encoder term, 200 per
dependency, 30 per successor plus 600 per successor pending on one
(object, key) group, 200 per actor, 10 per inflated byte, 0.3 per change
× actor and 3 per large change × actor; for change chunks 1,000 per op,
80 per pred, 2,500 per change) stays above the measured peak for every
generated and crafted input tried: actual/estimate is at most 0.80 (ops
prepended to a list of maps), about 0.2 for appended text and lists, and
0.1 for a key overwritten 200,000 times in one change. Allocation
failure itself cannot be turned into an ERROR: `std::alloc::
set_alloc_error_hook` is unstable in Rust 1.98, a `#[global_allocator]`
must not unwind, and allocating with `palloc` would `longjmp` through
Automerge's frames.

### Load memory limit (2026-09-29)

`pg_automerge.max_load_memory` (see [Resource limits](#resource-limits)),
implemented from the measurements of the previous entry.

Measured peaks against the estimate (`tests/memory_bounds.rs`, dev build
with optimized dependencies, counting allocator; peak / estimate):

| Input | save | compressed | change chunks |
|---|---|---|---|
| 200,000 characters of text | 0.20 | 0.20 | 0.57 |
| 3,000 typed characters (3,000 changes) | 0.40 | 0.40 | 0.41 |
| 50,000 characters deleted | 0.21 | 0.21 | 0.39 |
| 40,000 ints prepended to a list | 0.69 | 0.71 | 0.63 |
| 20,000 maps prepended to a list | 0.80 | 0.80 | 0.67 |
| 40,000 map keys | 0.62 | 0.67 | 0.54 |
| one key overwritten 40,000 times | 0.10 | 0.10 | 0.45 |
| one key set and deleted 20,000 times | 0.55 | 0.56 | 0.34 |
| 5,000 one-op changes | 0.33 | 0.33 | 0.46 |
| 1,000 actors / 1,000 conflicting forks | 0.30 / 0.38 | 0.31 / 0.41 | 0.43 / 0.43 |
| 400 actors, then 400 changes of 16 ops | 0.32 | 0.32 | 0.41 |
| 6,000 characters with 2,000 marks | 0.39 | 0.42 | 0.67 |
| 1 MB of bytes | 0.49 | 0.71 | 0.69 |

Crafted chunks (bytes, peak / estimate): a document chunk of 20,000 empty
changes (77 bytes) 0.71, of 1,000 changes with 1,000 dependencies each (70
bytes) 0.68, of one op with 200,000 successors (136 bytes) 0.48, of a
200,000-item list (141 bytes) 0.17; a change chunk of 100,000 ops (104
bytes) 0.58, of one op with 1,000,000 preds (71 bytes) 0.08 (priced as the
document it normalizes to, where the preds become successors on one key);
a save followed by 2,000 change chunks 0.34. Change chunks whose header
lists repeat entries (added after review, which found the first version
priced other actors by name: a 200 kB input estimated just under 2 GB
aborted a scratch cluster capped at 4 GB): 1,100,000 or 2,200,000 empty
other actors, plain or compressed (1-2 kB), 0.64, 1,100,000 copies of one
16-byte actor 0.32, 1,100,000 copies of one dependency (compressed, 34 kB)
0.36; in release they take 1.3 s per GB of the estimate, the battery's
worst being 5.5 s per GB (change chunks of 1,000 actors). Merges of two concurrent
documents (both loaded, the other's changes applied): 0.21 to 0.38 of the
two loads plus the apply estimate. Refused inputs peak below 64 kB for the
run-length bombs (100 to 221 bytes describing 10^8 to 2^62 rows) and below
a quarter of the limit for deflate bombs (256 MB of zeros, stopped at a
tenth of a 100 MB limit). A 200,000-input release fuzz session (the
properties of [Testing](#testing)) found no violation.

`tests/limits.sh`: under `ulimit -v 1000000`, with the limit off, the
12 kB compressed save of a 12,000,000-character text and a 113-byte change
chunk describing 20,000,000 ops each abort the backend (`memory allocation
of 1543503872 bytes failed`, signal 6) and the cluster restarts; with the
default limit both are refused with `53400` through every path and nothing
restarts. 32 s, most of it generating the text.

Cost (release build, median; before is the previous commit, two runs
each). `mise run bench-core`, ms:

| | before | after |
|---|---|---|
| normalize, 3 MB text stored / compressed | 2498 / 2526 | 2495 / 2567 |
| normalize, 877 kB list stored / compressed | 161 / 166 | 166 / 172 |
| normalize, 83 kB list stored / compressed | 16.0 / 16.6 | 16.1 / 17.4 |
| the scan alone, 877 kB list stored / compressed | - | 1.7 / 3.7 |
| the scan alone, 3 MB text stored / compressed | - | 0.0 / 21.6 |

`mise run bench-sql` (`BENCH_REPS=5`, ms; before: the mean of two runs):

| case | 83 kB before | after | 877 kB before | after | 3 MB before | after |
|---|---|---|---|---|---|---|
| R1 read (unchanged code) | 25 | 26 | 261 | 265 | 2789 | 2780 |
| R2 three reads (unchanged code) | 77 | 78 | 781 | 806 | 8362 | 8333 |
| I1 insert of a compressed save | 21 | 22 | 205 | 210 | 2756 | 2752 |
| W1 merge(doc, changes) | 38 | 38 | 364 | 368 | 5204 | 5223 |
| W2 merge(doc, newer save) | 22 | 23 | 209 | 211 | 2771 | 2791 |
| W3 merge(doc, save::automerge) | 22 | 24 | 206 | 214 | 2765 | 2793 |
| W4 upsert | 41 | 39 | 369 | 382 | 5283 | 5318 |
| W6 W3 as a text parameter | 39 | 39 | 374 | 383 | 5312 | 5382 |
| A2 merge_agg of two versions | 18 | 18 | 172 | 176 | 2547 | 2532 |
| R3, W5, A1, C1, C2 (no load) | unchanged | | | | | |

The differences are of the size of the drift of the read-only cases R1
and R2, whose code did not change (up to 3%). The first version of the
scan decoded every value and computed Gmax exactly for every document,
10.7 ms on the 877 kB list (+6% on its writes); counting literal runs
without decoding them and taking Gmax at its bound unless that decides a
refusal brought it to 1.7 ms.

Remeasured at the final verification, interleaved (the 72e8d57 build,
then this one, three pairs of `mise run bench-sql` runs, 5 to 9
repetitions each; mean of the medians, ms), the write cost is above the
drift after all on the smaller documents, while the read-only R1 stays
within 0.5%:

| case | 83 kB before | after | 877 kB before | after | 3 MB before | after |
|---|---|---|---|---|---|---|
| R1 read (unchanged code) | 26.0 | 26.0 | 260 | 261 | 2781 | 2765 |
| I1 insert of a compressed save | 21.3 | 23.0 (+8%) | 201 | 213 (+6%) | 2754 | 2765 (+0.4%) |
| W1 merge(doc, changes) | 37.7 | 41.0 (+9%) | 360 | 374 (+4%) | 5196 | 5219 (+0.4%) |
| W2 merge(doc, newer save) | 22.3 | 23.7 (+6%) | 208 | 215 (+4%) | 2788 | 2790 (+0.1%) |
| W3 merge(doc, save::automerge) | 22.0 | 23.0 (+5%) | 205 | 215 (+5%) | 2770 | 2802 (+1.2%) |
| W6 W3 as a text parameter | 38.3 | 40.3 (+5%) | 373 | 390 (+5%) | 5345 | 5363 (+0.3%) |

(3 MB: one pair.) `bench-core`, two pairs: normalize of the 877 kB list
161.9 → 164.6 ms stored, 167.7 → 175.6 ms compressed (+5%), of the 3 MB
text within 1%. So the price of the limit is about 4-6% on writes of
list-shaped documents of 0.1-1 MB (1-2 ms at 83 kB, 7-15 ms at 877 kB,
of which the scans of a compressed input and of its normalized result
account for 5.4 ms; the rest was not attributed), and about 1% on a
large text, whose columns are few long runs. Recovered in the next
entry. Reads are unchanged; a
merge of stored values pays the scan of its result (A2, `merge_agg` of
two versions: 174 → 181 ms at 877 kB, one pair).

### The limit's write cost recovered (2026-09-29)

Profiled with `perf` on an LTO-off symbolized build of the extension
(backends running I1 and W1-W3 in a loop) and timed piece by piece in
the core, the write cost of the limit was all in the scans, and the
unattributed part was the inflation of a compressed save done three
times: by the scan, by `Automerge::load`, and by `header::inflate_document`
for the save-and-load shortcut, which also hashed the inflated chunk
(together 4.1 ms of `normalize`'s 173 ms on the 877 kB list). Two
changes:

- The scan keeps the columns it inflated for a single compressed save
  (`budget::scan_input_keep`) and `header::inflated_document_is` compares
  the normalized save with them in place: 0.19 ms instead of 4.1 ms (on
  the 3 MB text 0.7 ms instead of 28 ms), so a compressed save is
  inflated twice, as before the limit (without a limit, three times:
  nothing is kept). The columns are held through the load, after its
  estimate is checked, at most the inflated bytes, each charged 10: the compressed saves
  of the memory battery peak a little higher (1 MB of bytes 0.61 → 0.71
  of the estimate, 40,000 map keys 0.65 → 0.67), the worst input is still
  0.80.
- `column_stats` steps over literal numbers by their LEB128 last bytes,
  eight bytes at a time, over strings by their lengths without the
  general decoder, and reads one-byte run headers inline: the scan of the
  stored 877 kB list takes 0.7 ms instead of 1.7 ms (compressed: 2.6
  instead of 3.7, most of it the inflation). A unit test checks it
  against decoding every value on 20,000 generated and corrupted columns.

Interleaved `mise run bench-sql` runs (the 72e8d57 build before the limit,
the previous commit, this one; median of the per-run medians of 9
repetitions, six rounds, 3 MB text three rounds of 5; ms) and user-space
instructions per statement (`perf stat` on the backend, 10-20
statements):

| case | doc | before the limit | limit | this | instructions: limit | this |
|---|---|---|---|---|---|---|
| I1 | 83 kB | 21.4 | 22.4 (+5%) | 21.0 (-2%) | +2.6% | +0.2% |
| I1 | 877 kB | 203.2 | 207.9 (+2%) | 199.7 (-2%) | +2.4% | +0.0% |
| I1 | 3 MB | 2745 | 2774 (+1%) | 2740 (-0.2%) | | |
| W1 | 83 kB | 37.6 | 38.0 (+1%) | 37.7 (+0.4%) | +0.6% | +0.3% |
| W1 | 877 kB | 361.7 | 367.2 (+2%) | 363.9 (+0.6%) | +0.7% | +0.3% |
| W1 | 3 MB | 5159 | 5245 (+2%) | 5221 (+1%) | | ±0.0% |
| W2 | 83 kB | 22.1 | 22.8 (+3%) | 22.4 (+1%) | +2.5% | +0.2% |
| W2 | 877 kB | 205.4 | 213.2 (+4%) | 204.3 (-0.5%) | +2.4% | +0.1% |
| W2 | 3 MB | 2770 | 2785 (+0.5%) | 2756 (-0.5%) | | |
| W3 | 83 kB | 22.1 | 22.7 (+3%) | 22.0 (-0.4%) | +2.5% | +0.2% |
| W3 | 877 kB | 205.7 | 211.5 (+3%) | 204.2 (-0.8%) | +2.4% | +0.0% |
| W3 | 3 MB | 2771 | 2771 (0%) | 2755 (-0.6%) | | |

What is left is the scan of the stored document a merge of change
chunks applies them to (W1, 0.3% of its instructions; its time is within
the drift, and on the 3 MB text W1 executes the same number of
instructions as before the limit, ±0.004%, so its +1% is drift too). In
`bench-core` `Automerge::load` itself, which did not change, varies by
2-4% between the three binaries (code layout), so its `normalize` is best
compared net of the load in the same binary: on the 877 kB list a
compressed save costs 8.2 ms beyond its load before the limit, 13.7 with
the limit, 7.3 now; a canonical one 2.6, 5.4 and 3.8 (three runs each).

### Rebuilt copies and column metadata (2026-09-30)

A defensive test round against the load limit (release build, counting
allocator) found four kinds of input whose peak the estimate did not
bound, because Automerge copies something the input holds once, per
change or per op, or keeps per-entry structures the estimate did not
price. Peak and estimate in MB (2^20 bytes), before and after the terms
of [The estimate](#the-estimate) that price them:

| Input (bytes) | Peak | Estimate before | after |
|---|---|---|---|
| Document chunk, 2,000 empty changes sharing one 100 kB message (a repeat run; 100 kB, or 209 bytes deflated) | 766 | 4.6 (167x) | 958 (0.80) |
| Document chunk, 3 changes sharing one 30 MB message (30 MB) | 372 | 286 (1.30x) | 572 (0.65) |
| Document chunk, 2,000 empty changes by a 100 kB actor (100 kB) | 766 | 4.6 (167x) | 958 (0.80) |
| Document chunk, 2,000 changes of a 16-byte actor, each putting a key in a map a 100 kB actor made (100 kB) | 766 | 5.7 (135x) | 960 (0.80) |
| Document chunk, 2,000 changes sharing one 100 kB key (100 kB) | 384 | 5.7 (68x) | 578 (0.67; 959 before keys were priced below messages, below) |
| Change chunk, 2,000 puts of one 100 kB key (100 kB) | 192 | 3.1 (63x) | 1,528 (0.13) |
| Change chunk, 2,000 marks with one 100 kB name (100 kB) | 383 | 3.1 (125x) | 1,528 (0.25) |
| Change chunk, 500 maps each given one 50 kB key between two others (52 kB; `normalize`, with its save) | 141 | 2.1 (67x) | 239 (0.59) |
| Document chunk, 2,100,000 / 4,200,000 empty columns (4.2 / 8.4 MB) | 240 / 480 | 40 / 80 (6.0x) | 441 / 881 (0.54) |
| Change chunk, 2,100,000 / 4,200,000 empty columns (4.2 / 8.4 MB) | 336 / 672 | 40 / 80 (8.4x) | 441 / 881 (0.76) |
| The same compressed (4 / 8 kB) | 348 / 696 | 40 / 80 (8.7x) | 441 / 881 (0.79) |

Where Automerge 0.12 copies:

- Messages: `ChangeCollector::collect` rebuilds every change of a
  document as a `StoredChange` whose bytes hold the message and whose
  `message` is a `String` copy of it (`op_set2/change/collector.rs`,
  `finish`); `Document::verify_changes` clones every rebuilt change into
  `MismatchedHeads` when the heads do not match. 4.0 bytes per byte of
  the expanded run.
- Actor ids: `ActorId` is a `TinyVec<[u8; 16]>`, so a longer id is a heap
  copy wherever it is cloned; every rebuilt change holds its own actor
  and the other actors its ops refer to in its bytes and as `ActorId`s
  (`ChangeCols::actor`, `other_actors`), cloned again into the error.
  4.0 bytes per byte.
- Keys and mark names: a document's rebuilt changes hold their ops' keys
  in their bytes (2.0 bytes per byte with the error's clone); applying a
  change (`import_ops` in `op_set2/change/batch.rs`) makes an owned
  `String` of every op's key (`Key::map`) and mark name, all held until
  the batch is applied (1.0 and 2.0 bytes per byte), and the document's
  key column holds a key literally where other keys come between its
  rows (4.0 with the `String`s, 5.9 with `normalize`'s save).
- Column metadata: `RawColumns::parse` collects the entries with
  `parse::apply_n` into a vector that doubles as it grows, collects them
  again into `RawColumn`s, and the change and document paths copy the
  list once more (`uncompressed()`, `ChangeOpsColumns`,
  `OpSet::validate`): up to 120 bytes per entry (document), 168 (change
  chunk), 174 (compressed), at counts just past a power of two.

The scan computes the expanded sizes from run headers (a repeat run of
`n` strings of `len` bytes: `(n - 1) × len`; an actor column's run:
its length, at most the changes, times the actor's length), so it stays
proportional to the input. It no longer allocates per metadata entry:
before, it kept two vectors per entry (48 bytes per two input bytes),
so refusing such input cost about a third of its estimate; a block whose
extra entries alone exceed the limit now stops the scan before they are
read (12,000,000 entries in 24 kB: refused at the default limit with a
peak under 72 MB, in under 2 s).

Saves written by Automerge of the same shapes load (2,000 commits
sharing one 100 kB message 0.40, one 100 kB key 0.33, by a 100 kB actor
0.40, one 100 kB mark name, whose rows text characters come between,
so the document holds it literally, 0.60), and so do they after another
save, where they are rebuilt and applied as changes (0.15, 0.18, 0.31,
0.60), and through `merge_changes` (0.15, 0.18, 0.31, 0.70). The
crafted document chunks list no heads, so a load after another save,
or `merge_changes`, skips them as already contained (a peak of 32
bytes); with one made-up head they are rebuilt and applied (Automerge
does not check a later chunk's changes against its heads): 0.15 to
0.46 after a save, 0.18 to 0.36 through `merge_changes`.

These terms raise the estimate of ordinary documents too, where they
repeat keys longer than a few bytes or have actor ids longer than 16
bytes (their other inputs moved by at most +1%: map keys repeated by a
run, one key overwritten 40,000 times +0.3%, 400 actors × 400 changes
of 16 keys +0.6%; worst ratio still 0.80). An overwritten key is a
repeat run of the document's key column, and every overwrite a rebuilt
copy; a long actor id is counted per change and per successor. Measured
against the commit before these terms (`save_nocompress`, MB; the
"apply" estimate is of `merge_changes` of the save into another
document), with keys and mark names first priced like messages (5 per
byte), then at 3:

| Document | Estimate before | 5 per key byte | 3 per key byte | Peak (ratio) | Apply estimate before | 5 | 3 | Peak (ratio) |
|---|---|---|---|---|---|---|---|---|
| 8 actors × 2,000 rounds, 20 keys of 64 bytes each round | 28.1 | 40.3 (+43%) | 35.5 (+26%) | 14.1 (0.40) | 74.9 | 106.6 (+42%) | 101.8 (+36%) | 26.9 (0.26) |
| The same, keys of 36 bytes (UUIDs) | 28.1 | 35.0 (+24%) | 32.2 (+15%) | 12.3 (0.38) | 74.9 | 92.7 (+24%) | 90.0 (+20%) | 24.8 (0.28) |
| The same, keys of 4 bytes | 28.1 | 28.9 (+3%) | 28.6 (+2%) | 10.2 (0.36) | 74.9 | 76.9 (+3%) | 76.6 (+2%) | 22.3 (0.29) |
| One 32-byte actor, 20,000 rounds, 20 keys of 64 bytes | 269.5 | 397.7 (+48%) | 345.8 (+28%) | 138.3 (0.40) | 733.0 | 1,066.3 (+45%) | 1,009.5 (+38%) | 295.8 (0.29) |
| Text, 4 actors with 32-byte ids × 5,000 characters, half deleted | 18.2 | 20.5 (+13%) | 20.0 (+10%) | 7.6 (0.38) | 45.8 | 51.8 (+13%) | 50.5 (+10%) | 23.5 (0.46) |
| The same, 64-byte ids | 18.2 | 22.8 (+25%) | 21.9 (+20%) | 7.6 (0.35) | 45.8 | 57.7 (+26%) | 55.3 (+21%) | 23.8 (0.43) |

(The "3" column also caps a change's own and other long actors
together at every long actor once, where before only the others were
capped.) The real cost of the overwritten 64-byte keys is 1.6-1.7 bytes
per expanded byte (`normalize` +4.1 MB for 2.6 MB expanded), under the
3 charged; applied to another document they cost about 1.9 per byte
against 11 charged (3 rebuilt + 8 applied), because the applied keys
are priced for the worst case, where the document they are applied to
has rows between theirs and holds every copy literally (4.0 per byte
measured, 5.9 with `normalize`'s save). So a document whose keys of
tens of bytes are overwritten many times, or whose actor ids are long,
is priced 10-40% higher than before, and one near the limit before
(1.4-1.6 GB at the default 2 GB) can now exceed it on a merge, an
`INSERT` or a restore. The battery holds two such documents now
(`long_keys`: the first row, `long_actors`: the fifth; 0.40 and 0.38 as
saves). A 200,000-
input release fuzz session found no violation. `tests/limits.sh` with
the four inputs (a 1 kB compressed save of 6,000 changes sharing a
100 kB message, a 100 kB document chunk of 6,000 changes by a 100 kB
actor, a 100 kB change chunk of 16,000 puts of one 100 kB key, and a
24 kB compressed change chunk listing 12,000,000 empty columns): each
aborts the capped cluster without the limit, and gets `53400` with it
through every path, with no restart.

Cost, interleaved (the previous commit and this one alternating, five
pairs; median of the per-run medians of five repetitions; release). The
scan alone (`budget::scan_input`, median of seven batches of 2,000):
83 kB list stored 66.4 → 64.2 µs, compressed 293 → 292 µs; 877 kB list
stored 673 → 647 µs, compressed 2,476 → 2,470 µs (no per-entry vectors).
`bench-core` normalize, ms:

| document | stored before | after | compressed before | after |
|---|---|---|---|---|
| 83 kB list | 16.5 | 16.2 | 16.9 | 16.6 |
| 877 kB list | 166.9 | 165.2 | 170.2 | 169.8 |
| 3 MB text | 2488.8 | 2480.9 | 2525.9 | 2528.4 |
| 5,000 typed characters | 22.4 | 22.6 | 22.4 | 22.6 |

`bench-sql`, ms:

| case | 83 kB before | after | 877 kB before | after | 3 MB before | after |
|---|---|---|---|---|---|---|
| R1 read (unchanged code) | 25.7 | 25.6 | 261.6 | 256.0 | 2796 | 2807 |
| I1 insert of a compressed save | 21.1 | 21.7 | 204.5 | 199.8 | 2762 | 2764 |
| W1 merge(doc, changes) | 38.1 | 37.7 | 368.1 | 361.9 | 5275 | 5213 |
| W2 merge(doc, newer save) | 22.5 | 23.0 | 208.2 | 207.1 | 2791 | 2781 |
| W3 merge(doc, save::automerge) | 22.1 | 22.6 | 209.7 | 209.5 | 2776 | 2783 |
| W4 upsert | 38.6 | 38.2 | 372.5 | 368.1 | 5337 | 5302 |
| W6 W3 as a text parameter | 38.7 | 38.7 | 381.2 | 373.6 | 5362 | 5374 |
| A2 merge_agg of two versions | 18.8 | 18.2 | 175.0 | 174.4 | 2566 | 2562 |

All within ±3.1% (at 83 kB, ±0.6 ms either way), about the drift of R1,
whose code did not change (−2.2% at 877 kB); the scan, the only code on
these paths that changed, got faster, by 2 to 26 µs.

### The Docker image against the pgrx Postgres (2026-09-30)

The numbers above were all measured on the pgrx-managed Postgres 18.6,
which pgrx builds with `--enable-cassert` and
`RANDOMIZE_ALLOCATED_MEMORY` (every `palloc` filled with noise). The
Docker image runs the same extension (a release build, the same profile)
on the PGDG Postgres 18.6 without either. `mise run bench-sql` and
`mise run docker-bench-sql` (a throwaway container, default settings like
the pgrx cluster's, over TCP on 127.0.0.1 like the pgrx runs), alternated
two rounds each, `BENCH_REPS=5`, the mean of the two runs' medians, ms:

| case | 83 kB pgrx | image | 877 kB pgrx | image | 3 MB pgrx | image |
|---|---:|---:|---:|---:|---:|---:|
| R1 read | 26 | 25 (−6%) | 258 | 252 (−2%) | 2783 | 2784 (+0%) |
| R2 three reads | 77 | 74 (−3%) | 782 | 760 (−3%) | 8298 | 8343 (+1%) |
| R3 heads | 0.4 | 0.3 | 0.4 | 0.3 | 0.4 | 0.3 |
| I1 insert of a save | 22 | 22 (−2%) | 204 | 200 (−2%) | 2760 | 2728 (−1%) |
| W1 merge(doc, changes) | 37 | 36 (−3%) | 366 | 353 (−4%) | 5211 | 5170 (−1%) |
| W2 merge(doc, newer save) | 24 | 21 (−11%) | 206 | 198 (−4%) | 2760 | 2740 (−1%) |
| W3 the same, cast first | 22 | 21 (−5%) | 206 | 199 (−4%) | 2758 | 2730 (−1%) |
| W4 upsert | 40 | 36 (−10%) | 367 | 364 (−1%) | 5248 | 5244 (−0%) |
| W5 no-op merge | 0.8 | 0.6 | 0.9 | 0.7 | 0.9 | 0.6 |
| W6 W3 as a text parameter | 38 | 37 (−4%) | 372 | 370 (−1%) | 5325 | 5302 (−0%) |
| A1 merge_agg of one row | 1.4 | 0.7 | 8.7 | 4.3 | 24 | 9.8 |
| A2 merge_agg of two versions | 18 | 17 (−6%) | 173 | 170 (−2%) | 2548 | 2518 (−1%) |
| C1 contains(doc, newer save) | 0.9 | 0.4 | 1.2 | 0.7 | 9.1 | 5.2 |
| C2 contains(doc, stored newer) | 1.2 | 0.6 | 1.2 | 0.8 | 1.1 | 0.6 |

Everything that loads a document is within 0 to 6% (up to 11% at 83 kB,
where a run is 20 ms and the noise ±1 ms): the time is Automerge's, in
Rust, and identical code. The paths that avoid the load (heads, no-op
merges, `merge_agg` of one row, containment by heads and change counts)
take about half the time on the image: what is left there is Postgres'
own work (detoasting, `palloc`, the executor), which the assertions and
the memory randomization slow down. The earlier sections' conclusions
hold on a production build; their millisecond-scale absolute numbers
are about twice what a production server shows.

### Deep blocks (2026-09-30)

The jsonb walk before and after it skips Automerge's document iterator
for documents with blocks (see [Deep blocks](#deep-blocks)); release
builds of the previous commit (555110e) and this one, interleaved.

The crash: the Docker image of 0.1.0 built before `automerge_spans`
(the one the reported crash came from) segfaults on
`INSERT INTO t (doc) VALUES (:'deep_block')` into a table with a stored
generated `doc::jsonb` column (the regress fixture: 2,000 levels, 4 kB);
so did `tests/limits.sh` step 3 with the previous commit's library, on
its 5,000-level input. With this commit both pass.

`mise run bench-core` (five rounds, each binary's median of 5; the
median of the rounds, ms; "walk" is the sweep before and the checked
walk, block check included, after):

| doc | load (untouched) | block check | walk before | walk after | per-object walk |
|---|---|---|---|---|---|
| text3mb | 2498 / 2670 | 0.0 | 252.8 | 249.9 | 193.0 |
| items20k | 157.6 / 159.6 | 1.1 | 64.4 | 66.4 | 146.7 |
| items2k | 15.4 / 15.6 | 0.1 | 5.6 | 5.8 | 13.5 |
| typed5k | 21.6 / 22.7 | 0.0 | 0.4 | 0.4 | 0.3 |
| rich20k | 368.4 / 381.1 | 0.0 | 917.2 | 15.1 | 15.1 |

The load, which this commit does not touch, differs by up to 7% between
the two binaries: code layout. The walk after costs the sweep plus the
check (items20k: 65.5 ms for the new binary's own sweep, 66.4 ms checked).

`mise run bench-sql` read paths (R1: `doc->>'status'`, R2: three
accessors, R3: `automerge_heads`; five interleaved rounds of 5, the
median of the rounds, ms), and R1 again (six rounds of 9):

| case | doc | before | after |
|---|---|---|---|
| R1 | items20k (877 kB) | 263 | 262 (-0.4%); again 266 → 264.5 (-0.6%) |
| R1 | text3mb (3 MB) | 2839 | 2902 (+2.2%); again 2878.5 → 2887 (+0.3%) |
| R1 | items2k (83 kB) | 28 | 28 |
| R1 | rich20k (1.7 MB) | 1332 | 424 (-68%) |
| R2 | items20k | 796 | 776 (-2.5%) |
| R2 | text3mb | 8567 | 8625 (+0.7%) |
| R2 | items2k | 78 | 77 |
| R2 | rich20k | 4004 | 1235 (-69%) |
| R3 | all | 0.4-0.5 | 0.4-0.6 (no load, no walk) |

Merge results read as jsonb (expanded values without stored bytes, which
are saved once for the check; five interleaved rounds, each the median of
7, ms): `merge(doc, c1)::jsonb->>'status'` items20k 263.5 → 260.9, items2k
26.1 → 26.3, text3mb 2846 → 2896 (+1.8%); two merges into a PL/pgSQL
variable and ten reads of it items20k 1074 → 1046, items2k 100.9 → 98.1,
text3mb 5196 → 5256 (+1.2%). The added work there is one
`save_nocompress` (7.3 ms on text3mb, kept for when the value is stored)
and the check; the rest of the difference on text3mb is of the size and
sign of the untouched load's above.

### Soak test (2026-10-01)

`mise run soak` as described in [Soak test](#soak-test): 70 minutes, 8
clients, documents up to 1000 kB (`SOAK_DOC_SIZE`; the largest a 1.26 MB
rich text of 1.04 million operations and a 1.07 MB list of 23,000
maps), 200 rows at the start, a 4 GB container, `shared_buffers
= 256MB`, `max_load_memory = 512MB`, the Docker image on this
development machine (8 cores, all busy throughout). Three runs: before
the trim (run 1, image of 453cb9e), with it (run 2), and the final
configuration (run 3: the trim, the GIN index with `fastupdate = off`,
the recalibrated checks).

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| transactions (per client) | 122,769 (15,346) | 121,421 (15,178) | 136,994 (17,124) |
| failed transactions, errors, crashes | 0 | 0 | 0 (one autovacuum cancelled by the manual `VACUUM`) |
| rows at the end | 2,630 | 2,545 | 2,915 |
| lane writes, notifications received | 52,414, 54,844 | 51,475, 53,820 | 57,916, 60,631 |
| rows whose change count is off, generated columns off | 0, 0 | 0, 0 | 0, 0 |
| `allocated_bytes` between statements (max of ~3,600 samples) | 16 B | 16 B | 16 B |
| live documents between statements | 0 | 0 | 0 |
| peak per backend (the 1.26 MB rich text) | 342 MB | 342 MB | 342 MB |
| memory contexts per backend, first → last (slope) | 2.2-2.8 → 3.01 MB (0.02 MB/h) | 2.2-2.8 → 3.11 MB (0.02 MB/h) | 1.5-2.9 → 3.02 MB (0.02 MB/h) |
| backend anonymous RSS, sampled (mostly inside statements) | 200-356 MB throughout | 5-397 MB | 9-355 MB |
| backend RSS high, 2nd half minus 1st (max over backends) | 9.4 MB | 42.7 MB | 20.3 MB |
| container anonymous memory, median per window | 2,348-2,430 MB | 1,305-1,449 MB | 1,441-1,699 MB |
| container anonymous memory, max | 2,642 MB | 2,297 MB | 2,474 MB |
| TOAST size, start of window 1 → end | 191 → 388 MB | 164 → 381 MB | 161 → 458 MB |
| TOAST dead tuples / free space (pgstattuple_approx) | 6-27% / 19-38% | 5-24% / 16-44% | 5-30% / 17-40% |
| autovacuums of the TOAST table | 68 | 69 | 68 |
| WAL (per lane write) | 48.6 GB (950 kB) | 47.7 GB (949 kB) | 75.5 GB (1,336 kB) |
| `read_gin` p50, first → last window | 85 → 499 ms | 84 → 488 ms | 1.4 → 3.0 ms |

What they show:

- **No leak.** The Rust heap went back to 16 bytes between statements in
  every sample of every backend for 70 minutes (about 34,000 loads per
  backend), no document stayed alive, and the memory contexts stayed at
  3.0 to 3.1 MB per backend (the 0.02 MB/h slope is the first samples
  after the backend started; flat after the first window). The backends'
  anonymous RSS did not trend: in run 1 its high was 344-354 MB in the
  first half and 339-356 MB in the second.
- **`malloc` kept what the largest document needed, in every backend
  (fixed).** In run 1 every backend held 200 to 355 MB all the time, for
  documents that need 342 MB while loaded and 16 bytes afterwards:
  see [Returning freed memory](#returning-freed-memory-pg_automergetrim_threshold).
  With the trim (run 2) a backend drops to 5-75 MB after such a
  transaction, and the container's median anonymous memory fell by
  about 1 GB (40%); its maximum (2.3 GB) is the documents being loaded at
  that moment. Throughput was the same (28.9 against 29.2 transactions
  per second, run to run noise on this machine being about as large).
- **GIN with a pending list stops being used under write load
  (documented).** `read_gin`'s median grew with the table, 85 → 499 ms.
  On run 2's data (2,545 rows, kept with `SOAK_KEEP=1`), idle, the
  containment query takes 1 ms through the index (80 ms for the big
  documents' slot: the recheck detoasts their 425 kB jsonb); under the
  same load (150 s, 8 clients) the planner chose a sequential scan
  instead (600 to 900 ms: it detoasts every row's jsonb, 25,000 buffers),
  because the pending list, which every write of a big document fills
  with thousands of keys, is part of the index's cost. With
  `ALTER INDEX .. SET (fastupdate = off)`: the bitmap scan again
  (1.5 ms), `read_gin` 11.6 ms against 466 ms on average, writes 74 ms
  against 68 ms (big ones 3.45 s against 3.60 s), 31.5 against 25.5
  transactions per second. The README recommends `fastupdate = off`, and
  the soak creates its index that way (`SOAK_GIN_FASTUPDATE=on` for
  Postgres' default). Run 3 confirms it over the whole run: `read_gin`'s
  median 1.4 → 3.0 ms (its p95, 16 → 83 ms, is the big documents' slot,
  whose candidates grow with the table and are rechecked against their
  425 kB jsonb), and 32.6 instead of 28.9 transactions per second. The
  cost is WAL: 1,336 kB per write instead of 949 kB (+41%), every write
  inserting its keys into the index tree instead of appending them to the
  pending list (and 90 "checkpoints are occurring too frequently" against
  6). The README says both.
- **WAL and TOAST are the cost of writing whole documents.** 949 kB of
  WAL per write over the mix: every update writes the new TOAST value in
  full, twice with the generated jsonb copy (425 kB against 427 kB
  stored for a big document), plus full-page images after each of the
  frequent checkpoints (`max_wal_size` 1 GB: six "checkpoints are
  occurring too frequently" in run 2). Autovacuum's defaults kept the
  TOAST table's dead tuples under 27% (about one autovacuum a minute),
  its free space was reused, and the table grew with the rows, not with
  the writes (the heap's free space after `VACUUM` 26-28%). README:
  [Operations](README.md#operations).
- **Latency did not drift**, except `read_gin` above and what the
  growing table and documents explain: the small writes' median rose
  from 12 to 16 ms as the rows and their change counts grew (54 → 71
  changes per small document on average) and the client time `read_gin`
  took grew (throughput per window fell about 20% over the run for that
  reason); p95 and p99 of every other script were flat. The big
  documents' scripts (`write_big`, `read_big`, `upsert`) are bimodal
  (`read_big` takes about 0.7 s on the list, 3.2 s on the rich text), so
  their median jumps between the two from window to window.
- Checks recalibrated after run 1 and 2: run 1's "no ERROR" check failed
  because of the author's own queries against the server during the run
  (an `ERROR` from deliberately exceeding `max_load_memory`, now
  documented: don't); its RSS growth check (a least-squares slope) failed
  on noise (−121 to +46 MB/h between backends with no trend), replaced
  by comparing the high-water marks of the two halves, which run 2
  missed at 32 MB (42.7 MB: one sample of 382 MB, a load's Rust peak
  plus its `palloc` memory at the same moment), so the limit is 64 MB;
  the memory contexts got their own check (1 MB). Run 3 passed every
  check but the log check, on one `ERROR:  canceling autovacuum task`:
  Postgres cancels an autovacuum that blocks the harness's own manual
  `VACUUM`, which is expected and now allowed. Run 4 (65 minutes, the
  final harness, pgbench from the image): every check passed; 124,514
  transactions (15,564 per client), 55,460 notifications, 16 bytes
  allocated and no live document between statements, memory contexts
  3.01-3.02 MB, backend RSS high-water growth 11.1 MB, container
  anonymous memory 1.48-1.63 GB at the median per window and 2.34 GB at
  most, `read_gin` median 1.0 → 2.3 ms, 1,305 kB of WAL per write.
