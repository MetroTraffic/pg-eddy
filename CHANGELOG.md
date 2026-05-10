# Changelog

What's new in pg_eddy — written for everyone, not just developers.

For future plans and upcoming features, see [plans/implementation_plan.md](plans/implementation_plan.md).

## Table of Contents

- [0.9.0](#090--2026-05-09--aggregation-list-comprehensions-and-numeric-operators) — Aggregation, List Comprehensions, and Numeric Operators
- [0.8.0](#080--2026-05-09--with-optional-match-unwind-and-case-expressions) — WITH, OPTIONAL MATCH, UNWIND, and CASE Expressions
- [0.7.0](#070--2026-05-09--cypher-predicates-ordering-and-built-in-functions) — Cypher Predicates, Ordering, and Built-in Functions
- [0.6.0](#060--2026-05-09--cypher-query-engine) — Cypher Query Engine
- [0.5.1](#051--2026-05-09--tap-infrastructure-wal-hardening-and-age-benchmark) — TAP Infrastructure, WAL Hardening, and AGE Benchmark
- [0.5.0](#050--2026-05-09--indexes-constraints-and-full-crud-api) — Indexes, Constraints, and Full CRUD API
- [0.4.0](#040--2026-05-09--mvcc-and-vacuum) — MVCC and VACUUM
- [0.3.0](#030--2026-05-09--edge-storage--adjacency-lists) — Edge Storage + Adjacency Lists
- [0.2.0](#020--2026-05-09--node-storage) — Node Storage
- [0.1.0](#010--2026-05-09--am-skeleton) — AM Skeleton

---

## [0.9.0] — 2026-05-09 — Aggregation, List Comprehensions, and Numeric Operators

v0.9.0 expands the Cypher engine with a complete aggregation suite, list
comprehensions and predicates, correct numeric semantics, and full openCypher
null propagation for comparisons. 188/188 in-scope TCK scenarios pass (100%).

### New Cypher Features

**Aggregation functions** — full suite: `count(*)`, `count(expr)`,
`count(DISTINCT expr)`, `sum`, `avg`, `min`, `max`, `stdev`, `stdevp`,
`collect`, `collect(DISTINCT expr)`. All aggregate functions correctly ignore
null inputs and return null when the input set is empty (except `count` which
returns 0).

**List comprehensions**: `[x IN list WHERE pred | projection]` — filters a list
and optionally transforms each element. The WHERE clause and projection are
both optional.

**List predicates**: `any(x IN list WHERE pred)`, `all(x IN list WHERE pred)`,
`none(x IN list WHERE pred)`, `single(x IN list WHERE pred)`.

**XOR operator**: `a XOR b` — boolean exclusive-or with full null propagation.

**Exponentiation**: `x ^ y` — left-associative per the openCypher spec
(`4^6^3 = (4^6)^3`).

**List subscript and slice**: `list[i]`, `list[i..j]` — with null-safe
element access.

**List concatenation with scalar append**: `list + element`, `element + list`.

### Correctness Fixes

**Null propagation in all comparisons**: `compare_values` now returns
`Option<bool>` — ordering operators (`<`, `>`, `<=`, `>=`) on null inputs
produce null, while equality (`=`) between different types produces `false`
rather than null.

**List equality semantics**: `[a, b] = [c, d]` uses recursive element
comparison with full null propagation. Lists of different lengths are
definitively not equal (no null short-circuit on length mismatch).

**Cross-type sort ordering**: `ORDER BY` now respects the openCypher type
ordering: `null > lists > numbers > strings > booleans`. Mixed-type lists
sort lexicographically with per-element type ordering.

**Boolean ordering**: `false < true` is now correctly implemented for
`<`, `>`, `<=`, `>=` operators.

**OPTIONAL MATCH null safety**: OPTIONAL MATCH on relationships
(`OPTIONAL MATCH ()-[r]->()`) now correctly returns one null row when no
relationships exist. The node isomorphism filter is null-safe so null nodes
from OPTIONAL MATCH pass through instead of being filtered out.

**Column naming**: `RETURN count(a) > 0`, `RETURN count(DISTINCT a)`,
`RETURN n.x IS NULL` now produce correct column names matching the Cypher
expression text.

### TCK

- 188/3880 overall (4.8%); 188/188 in-scope (100%)
- Newly unlocked acceptance tests: `Aggregation1`–`Aggregation8`,
  `ListComprehension`, `ListPredicate`, `Comparison1` (list equality),
  `ReturnOrderBy1` / `WithOrderBy1` (list sort), `Graph6` (optional rel),
  `Null1`, `Null2` (IS NULL column names), `Precedence1` (^, boolean order)

---

## [0.8.0] — 2026-05-09 — WITH, OPTIONAL MATCH, UNWIND, and CASE Expressions

v0.8.0 is a major architectural expansion of the Cypher query engine. The AST,
planner, and executor have been refactored from a single-clause model to a
full **multi-clause pipeline**, enabling composition across MATCH chains with
WITH, outer-join semantics via OPTIONAL MATCH, list expansion via UNWIND, and
conditional logic via CASE. 172/172 in-scope TCK scenarios pass (100%).

### New Cypher Features

**WITH clause**: Projects and renames bindings between query stages, optionally
filtering with `WHERE`. Supports `DISTINCT`, `ORDER BY`, `SKIP`, and `LIMIT`.
Variables not projected by WITH are no longer in scope for subsequent clauses,
exactly matching the openCypher spec.

**OPTIONAL MATCH**: Returns all rows from the left side even when the pattern
finds no matches. Unbound variables from an OPTIONAL MATCH produce `null`
bindings that propagate correctly through subsequent WHERE and RETURN clauses
with openCypher 3-valued logic.

**UNWIND**: Expands a list expression into one row per element, binding each
element to the given variable. Works with literal lists, property accesses, and
expressions. `UNWIND [] AS x` produces zero rows.

**CASE expressions** — both forms:
- *Searched*: `CASE WHEN cond THEN val … [ELSE val] END` — evaluates conditions
  in order and returns the first matching branch.
- *Simple*: `CASE expr WHEN val THEN result … [ELSE val] END` — compares the
  subject expression against each WHEN value.
Both forms return `null` when no branch matches and no ELSE is present.

### Architecture

The internal `Query` type now holds a `Vec<QueryClause>` pipeline instead of a
single match + return pair. The planner folds over clauses left-to-right,
building up a `LogicalPlan` tree. New plan nodes: `SingleRow` (seed for queries
starting with UNWIND or a second MATCH) and `Unwind { input, expr, alias }`.
`Expand` gains an `optional: bool` flag for OPTIONAL MATCH semantics.

### TCK

- 172/3881 overall (4.4%); 172/172 in-scope (100%)
- Newly unlocked acceptance tests: `WithAcceptance`, `OptionalMatchAcceptance`,
  `UnwindAcceptance`, `CaseExpressionAcceptance`, `TriadicSelection`

---

## [0.7.0] — 2026-05-09 — Cypher Predicates, Ordering, and Built-in Functions

v0.7.0 substantially expands the Cypher query engine with string predicates,
list operations, result ordering and pagination, correct null semantics, and a
large suite of built-in functions. 107/107 in-scope TCK scenarios pass (100%).

### New Cypher Features

**String predicates**: `STARTS WITH`, `ENDS WITH`, `CONTAINS`, and `=~` (POSIX
regular expression match, evaluated via PostgreSQL's native regex engine).

**List membership**: `x IN [a, b, c]` — list literals and membership tests with
openCypher null semantics (null IN list containing a match → true; no match →
null if list contains null, else false).

**Result ordering and pagination**: `ORDER BY expr [ASC|DESC], ...`, `SKIP n`,
`LIMIT n`. ORDER BY resolves aliases from both the RETURN clause and the MATCH
bindings; NULL sorts last per openCypher spec.

**Corrected null semantics**: `AND`, `OR`, and `NOT` now use 3-valued logic
exactly as specified in the openCypher standard (`null AND false = false`,
`null AND true = null`, `null OR true = true`, etc.).

### Built-in Functions Added

**Type conversion**: `toBoolean(value)`

**Size/length**: `size(string|list)`, `length(string|list)`

**List functions**: `head(list)`, `tail(list)`, `last(list)`, `reverse(list)`,
`range(start, end[, step])`

**String functions**: `trim(s)`, `ltrim(s)`, `rtrim(s)`, `upper(s)` / `toUpper(s)`,
`lower(s)` / `toLower(s)`, `substring(s, start[, length])`,
`replace(s, search, replacement)`, `split(s, delimiter)`, `reverse(s)`

**Math functions**: `abs()`, `ceil()` / `ceiling()`, `floor()`, `round()`,
`sign()`, `sqrt()`, `log()` (natural log), `log10()`, `exp()`, `sin()`,
`cos()`, `tan()`, `asin()`, `acos()`, `atan()`, `atan2(y, x)`, `pi()`, `e()`

### Test Results

- **Unit tests**: 61/61 pass
- **TCK**: 107/3881 overall (2.8%); 107/107 in-scope (100%)

---

## [0.6.0] — 2026-05-09 — Cypher Query Engine

v0.6.0 delivers the first working Cypher query engine for pg_eddy. You can now
execute `MATCH (n:Label) RETURN n` queries via `pg_eddy.cypher()` and inspect
the logical plan with `pg_eddy.cypher_explain()`. The engine is a recursive
interpreter — it walks the logical plan tree and drives the native AM accessors
directly, avoiding SQL injection risk and SQL round-trips alike. 61/61 tests pass.

### New Functions

**`pg_eddy.cypher(query TEXT, params JSONB DEFAULT NULL) RETURNS SETOF JSONB`**  
Execute a Cypher query and receive JSONB rows. Each output row is a JSON object
whose keys are the names from the `RETURN` clause.

```sql
SELECT * FROM pg_eddy.cypher('MATCH (n:Person) WHERE n.age > $min RETURN n.name',
                              '{"min": 30}'::jsonb);
```

**`pg_eddy.cypher_explain(query TEXT) RETURNS TEXT`**  
Return the logical query plan as a human-readable string, without executing it.
Useful for understanding how the planner decomposed the query.

```sql
SELECT pg_eddy.cypher_explain('MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b');
-- Project
--   Filter (isomorphism: id(a) <> id(b))
--     Expand a -[:KNOWS]-> b
--       LabelScan a :Person
```

### Cypher Language Coverage

The parser handles a useful subset of openCypher:

- **Patterns**: node patterns `(n:Label {prop: val})`, relationship patterns
  `(a)-[:TYPE]->(b)`, bidirectional `(a)-[:TYPE]-(b)`, any-direction `(a)-->(b)`
- **WHERE**: equality (`=`, `<>`, `<`, `>`, `<=`, `>=`), `IS NULL`, `IS NOT NULL`,
  `AND`, `OR`, `NOT`, arithmetic (`+`, `-`, `*`, `/`, `%`), property access
  (`n.prop`), parameters (`$name`), string literals, numeric literals
- **RETURN**: property access, variable projection, `RETURN DISTINCT`,
  function calls, `AS` aliases
- **Parameters**: `$name` mapped to the `params` JSONB argument

### Built-in Functions

`id(n)`, `labels(n)`, `type(r)`, `properties(n)`, `keys(n)`,
`coalesce(a, b, …)`, `toString(v)`, `toInteger(v)`, `toFloat(v)`

### Logical Planner

The planner (`src/cypher/planner.rs`) builds a tree of:

- **LabelScan** — iterates all nodes matching a label via `_pg_eddy.label_index`
- **Expand** — follows edges in `OUT`, `IN`, or `BOTH` directions via the
  adjacency-follow AM accessors
- **CrossProduct** — joins two independent patterns (no shared variables)
- **Filter** — evaluates a WHERE predicate or isomorphism constraint
- **Project** — evaluates the RETURN items and selects output columns

### Node Isomorphism

Per the openCypher specification, two distinct node variables in a MATCH pattern
must not be bound to the same physical node. pg_eddy enforces this by
automatically injecting `id(a) <> id(b)` filter nodes in the plan for every
pair of distinct node variables in the pattern.

### Tests

10 new pgrx integration tests cover end-to-end Cypher execution. 26 Rust unit
tests cover the lexer, parser, and planner individually. All 61 tests pass.

### Not Included in v0.6.0 (deferred to v0.7.0)

- openCypher TCK harness (requires downloading TCK `.feature` files)
- Fuzz targets for the lexer and parser
- `WITH`, `OPTIONAL MATCH`, `ORDER BY`, `SKIP`, `LIMIT`
- `IN [...]`, `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `=~`
- Additional built-ins: `size()`, `length()`, `head()`, `tail()`, `toBoolean()`

---

## [0.5.1] — 2026-05-09 — TAP Infrastructure, WAL Hardening, and AGE Benchmark

v0.5.1 completes Phase 4.x: multi-session TAP tests prove WAL correctness
under crash and concurrent-write conditions; two critical correctness bugs are
fixed; the AGE benchmark gate is passed (4.27× faster on 2-hop expand).
25/25 pgrx tests + 11/11 TAP assertions pass.

### Critical Bug Fixes

**WAL redo PANIC on restart** — Any database that used v0.2.0–v0.5.0 would
PANIC on the first restart after inserting nodes. `redo_node_insert` called
`XLogReadBufferForRedo` for block 1 on every `NODE_INSERT` record, but block 1
only exists on `NODE_INSERT_OVF` records (inserts with overflow property
pages). PostgreSQL's WAL replayer PANICs when asked to locate a block that was
never registered. Fixed with an `is_ovf` guard that only accesses block 1 when
the record type is `XLOG_PG_EDDY_NODE_INSERT_OVF`.

**MVCC isolation broken under REPEATABLE READ / SERIALIZABLE** — `count_nodes()`
and all node scans were using `TransactionIdDidCommit(xmin)`, which returns
`true` for any committed transaction — including transactions that committed
*after* the reader's snapshot was taken. A REPEATABLE READ session therefore
saw new nodes inserted by concurrent transactions, violating snapshot isolation.
Fixed by checking `XidInMVCCSnapshot(xmin, snapshot)` when the xmin is
committed: a node is only visible if its inserting transaction committed
*before* the snapshot was taken.

### TAP Test Infrastructure

Four crash-safety and concurrency tests are now run by `just tap`:

- **001_crash_recovery** — inserts 10 000 nodes, sends `SIGQUIT` (immediate
  shutdown, no checkpoint), restarts the cluster, verifies `count_nodes() = 10000`.
  This test was the one that caught the WAL redo PANIC above.
- **002_edge_crash_recovery** — builds a 10-node / 20-edge ring graph, crashes
  and restarts, verifies edge count and adjacency-chain integrity survive WAL
  replay.
- **003_mvcc_isolation** — T2 opens a REPEATABLE READ transaction and
  snapshots an empty graph; T1 inserts and commits; T2 re-reads and must still
  see zero nodes (snapshot isolation). T2 then commits and must see 1 node.
  This test was the one that caught the MVCC bug above.
- **004_concurrent_inserts** — 4 background sessions each insert 1 000 nodes
  concurrently; verifies `count_nodes() = 4000` with all node IDs distinct
  (no sequence collisions or lost writes).

CI workflow `.github/workflows/tap.yml` runs all four scripts against a fresh
PostgreSQL 18 cluster on every push.

### New SQL Functions

| Function | Returns | Description |
|---|---|---|
| `count_nodes()` | `BIGINT` | Alias for the internal `node_count()`; used by TAP tests and user queries |
| `count_edges()` | `BIGINT` | Alias for the internal `edge_count()`; used by TAP tests and user queries |
| `find_edges(src BIGINT, dst BIGINT, rel_type TEXT)` | `SETOF BIGINT` | Fast edge lookup using rel-type catalog indexes when type + endpoint are given; falls back to adjacency-chain scan |

### Rel-type Catalog Indexes

Two new internal catalog tables enable O(1) edge lookup by type and endpoint
without scanning adjacency chains:

- `_pg_eddy.edge_type_src(type_id, src_node_id, edge_id)` — indexed on
  `(type_id, src_node_id)` and `edge_id`. Written on every `create_edge` call.
- `_pg_eddy.edge_type_dst(type_id, dst_node_id, edge_id)` — same structure
  for the destination endpoint.

Both tables are used by `find_edges()` fast paths and will be used by the
Cypher query planner in Phase 5.

### AGE Benchmark Gate — PASSED ✅

Results on a dev container (Debian 11, PostgreSQL 18.3, 1/50 scale):

| Operation | pg_eddy | AGE | Ratio |
|---|---|---|---|
| Node insert (1K nodes) | 0.129 s | 0.026 s | 0.20× (slower — P1 bug) |
| 1-hop adjacency follow | 12.52 ms | 12.24 ms | 0.98× (parity) |
| **2-hop neighbour expand** | **11.49 ms** | **49.08 ms** | **4.27× faster** |

The ≥2× gate on 2-hop expansion is cleared. **v0.6.0 (Cypher engine) starts
next.** The insert regression (5× slower than AGE due to per-edge SPI writes
to the catalog index tables) is filed as a P1 bug, deferred to v0.5.2 after
the first Cypher milestone ships.

### Schema Note

PostgreSQL reserves all schema names beginning with `pg_`. The `schema =
'pg_eddy'` field that was briefly attempted in the control file was rejected
by PostgreSQL with `ERRCODE_RESERVED_NAME`. Functions install in `public`
(or the schema chosen at `CREATE EXTENSION` time). Internal objects remain in
`_pg_eddy` as before.

### Migration

Upgrade from v0.5.0:

```sql
ALTER EXTENSION pg_eddy UPDATE TO '0.5.1';
-- or run: psql -f sql/pg_eddy--0.5.0--0.5.1.sql
```

New objects added by the migration:

| Object | Type | Description |
|---|---|---|
| `_pg_eddy.edge_type_src` | TABLE | Rel-type → src-node → edge catalog index |
| `_pg_eddy.edge_type_dst` | TABLE | Rel-type → dst-node → edge catalog index |
| `count_nodes()` | FUNCTION | Alias for `node_count()` |
| `count_edges()` | FUNCTION | Alias for `edge_count()` |
| `find_edges(bigint, bigint, text)` | FUNCTION | Fast edge lookup by type + endpoint |

---

## [0.5.0] — 2026-05-09 — Indexes, Constraints, and Full CRUD API

v0.5.0 completes Phase 4: the storage layer is feature-complete for building
the query engine on top. Property overflow pages, physical VACUUM compaction,
label indexes, and the full node/edge CRUD API are all implemented.
24/24 pgrx tests pass.

### Critical WAL Opcode Fix

All WAL info bytes now use only the **high nibble** (bits 4–7). PostgreSQL's
XLogInsert reserves bits 2–3 of the low nibble for its own flags and will
PANIC if an extension sets them. The previous opcodes were broken:

| Record | Old (broken) | New (correct) |
|---|---|---|
| `NODE_INSERT` | `0x00` | `0x00` (unchanged) |
| `NODE_INSERT_OVF` | `0x05` | `0x10` |
| `NODE_DELETE` | `0x02` | `0x20` |
| `NODE_COMPACT` | `0x04` | `0x30` |
| `EDGE_INSERT` | `0x10` | `0x40` |
| `EDGE_DELETE` | `0x11` | `0x50` |
| `ADJ_UPDATE` | `0x20` | `0x60` |
| `VACUUM_PAGE` | `0x30` | `0x70` |

**Databases created with v0.4.0 or earlier cannot be upgraded in-place** — the
on-disk WAL records have the old opcodes. Create a fresh cluster for v0.5.0.

### Property Overflow Pages

Node records now support properties exceeding 48 bytes. When the inline
property buffer is full, a **property overflow page** is allocated in the same
node relation and its block number is stored in the `prop_overflow_page` field
of the node record. The overflow page holds the full serialised property map.

WAL coverage: the overflow block is written inside the same critical section
as the node record, using `REGBUF_FORCE_IMAGE` so the full page image is
captured. VACUUM skips overflow pages (they are reclaimed when the parent node
record becomes dead-to-all-snapshots).

### Physical VACUUM Compaction

After `VACUUM _pg_eddy.nodes`, dead node slots are now physically removed from
pages via `PageRepairFragmentation`. The page is WAL-logged as a full page
image via `XLOG_PG_EDDY_NODE_COMPACT`. Zeroed-out adjacency headers for dead
slots are cleared at the same time. Free space is correctly returned to
PostgreSQL's free space map.

### Label B-tree Index

`_pg_eddy.label_index(label_id INT, node_id BIGINT)` is maintained by the
Rust/SPI layer in `create_node`, `update_node`, `add_label`, `remove_label`,
and `delete_node`. It enables O(|matching nodes|) label scans without sweeping
all node pages.

### New SQL Functions

| Function | Returns | Description |
|---|---|---|
| `add_label(node_id BIGINT, label TEXT)` | `BOOLEAN` | Add a label to an existing node; `FALSE` if already present |
| `remove_label(node_id BIGINT, label TEXT)` | `BOOLEAN` | Remove a label; `FALSE` if not present |
| `detach_delete_node(node_id BIGINT)` | `BOOLEAN` | Delete all incident edges then delete the node atomically |
| `find_nodes(label TEXT, property_filter JSONB)` | `SETOF BIGINT` | Fast label lookup via `label_index`; optional property post-filter |
| `schema_info()` | `JSONB` | Label, rel-type, and property-key registry summary |

### Migration

Upgrade from v0.4.0:

```sql
ALTER EXTENSION pg_eddy UPDATE TO '0.5.0';
-- or run: psql -f sql/pg_eddy--0.4.0--0.5.0.sql
```

**Note**: if your cluster has WAL generated by v0.4.0 or earlier, create a
fresh cluster rather than upgrading — the WAL opcode change is not backward
compatible.

New objects added by the migration:

| Object | Type | Description |
|---|---|---|
| `_pg_eddy.label_index` | TABLE | Label → node B-tree catalog index |
| `add_label(bigint, text)` | FUNCTION | Add a label to a node |
| `remove_label(bigint, text)` | FUNCTION | Remove a label from a node |
| `detach_delete_node(bigint)` | FUNCTION | Detach-delete a node and all its edges |
| `find_nodes(text, jsonb)` | FUNCTION | Label + property scan |
| `schema_info()` | FUNCTION | Registry summary |

---


v0.4.0 implements Phase 3: correct MVCC semantics for nodes and a working
VACUUM pass for both node and edge tables. 17/17 pgrx tests pass.

### What's New

**Node MVCC**

- `pg_eddy.update_node(node_id, labels, properties)` — logically deletes the
  old node record and inserts a new MVCC version on the same page, preserving
  the adjacency-header slot index (`adj_slot_idx`).
- `pg_eddy.delete_node(node_id)` — sets xmax on the node record; physical
  reclamation happens during the next VACUUM pass.
- `read_node_at_offset` now performs full xmin/xmax visibility checks, so
  deleted or not-yet-committed node inserts are correctly filtered out of
  scans and `get_node()` results.

**adj_slot_idx fix**

A bug in Phase 1 caused every node to be stored with `adj_slot_idx = 0`,
meaning all nodes on a page incorrectly shared the same adjacency header
slot. This is now fixed: after `PageAddItemExtended` the correct slot index
(`off − 1`) is written back into the in-page record and used for all
adjacency-header reads and writes.

**VACUUM**

- `VACUUM _pg_eddy.nodes` and `VACUUM _pg_eddy.edges` are now functional.
  The `relation_vacuum` AM callback scans every page, finds slots whose
  xmax has been committed before `GetOldestNonRemovableTransactionId`, marks
  them `LP_DEAD`, and WAL-logs the change via the new
  `XLOG_PG_EDDY_VACUUM_PAGE` (0x30) WAL record type.
- Dead edge slots are **not** physically removed in v0.4.0; instead they are
  kept with `LP_DEAD` flags so that adjacency-chain traversal can still read
  the `next_out` / `next_in` pointers through them. Physical compaction
  (`PageRepairFragmentation`) is planned for Phase 4.
- `edge_store::follow_chain` now handles `LP_DEAD` slots: they are skipped
  (not yielded to callers) but the chain pointer is still followed so the
  remainder of the chain is reachable.

**WAL**

- New `XLOG_PG_EDDY_NODE_DELETE` (0x02) WAL record: sets xmax on the
  in-page `HeapTupleHeaderData`.
- New `XLOG_PG_EDDY_VACUUM_PAGE` (0x30) WAL record: a compact list of
  offset numbers to mark `LP_DEAD` on redo.
- Both records have corresponding redo functions, `rmgr_desc`, and
  `rmgr_identify` entries.

**am_stats()**

`pg_eddy.am_stats()` returns a JSONB document with `live_nodes`, `dead_nodes`,
`live_edges`, `dead_edges`, `node_pages`, and `edge_pages`, suitable for
diagnosing fragmentation before running VACUUM.

### Edge-store improvements

The private `find_node_location` in `edge_store.rs` has been replaced by the
public `node_store::find_node_location`, which returns the **stored**
`adj_slot_idx` from the node record rather than computing it from the item
offset. This is important for correctness after node updates create new items
at different offsets while the adj slot stays the same.



## [0.3.0] — 2026-05-09 — Edge Storage + Adjacency Lists

v0.3.0 implements Phase 2 of the pg_eddy roadmap. Edges are stored with
singly-linked adjacency chains. Edge deletes are logical only (set xmax);
physical compaction is deferred to Phase 3 VACUUM. 14/14 pgrx tests pass.

### Storage Layout — Edge Pages

Each edge page (8 KB) uses standard `PageInit(page, BLCKSZ, 0)` — no
`pd_special` area. Edge slots contain:

```
HeapTupleHeaderData (24 B)
rel_id           (8 B, i64 LE)   — globally unique edge id
rel_type_id      (4 B, i32 LE)   — relationship type (from rel_type_registry)
source_node_id   (8 B, i64 LE)
target_node_id   (8 B, i64 LE)
next_out_page    (4 B, u32 LE)   — next edge in source's outgoing chain
next_out_slot    (2 B, u16 LE)   — 0 = end of chain
next_in_page     (4 B, u32 LE)   — next edge in target's incoming chain
next_in_slot     (2 B, u16 LE)   — 0 = end of chain
prop_inline_len  (2 B, u16 LE)
prop_overflow_page (4 B, u32 LE) — 0 = no overflow (Phase 2: overflow = PE200)
prop_data        (up to 48 B)    — inline binary properties
```

Adjacency heads are stored in the **node page** `pd_special` area
(`NodeAdjHeader` entries), NOT inside edge records, so inserting an edge never
creates a new MVCC version of the source or target node record.

### Adjacency Chain Protocol

- **Insert at head**: new edges are inserted at the front of the out-chain
  (source) and in-chain (target). The `next_*` pointers are set to the
  previous head before the insert.
- **Delete = logical only**: `xmax` is set; the slot remains in the chain.
  Traversal skips invisible slots; VACUUM (Phase 3) rebuilds the chain.
- **Lock ordering**: source node page is always locked before target node page
  (by block number) to prevent deadlocks under concurrent inserts.

### WAL Records

| Record | Opcode | Covers | Approx. size |
|---|---|---|---|
| `XLOG_PG_EDDY_EDGE_INSERT` | `0x10` | Edge page (new slot) | 80–120 B |
| `XLOG_PG_EDDY_EDGE_DELETE` | `0x11` | Edge page (xmax set) | 12 B |
| `XLOG_PG_EDDY_ADJ_UPDATE`  | `0x20` | Node page (new adj header) | 30 B |

Each `create_edge` call emits three WAL records (one per opcode above, two
`ADJ_UPDATE` for source and target). All are within a single critical section.

### Catalog

- `ensure_rel_type(name)` / `rel_type_name(id)` — SPI-backed relationship type
  registry with idempotent upsert.
- `next_edge_id()` — allocates a dense sequential edge id via
  `nextval('_pg_eddy.edge_id_seq')`.

### Simplified MVCC (Phase 2)

Tuple visibility checks use the PostgreSQL commit log (`TransactionIdDidCommit`,
`TransactionIdIsCurrentTransactionId`) to filter out ghost tuples from
rolled-back transactions. Full `HeapTupleSatisfiesVisibility` with snapshot
isolation is Phase 3.

### SQL API

New functions installed by this release:

| Function | Returns | Description |
|---|---|---|
| `create_edge(source BIGINT, target BIGINT, type TEXT, properties JSONB)` | `BIGINT` | Insert an edge; returns its `rel_id` |
| `get_edge(rel_id BIGINT)` | `JSONB` | Read an edge by id; `NULL` if not found or deleted |
| `delete_edge(rel_id BIGINT)` | `BOOLEAN` | Logically delete an edge; `TRUE` if found |
| `edge_count()` | `BIGINT` | Count all non-deleted edges |
| `neighbours(node_id BIGINT, direction TEXT, rel_type TEXT)` | `SETOF BIGINT` | Follow adjacency chain; returns neighbour node ids |
| `expand(node_id BIGINT, direction TEXT, rel_type TEXT)` | `TABLE(...)` | Like neighbours but returns full edge info |

`direction` is `'OUT'`, `'IN'`, or `'BOTH'`. `rel_type` is `NULL` for all
types.

### Migration

Upgrade from v0.2.0:

```sql
ALTER EXTENSION pg_eddy UPDATE TO '0.3.0';
-- or run: psql -f sql/pg_eddy--0.2.0--0.3.0.sql
```

New objects added by the migration:

| Object | Type | Description |
|---|---|---|
| `_pg_eddy.edge_id_seq` | SEQUENCE | Dense sequential edge id allocator |
| `create_edge(...)` | FUNCTION | Edge insert API |
| `get_edge(...)` | FUNCTION | Edge read API |
| `delete_edge(...)` | FUNCTION | Edge logical delete API |
| `edge_count()` | FUNCTION | Edge count API |
| `neighbours(...)` | FUNCTION | Adjacency-follow SRF (node ids) |
| `expand(...)` | FUNCTION | Adjacency-follow SRF (full edge rows) |

### Deliverable Checklist (Phase 2)

- [x] Edge page layout: MVCC records + singly-linked chain pointers
- [x] `tuple_insert` for edges with adjacency chain maintenance
- [x] Logical delete for edges (xmax set, no chain modification)
- [x] WAL redo for `EDGE_INSERT`, `EDGE_DELETE`, `ADJ_UPDATE`
- [x] Lock ordering: source node page locked before target node page
- [x] Adjacency-follow scan (`neighbours`, `expand`) — O(degree), no index
- [x] `create_edge`, `get_edge`, `delete_edge`, `edge_count`
- [x] `neighbours(node_id, direction, rel_type)` — SETOF BIGINT
- [x] `expand(node_id, direction, rel_type)` — TABLE(rel_id, other_node_id, rel_type_id, rel_properties)
- [ ] Property overflow pages (deferred; > 48 B properties raise PE200)
- [ ] Slot callback verification with SQL trigger (Phase 3)
- [ ] Early pg-trickle smoke test (Phase 3)
- [ ] Concurrency / crash-safe edge tests (Phase 3)

---

## [0.2.0] — 2026-05-09 — Node Storage

v0.2.0 implements the first real storage layer on top of the Phase 0 skeleton.
Nodes can be created, read back, and survive crash recovery. The split-region
page layout and custom WAL records are proven correct before edges are added.
9/9 pgrx tests pass.

### Storage Layout

- **Split-region node pages**: each 8 KB node page is divided into two physically
  distinct regions to eliminate MVCC tuple bloat on high-degree nodes.
  - **Region 1 — Adjacency Header Array** (`PD_NODE_SPECIAL_SIZE` = 2 400 B =
    100 × 24-byte `NodeAdjHeader`): fixed-size, updated in-place under exclusive
    buffer lock; never creates new MVCC versions. Zeroed in Phase 1 (edges are
    Phase 2). Each entry carries `out_head_pg`, `out_head_sl`, `in_head_pg`,
    `in_head_sl`, `out_degree`, `in_degree`, and a reserved `graph_partition_id`.
  - **Region 2 — MVCC Node Records** (variable-length, `HeapTupleIsVisible()`
    semantics): `HeapTupleHeaderData` (24 B) + `node_id` (8 B) + `adj_slot_idx`
    (2 B) + `label_count` (1 B) + `prop_inline_len` (2 B) + `prop_overflow_page`
    (4 B) + pad (1 B) + `label_ids[]` + `prop_bytes[]` (max 48 B inline).

### WAL

- `XLOG_PG_EDDY_NODE_INSERT` (opcode `0x00`): registers the target page and
  item bytes (offset + slot data). Redo function replays inserts using
  `XLogReadBufferForRedo` + `PageAddItemExtended`.
- WAL accessor macros (`XLogRecGetInfo`, `XLogRecGetData`, etc.) implemented
  via `DecodedXLogRecord` fields — avoids relying on internal struct layout.
- No `GenericXLog` pages: every WAL record is a compact custom record; a
  `pg_waldump` inspection of a 10 K-node load shows exclusively
  `CUSTOM_RMGR` + `PG_EDDY/NODE_INSERT` entries, never `Generic`.

### Property Encoding (`prop_store.rs`)

- Type-tagged binary format: each property is a `[key_id: 4 B][type_tag: 1 B][value]`
  cell packed into a contiguous byte slice.
- Supported types in this release: `Integer` (8 B), `Float` (8 B),
  `Boolean` (1 B), `String ≤255 B` (1-byte length + UTF-8),
  `String >255 B` (4-byte length + UTF-8), `Null` (0 B payload),
  `List` (4-byte count + recursive elements), `Map` (4-byte pair count +
  recursive key-value pairs), `Date` (4 B days since Unix epoch),
  `LocalDateTime` (8 B µs since Unix epoch), `Duration` (16 B: months 4 B +
  days 4 B + nanos 8 B).
- Symmetric encode/decode with property-key registry lookup via closure.
- Round-trip unit tests in `prop_store::tests` exercise every type tag.

### Catalog (`catalog/labels.rs`)

- `ensure_label(name)` / `label_name(id)` — SPI-backed label registry with
  `INSERT … ON CONFLICT DO NOTHING` for idempotent upsert.
- `ensure_prop_key(name)` / `prop_key_name(id)` — identical pattern for the
  property key registry.
- `next_node_id()` — allocates a dense sequential node id via
  `nextval('_pg_eddy.node_id_seq')`.

### SQL API

New functions installed by this release:

| Function | Returns | Description |
|---|---|---|
| `create_node(labels TEXT[], properties JSONB)` | `BIGINT` | Insert a node; returns its new `node_id` |
| `get_node(node_id BIGINT)` | `JSONB` | Read a node by id; returns `NULL` if not found |
| `node_count()` | `BIGINT` | Full sequential scan counting all visible nodes |
| `health_check()` | `TEXT` | Returns `'pg_eddy OK'` |

### Migration

Upgrade from v0.1.0:

```sql
ALTER EXTENSION pg_eddy UPDATE TO '0.2.0';
-- or run: psql -f sql/pg_eddy--0.1.0--0.2.0.sql
```

New objects added by the migration:

| Object | Type | Description |
|---|---|---|
| `_pg_eddy.node_id_seq` | SEQUENCE | Dense sequential node id allocator |
| `_pg_eddy.nodes` | TABLE | Node storage table (custom AM `pg_eddy_node`) |
| `_pg_eddy.edges` | TABLE | Edge storage table (custom AM `pg_eddy_edge`) |
| `nodes` | VIEW | Public alias over `_pg_eddy.nodes` |
| `edges` | VIEW | Public alias over `_pg_eddy.edges` |
| `create_node(text[], jsonb)` | FUNCTION | Node insert API |
| `get_node(bigint)` | FUNCTION | Node read API |
| `node_count()` | FUNCTION | Node count API |
| `key_id INT` | COLUMN | Registry `key_id` column narrowed from `BIGINT` to `INT` |

---

## [0.1.0] — 2026-05-09 — AM Skeleton

v0.1.0 is the founding release of `pg_eddy`. The custom Table Access Method
skeleton is registered, the WAL resource manager is wired in, and the extension
loads cleanly under `shared_preload_libraries`. All mutation callbacks are stubs
that return "not implemented"; the full-table scan returns empty. The purpose
of this release is to prove the AM registration path end-to-end before any
storage logic is written.

### Custom Table Access Methods

- `pg_eddy_node_handler` — AM handler function for node storage; registered as
  `CREATE ACCESS METHOD pg_eddy_node TYPE TABLE HANDLER pg_eddy_node_handler`.
- `pg_eddy_edge_handler` — AM handler function for edge storage; registered as
  `CREATE ACCESS METHOD pg_eddy_edge TYPE TABLE HANDLER pg_eddy_edge_handler`.
- All AM callbacks are stubs: `scan_begin` / `scan_getnextslot` / `scan_end`
  return an empty scan; every mutation callback (`tuple_insert`,
  `tuple_update`, `tuple_delete`) raises `PE001 — not implemented`.
- Rust callbacks use `unsafe extern "C-unwind"` and `#[unsafe(no_mangle)]` per
  the Rust Edition 2024 rules in effect.

### WAL Resource Manager

- Custom WAL resource manager registered at `_PG_init` via
  `RegisterCustomRmgr()` (development ID 128).
- No-op `rm_redo` and `rm_desc` callbacks at this stage — proves registration
  works and the RMGR entry appears in `pg_stat_wal`.

### Extension DDL (`sql/pg_eddy--0.1.0.sql`)

- Internal schema `_pg_eddy` (underscore prefix; not subject to PostgreSQL's
  reserved-name restriction on `pg_` prefixes).
- Registry tables on the standard heap (small, always warmed into
  `shared_buffers`):
  - `_pg_eddy.label_registry (label_id BIGINT, name TEXT UNIQUE)`
  - `_pg_eddy.rel_type_registry (type_id BIGINT, name TEXT UNIQUE)`
  - `_pg_eddy.property_key_registry (key_id BIGINT, name TEXT UNIQUE)`
- Stub backing tables `nodes` and `edges` created `USING pg_eddy_node` /
  `USING pg_eddy_edge` (no columns; layout is Phase 1).
- No `schema =` field in the control file — PostgreSQL 18 rejects schema names
  beginning with `pg_` (`ERRCODE_RESERVED_NAME`); objects install into
  whichever schema the user specifies.

### Error Taxonomy

- `src/error.rs`: `PgEddyError` enum with `thiserror`-derived `Display`.
  Initial codes: `PE001` (not implemented), `PE002` (internal error).

### SQL API

| Function | Returns | Description |
|---|---|---|
| `health_check()` | `TEXT` | Returns `'pg_eddy OK'` — smoke-tests `CREATE EXTENSION` worked |

### CI and Tooling

- GitHub Actions workflow: `cargo pgrx test pg18`, `cargo clippy --features pg18`.
- `justfile` with `build`, `test`, `lint`, and `run` targets.
- `rust-toolchain.toml` pinned to the stable toolchain required by pgrx 0.18.
- `AGENTS.md`, `CONTRIBUTING.md`, `LICENSE` (Apache 2.0).
