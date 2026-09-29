# pg_automerge design

A Postgres 18 extension (Rust, pgrx 0.19.3, `automerge` crate 0.12.0) that
stores [Automerge](https://automerge.org) documents in an `automerge`
column type, lets you **query them as `jsonb`**, and **merges** concurrent
writes. Only Postgres 18 is supported.

Contents: [Scope](#scope) · [Architecture](#architecture) ·
[The `automerge` type](#the-automerge-type) ·
[SQL API and semantics](#sql-api-and-semantics) ·
[jsonb mapping](#jsonb-mapping) · [Error codes](#error-codes) ·
[Performance](#performance) · [Implementation notes](#implementation-notes) ·
[Testing](#testing) · [Versioning and upgrades](#versioning-and-upgrades) ·
[Appendix: benchmarks](#appendix-benchmarks)

## Scope

In scope:

- Storing Automerge documents produced elsewhere.
- Reading them with every `jsonb` operator/function/index.
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
-- merged (ERROR "duplicate seq").
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
  and the change-chunk splitter (`header`), the history functions
  (`history`), the jsonb mapping as a walk into a `JsonSink` (`json`), the
  notification payload builder (`notify`), text encodings (`encoding`) and,
  with the `test-hooks` feature, test instrumentation (`test_hooks`).
  Every Automerge call runs under a panic guard (see
  [Errors](#errors-and-panics)).
- The root crate (`src/`): the pgrx glue, one module per SQL area:
  `datum.rs` (the Rust types of `automerge` arguments and results, and the
  prefix reads), `expanded.rs` (expanded values), `io.rs` (the type's SQL,
  I/O functions and casts), `merge.rs` (`merge`, `||`, `merge_agg`, the
  support function), `introspect.rs` (`automerge_heads`,
  `automerge_contains`), `history.rs`, `notify.rs` (the trigger),
  `jsonb.rs` (building jsonb from the core's walk) and `error.rs` (raising
  errors). The glue converts datums, calls the core,
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
- **Validated.** Input that does not load is rejected (`22P02`), and so is
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
    change the heads. The core tests compare this path with the full
    load-save-load on generated documents, trailing changes, corrupt and
    non-canonical deflate streams, and the fuzz harness checks it on every
    input.

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
is not acceptable (bytes, text, hashes) is always `22P02`, stored values
that fail are `XX000`.

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

Messages are short and lowercase; supporting data goes in the DETAIL
(e.g. the hashes of missing dependencies) and advice in the HINT (e.g. how
to declare the notification trigger). The LOCATION of an error (shown with
`\set VERBOSITY verbose`) is the source file and line that raised it.

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
    actor id (two histories with the same (actor, seq)): `XX000`
    "duplicate seq".
  - `merge` is an unreserved keyword since PG15; unqualified
    `SELECT merge(a, b)` and `SET doc = merge(doc, ...)` work on PG18
    (tested), so the function keeps the name `merge`.
- `merge(a automerge, changes bytea) → automerge`: applies external bytes on
  top of `a`, like Automerge's `load_incremental`. `changes` may be a full
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
    1. exactly one document chunk with a valid checksum whose header heads
       are heads of `a` (or changes of an expanded `a`): `a` unchanged,
       nothing loaded. A load of `a ++ changes` decides the same from the
       same header: Automerge skips a document chunk whose heads it has.
    2. When the headers say the save has no more changes than a stored `a`
       (an older save), `a` is loaded first and checked for those heads,
       again as that load would.
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
    already has is skipped by Automerge after the checksum check, without
    decoding its columns.
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
  `merge_agg_trans` runs `CHECK_FOR_INTERRUPTS` before each input. When no
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
- `automerge_heads(automerge) → text[]`: current heads as sorted lowercase hex
  change hashes. Read from the header without loading the document (see
  [Heads fast path](#heads-fast-path)).
- `automerge_contains(a automerge, b automerge) → bool`: whether every change
  in `b` is already in `a`, meaning `merge(a, b)` would be a no-op. Decided
  from the two headers when `heads(b) ⊆ heads(a)` (true) or
  `heads(a) ⊊ heads(b)` (false: a head of `b` missing from `a`'s heads has
  no successor in `b`, so it cannot be an ancestor of a head of `a`);
  otherwise `a` is loaded and `b` never is.
- `automerge_contains(a automerge, changes bytea) → bool`: whether `a`
  already has every change in `changes` (the same inputs as
  `merge(automerge, bytea)`), i.e. whether `merge(a, changes)` returns `a`
  unchanged. Empty `changes` is contained. Changes whose dependencies are
  in neither `a` nor `changes` are not in `a`: `false` (where `merge`
  raises `22P02`). Malformed bytes are `22P02` when they have to be loaded.
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
  - For exactly one document chunk (a save) with a valid checksum: whether
    `a` has the heads its header lists, from `a`'s heads, or else from its
    history (a stored `a` is loaded; the save never is). That is what the
    no-op check of `merge(automerge, bytea)`, and a load of `a ++ changes`,
    decide from the same header.
  - Otherwise (older changes, a save plus change chunks, compressed chunks)
    `a ++ changes` is loaded once (no save) and the heads compared.
  - On the no-load and the header paths only the framing, checksums and
    dependency lists or header heads are read, so `false` does not promise
    that `merge` accepts the input.

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
change hashes are only available after reconstructing the changes.

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
`39P01` (trigger protocol violated); fewer than two arguments, an empty
channel or one of 64 bytes or more, a key column listed twice or of type
`automerge` → `22023`; an unknown key column → `42703`; calling
`automerge_notify()` outside a trigger → `0A000`. Errors about how the
trigger is declared carry a HINT with the correct `CREATE TRIGGER`. These
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
  memory is not counted in the context (as for `merge_agg`'s state).
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
  changes, missing dependencies, a duplicate seq, a decoder panic) leaves
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
missing dependencies, duplicate seq, decoder panics) fails inside `merge`.
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
iterator (`ReadDoc::iter_at`), buffering each object's visible entries
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
marks). The previous per-object walk (`json::write_json_per_object`) is
kept for documents with legacy `Table` objects, which the sweep would read
as lists, and as the reference: a core test checks that both emit exactly
the same events for current and historical states of generated documents,
text with blocks, marks and non-string elements, unreachable objects,
conflicts and deep nesting.

Nesting is capped at 1000 levels (error `XX000`): the walk itself uses an
explicit stack, but `convertToJsonb` recurses (it checks the stack depth,
the cap gives a clear error instead).

## Error codes

| SQLSTATE | When | Message (DETAIL / HINT) |
|---|---|---|
| `22P02` invalid_text_representation | Bad text input; bytes that are not a loadable Automerge save or change sequence (including decoder panics); a result that does not survive a save and load | `invalid input syntax for type automerge: ...`, `invalid automerge document: ...`, `invalid automerge changes: ...` |
| `22P02` | `merge(automerge, bytea)` with changes whose dependencies are in neither input | `invalid automerge changes: missing N dependencies that neither the document nor the input contains` (DETAIL: `Missing changes: <hashes>.`) |
| `22P02` | A change hash that is not 64 hex digits | `invalid automerge change hash "...": expected 64 hexadecimal digits` |
| `22023` invalid_parameter_value | `automerge_to_jsonb(doc, heads)` with a head the document lacks; bad `automerge_notify` arguments (count, channel length, key column listed twice or of type `automerge`) | `automerge document does not contain change <hash>`, `automerge_notify(): ...` (HINT when the arguments are missing) |
| `22004` null_value_not_allowed | A NULL element in `since_heads` / `heads` | `since_heads must not contain NULL` |
| `39P01` trigger_protocol_violated | `automerge_notify()` not fired `AFTER ... FOR EACH ROW` for INSERT/UPDATE/DELETE | `automerge_notify() must be fired ...` (HINT: the correct `CREATE TRIGGER`) |
| `42703` undefined_column | `automerge_notify()` key column that does not exist | `automerge_notify(): key column "x" does not exist in table ...` |
| `0A000` feature_not_supported | `automerge_notify()` called outside a trigger | `automerge_notify() can only be called as a trigger` (HINT) |
| `XX000` internal_error | A stored value that does not load (corruption), a merge of two histories that reuse an actor id (`duplicate seq`), nesting deeper than 1000 levels, broken invariants | |

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
  on top of the current heads; for any other single save
  `automerge_contains` loads the stored document, never the save.
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
  returns a `Detoasted` guard holding either the datum and the detoasted
  bytes of a flat value (fetched once per call and used as the core
  `Input`; an unchanged result is the original datum, not the bytes) or
  the loaded document of an expanded one. Neither outlives the call.
  `merge`, `merge(automerge, bytea)` and `automerge_contains` first decide
  what they can from the heads (a prefix of a flat value), before
  anything is detoasted.
- Errors go through one `raise()` (`src/error.rs`) taking a core `Error`
  or a `PgError` (code, message, optional DETAIL and HINT). `raise` and
  `or_raise()` are `#[track_caller]`, so the error's LOCATION is the line
  that raised it.
- Interrupts: the core's long loops (the jsonb walk, building history rows
  and ordering them) call an interrupt check every 1024 steps, which the
  extension registers in `_PG_init` as `CHECK_FOR_INTERRUPTS()`
  (`set_interrupt_check`); `merge_agg_trans` checks between inputs. A
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
  notifications, the core's counts of live loaded documents and of
  `Automerge::load` calls, the forced failure of the save-and-load check
  and an override of `pg_automerge.verify_writes`, behind the
  `test-hooks` feature) is compiled only into test builds.
- `pg_automerge.verify_writes` is defined in `_PG_init` and registered with
  the core (`set_verification_check`), which asks it whenever it would run
  the save-and-load check (`verification_enabled`).

## Testing

- Core (`cargo test -p pg_automerge_core`): unit tests in the modules, and
  in `crates/pg_automerge_core/tests/`: `basics.rs` (normalization, merge,
  the accumulator), `edge_cases.rs` (odd documents, scalars, encodings,
  corrupted bytes), `merge_changes.rs` (`merge(automerge, bytea)` and
  `automerge_contains(automerge, bytea)`), `heads_fast_path.rs` (the header
  parser against `Automerge::load().get_heads()` over hundreds of generated
  documents: empty, up to 12 actors, random forks and merges, merge and
  `merge_agg` outputs, arbitrary prefixes), `history.rs` (checked against
  Automerge's `fork_at` / `get_changes` and a dependency-graph walk, and the
  change count against the loaded change graph), `loaded.rs` (loaded
  documents are byte-identical to the flat path). `common/` holds the
  random-history generator and stored-bytes wrappers around the
  `Input`-based API.
- `#[pg_test]`s (`cargo pgrx test pg18`) for SQL behaviour, in
  `src/tests/{io,merge,history,notify,expanded,hardening,loads}.rs`
  (`loads.rs` counts the `Automerge::load` calls of each write path, and
  covers the release of the cast's expanded values and the
  `pg_automerge.verify_writes` setting). They are `include!`d
  into the `#[pg_schema] mod tests` in `lib.rs` rather than declared as
  submodules, because pgrx runs each test as a function of the `tests`
  schema and only items of that exact module go there. Test documents are
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
  corrupt value failing `COPY FROM` with `22P02`. A restore and `COPY
  FROM` validate every value (one load each; the generated column is
  recomputed, one more conversion).
- `tests/upgrade.sh` (`mise run upgrade`, part of `mise run test`): see
  [Versioning and upgrades](#versioning-and-upgrades).
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
  idempotent), and `merge(automerge, bytea)` results load back with their
  heads. 1,500 inputs per `cargo test`; `mise run fuzz` runs 200,000 (or
  `FUZZ_ITERS`) and can save findings (`FUZZ_SAVE_DIR`) for
  `tests/corpus/`, which runs first. In 300,000 inputs it caught 4,903
  decoder panics (all `22P02`) and 19 inputs that load but whose re-save
  does not ("mismatching heads" on the reload; all of them several
  chunks, so never candidates for the compressed-input shortcut), all
  rejected by the save-and-load check, and found no property violation.
  Two of the latter are in the corpus; the deferred check of `merge`
  results is tested with the test hook (see
  [The deferred verification](#the-deferred-verification)).
- `tests/json_walk.rs`: the one-sweep jsonb walk against the per-object
  walk; `tests/normalize.rs`: the compressed-input shortcut against the
  full check; `tests/interrupts.rs`: the interrupt hook runs in the loops
  and its errors pass the guard; `tests/format_fixtures.rs`: values saved
  by every shipped automerge version (see
  [Versioning and upgrades](#versioning-and-upgrades)).
- `tests/bench_expanded.sh` (`mise run bench-expanded`, not part of
  `mise run test`) installs a release build and times the SQL workloads
  of [Performance](#performance) on three generated documents;
  `tests/bench_sql.sh` (`mise run bench-sql`) times the everyday paths
  (reads, inserts, the merge and upsert forms of a write, `merge_agg`) one statement at
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
  into `~/.pgrx/...`. It then checks that the tarball holds
  `pg_automerge.so` under `--pkglibdir` and the control file under
  `--sharedir/extension`. `tests/check_ci.sh` (part of `mise run lint`)
  checks these refusals and the workflow's concurrency group and package
  step.
- The multi-session shell scripts share `tests/lib.sh`; they start the pgrx-managed
  Postgres if it is not running and stop it again only if they started it.
- `mise run lint`: `tests/check_ci.sh`, rustfmt, clippy with `-D warnings` for the default, the
  `pg_test` and the core-only builds, and rustdoc with `-D warnings`.

## Versioning and upgrades

The extension version is the crate version (`default_version =
'@CARGO_VERSION@'` in `pg_automerge.control`), currently 0.1.0; pgrx
generates the install script `pg_automerge--X.Y.Z.sql` for the build.

Policy:

- After each release, its generated script is committed as
  `sql/snapshots/pg_automerge--X.Y.Z.sql` (`cargo pgrx schema pg18 -o
  ...`) and never edited again. 0.1.0 is the first.
- Any change to the SQL surface afterwards bumps the version, and comes
  with a hand-written `sql/pg_automerge--A--B.sql` from the previous
  version (`cargo pgrx install` and `package` ship every
  `sql/pg_automerge--*--*.sql`). C symbols that an older version's script
  references stay exported (the upgrade test installs the old script
  against the new library).
- `tests/upgrade.sh` (part of `mise run test`) installs every snapshot
  under a scratch version name, stores documents, runs `ALTER EXTENSION
  pg_automerge UPDATE`, and compares the extension's catalog (member
  objects and their comments, function definitions with their labels and
  symbols, aggregates, types, casts, operators) with a fresh `CREATE
  EXTENSION`, and the stored documents' fingerprints before and after. A
  snapshot of the current version is compared as is, so an SQL change
  without a version bump fails.

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
