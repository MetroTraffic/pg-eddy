# pg_eddy — Implementation Plan

## 1. Project Overview

**pg_eddy** is a PostgreSQL 18 extension written in Rust using pgrx 0.18 that
implements a high-performance native Labelled Property Graph (LPG) store. Its
core thesis is that a **custom Table Access Method** (AM) storing nodes and
edges in an adjacency-aware layout can deliver traversal performance
fundamentally superior to any heap+index approach (AGE, pg_graph, etc.) —
while remaining a fully transactional, MVCC-safe PostgreSQL extension that can
be operated with standard PostgreSQL tooling (pg_dump, pg_restore, EXPLAIN,
PgBouncer, CNPG).

The project delivers a **graph engine** (v0.1–v1.0): custom AM with
adjacency-follow traversal, native OpenCypher query engine targeting the
openCypher TCK conformance suite, and proven performance advantage over AGE
on multi-hop MATCH patterns.

A future **incremental view maintenance** (IVM) layer — integration with
[pg-trickle](https://github.com/trickle-labs/pg-trickle) for
incrementally-maintained graph views — is planned separately.
See [`plans/ivm_plan.md`](ivm_plan.md) for the IVM design and roadmap.

### Design Principles

- **Graph-first storage**: the custom AM places adjacency information adjacent
  to node data on disk, enabling O(degree) neighbour iteration without index
  lookups for the common case
- **OpenCypher conformance**: the query engine is **grammar-first, not
  TCK-first**. Each milestone is defined as "implement Cypher clause / operator
  X completely per the openCypher EBNF grammar". The TCK is then run to
  *measure* how much of the spec was correctly implemented — it is a
  conformance audit, not a development driver. Fixing specific `not ok` TCK
  lines by adding ad-hoc special cases in the parser or executor is explicitly
  forbidden (see §10.3 for the guard rules)
- **PostgreSQL-native**: leverage MVCC, WAL, parallel query, AIO (PG18), and
  the full extension ecosystem; never duplicate what PostgreSQL already does
  well
- **Safe Rust first**: `unsafe` only at FFI boundaries required by the AM and
  pgrx C-interop; all query and storage logic in safe Rust
- **Incremental adoption**: each release is independently useful; advanced
  features layer progressively on a stable core

### Target Users and Success Criteria

**Target users**:
1. Teams running PostgreSQL who need graph capabilities without operating a
   separate Neo4j instance
2. Applications requiring ACID transactions spanning both relational and graph
   data in the same database
3. Environments where operational simplicity matters: single backup procedure,
   single monitoring stack, single connection pool

**Why pg_eddy over Apache AGE?**
- AGE stores properties as JSONB — typed property comparisons require JSONB
  extraction rather than direct binary comparison
- AGE uses heap tables with B-tree indexes for traversal — multi-hop MATCH is
  O(k × log N) per hop; pg_eddy's adjacency-follow is O(degree) per hop
- AGE has no incremental view maintenance story

**Why pg_eddy over a standalone Neo4j for some users?**
- One system to operate instead of two: one backup, one monitoring stack, one
  connection pool
- Full ACID transactions spanning graph and relational data in the same
  transaction

**Honest benchmark expectations**: every adjacency-follow hop in pg_eddy goes
through PostgreSQL's buffer manager (`ReadBuffer` + `LockBuffer` + slot read +
`ReleaseBuffer`). Neo4j's native store uses memory-mapped files with direct
byte-offset arithmetic. The structural per-hop cost is real:
- For graphs that fit in `shared_buffers`: expect 5–10× slower than Neo4j on
  pure traversal microbenchmarks
- For I/O-bound graphs (larger than shared_buffers): both systems are I/O-
  dominated; the gap narrows to 2–3×
- vs AGE and other heap-based PostgreSQL graph tools: adjacency-follow should
  be 2–5× faster on multi-hop MATCH patterns starting from a known node

**Success at v1.0**:
- 100% openCypher TCK pass rate (goal); deviations documented with upstream
  references. The TCK is the *verification* of completeness, not the
  implementation guide — every feature must work generally, not only for the
  specific patterns that appear in TCK scenarios
- Adjacency-follow measurably faster than AGE on LDBC SNB multi-hop queries;
  published baselines with hardware, dataset size, and raw output
- `pg_dump`/restore round-trip lossless on 10M+ node graphs
- `pg_eddy.health_check()` returns OK on a clean install
- Docker image and CNPG extension image published

---

## 2. Technology Stack

| Layer | Technology |
|---|---|
| Language | Rust (Edition 2024) |
| PG binding | `pgrx` 0.18 (`pg18` feature flag) |
| PostgreSQL | 18.x (primary target) |
| Cypher parser | Custom recursive-descent parser in Rust (`src/cypher/parser.rs`) |
| Cypher IR | In-house algebra IR (`src/cypher/algebra.rs`) |
| Property encoding | Compact binary format: type-tagged values inlined up to 48 bytes, overflow to property store |
| Hashing | `xxhash-rust` (XXH3-64) — node/edge ID generation, internal dedup |
| Serialization | `serde` + `serde_json` — query results, error reports, config |
| Testing | pgrx `#[pg_test]`, `cargo pgrx regress`, `proptest`, `cargo-fuzz`, openCypher TCK harness |
| Benchmarks | `criterion` — micro-benchmarks; custom harness vs. Neo4j Community for end-to-end |

---

## 3. Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│                     Client Layer                        │
│  pgEddy.cypher(query, params)  │  SQL / SPI interface   │
└───────────────────┬─────────────────────────────────────┘
                    │
┌───────────────────▼─────────────────────────────────────┐
│              OpenCypher Query Engine                     │
│  Lexer → Parser → AST → Logical Plan → Physical Plan    │
│  Pattern rewriting · Filter pushdown · Index selection  │
└───────────────────┬─────────────────────────────────────┘
                    │
┌───────────────────▼─────────────────────────────────────┐
│               Native Graph Storage AM                   │
│  Node Store (custom pages) │ Edge Store (CSR pages)     │
│  Property Store (inline + overflow)                     │
│  Label Index (B-tree)  │  Rel-type Index (B-tree)       │
│  Property Index (B-tree per indexed property)           │
└─────────────────────────────────────────────────────────┘
```

---

## 4. LPG Data Model

### 4.1 Nodes

A **node** has:
- A globally unique `node_id BIGINT` (dense sequential integer from a shared
  sequence)
- Zero or more **labels** (string set, stored compactly as integer label IDs)
- A **property map**: a set of typed key-value pairs

### 4.2 Relationships (Edges)

A **relationship** has:
- A globally unique `rel_id BIGINT`
- A **type** (single string; stored as integer type ID)
- A directed `(source_node_id, target_node_id)` pair
- A **property map** identical in structure to node properties

### 4.3 Properties

Properties are typed values. Supported types (aligned with the openCypher type
system):
- `Integer` (64-bit signed)
- `Float` (64-bit IEEE 754)
- `Boolean`
- `String` (UTF-8, unbounded length; inlined up to 48 bytes, overflow to
  property store pages)
- `Date`, `LocalTime`, `LocalDateTime`, `DateTime`, `Duration`
- `Point` (2D/3D; backed by PostGIS `geometry` when available, binary fallback)
- `List` of any uniform type
- `Map` (nested, for complex sub-structures)
- `Null`

Properties are encoded as a compact binary array, not JSONB, to minimise storage
overhead and enable direct numeric comparisons without decode.

### 4.4 Catalogs

Two catalog tables (in the `_pg_eddy` internal schema) store the label and type
string registries:

```sql
CREATE TABLE _pg_eddy.label_registry (
    label_id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       TEXT   NOT NULL UNIQUE
);

CREATE TABLE _pg_eddy.rel_type_registry (
    type_id    BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       TEXT   NOT NULL UNIQUE
);

CREATE TABLE _pg_eddy.property_key_registry (
    key_id     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       TEXT   NOT NULL UNIQUE
);
```

These are small, warmed into shared_buffers at `_PG_init`, and cached in a
per-backend `HashMap<String, i64>` (label/type names → IDs).

---

## 5. Custom Storage Access Method

> **Design constraint**: The custom AM is the foundation of pg_eddy from
> v0.1.0. There is no heap-based prototype phase — if the AM cannot be made to
> work correctly with PostgreSQL's MVCC, WAL, and buffer management, the
> project stops. All phases build on a working custom AM.
> `shared_preload_libraries = 'pg_eddy'` is required from v0.1.0.

### 5.1 Motivation

PostgreSQL's heap AM stores tuples in pages with no awareness of graph
topology. A neighbour-iteration query (`MATCH (n)-[r]->(m) WHERE id(n) = $1`)
on a heap store requires an index scan on the edge table (O(log N + degree)),
followed by degree random reads for the target nodes. On a 100 M-edge graph
with average degree 20, this is 20 index lookups per hop.

Neo4j's native graph storage achieves O(degree) per-hop by storing each node
with a pointer to its first relationship record, and each relationship record
with forward/backward pointers forming a doubly-linked list per node. Following
the list requires sequential reads of fixed-size records — cache-friendly and
predictable.

pg_eddy's AM adapts this insight to PostgreSQL's page-based architecture while
preserving full MVCC semantics.

### 5.2 Page Formats

#### 5.2.1 Node Pages (`PGAT_NODE`)

Each node page (8 KB) is split into **two physically distinct regions** to
solve the MVCC adjacency-pointer update problem: storing adjacency head
pointers inside an MVCC-versioned node record means every edge insert creates
a new node tuple version. On a high-degree node (1M edges), this causes severe
tuple bloat and VACUUM pressure. The split avoids this entirely.

**Region 1 — Adjacency Header Array** (at page start, fixed-size, NOT
MVCC-versioned): one 24-byte entry per node slot on the page. Updated
**in-place under exclusive buffer lock** when edges are inserted or deleted —
WAL-logged as a compact `XLOG_PG_EDDY_ADJ_UPDATE` record (~32 bytes). Never
creates new tuple versions.

```
┌─────────────────────────────────────────────────┐  ← page offset 0
│ adj[0]: out_head_pg(4) out_head_sl(2)           │
│         in_head_pg(4)  in_head_sl(2)            │
│         out_degree(4)  in_degree(4)             │
│         graph_partition_id(4)         24 B/entry│
│ adj[1]: ...                                     │
│ adj[N-1]: ...                                   │
└─────────────────────────────────────────────────┘  ← offset N×24
```

- `graph_partition_id`: reserved for future distributed execution (see §16,
  Citus). Set to `0` in v0.x (single-instance). When populated, identifies
  which partition/community this node belongs to, enabling community-based
  colocation on Citus workers. Written at node creation; updatable via a
  future `pg_eddy.repartition()` API. The 4-byte cost per adjacency header
  is paid from day 1 to avoid a storage format migration later.

**Region 2 — MVCC Node Records** (variable-length, standard MVCC visibility
via `HeapTupleIsVisible`):

```
┌──────────────────────────────────────────────────────────┐
│ xmin (4B) │ xmax (4B) │ infomask (2B) │ infomask2 (2B)   │
│ node_id (8B) │ adj_header_idx (2B)                       │
│ label_count (1B) │ prop_inline_len (2B)                  │
│ prop_overflow_page (4B)                                  │
│ label_ids[label_count × 4B]  (variable, max 32 labels)  │
│ prop_inline_data[≤48B]                                   │
└──────────────────────────────────────────────────────────┘
```

- `adj_header_idx`: index into Region 1 for this node's adjacency heads.
  Written once at node creation; never changes across property/label updates
  (which create new MVCC versions of Region 2 only).
- `prop_inline_data`: up to 48 bytes of encoded properties (see §5.3); if
  properties exceed 48 bytes, `prop_overflow_page` points to a Property
  Overflow Page.

**Why this split matters**: inserting an edge updates only two adjacency
headers (in-place, ~32 bytes WAL each), never the MVCC node records. A
high-degree node (1M edges) does not cause tuple bloat when new edges are
added. Updating node properties creates a new MVCC version of Region 2 only,
leaving adjacency headers untouched.

**Adjacency headers are structural hints, not MVCC-versioned truth**:
- Head pointers may reference edges whose inserting transaction has not yet
  committed or has aborted. Traversal always checks each edge slot's MVCC
  visibility and skips invisible edges.
- Degree counters (`out_degree`, `in_degree`) are **approximate**. They are
  incremented on edge insert and decremented during VACUUM compaction — not on
  logical delete. After transaction aborts or before VACUUM runs, counters may
  overcount. Application code must not rely on exact degree counts; use
  `pg_eddy.neighbours()` with `COUNT(*)` for precise counts.
- VACUUM corrects both head pointers and degree counters by rebuilding the
  adjacency chain, skipping dead-to-all-snapshots edges (see §5.7).
- This is analogous to `pg_class.reltuples` — a useful hint maintained
  cheaply, corrected periodically by maintenance.

Node pages are allocated by `pg_eddy_node_am`.

#### 5.2.2 Edge Pages (`PGAT_EDGE`)

Each edge page packs **edge slot records** using **singly-linked adjacency
chains** (not doubly-linked):

```
┌──────────────────────────────────────────────────────────────┐
│ xmin (4B) │ xmax (4B) │ infomask (2B) │ infomask2 (2B)       │
│ rel_id (8B) │ rel_type_id (4B)                               │
│ source_node_id (8B) │ target_node_id (8B)                    │
│ next_out_page (4B) │ next_out_slot (2B)                      │
│ next_in_page  (4B) │ next_in_slot  (2B)                      │
│ prop_inline_len (2B) │ prop_overflow_page (4B)               │
│ prop_inline_data[48B]                                        │
└──────────────────────────────────────────────────────────────┘
```

- `next_out_page/next_out_slot`: singly-linked chain of outgoing edges of
  `source_node_id`; the head is stored in the source node's adjacency header
  (`out_head_pg`/`out_head_sl`)
- `next_in_page/next_in_slot`: singly-linked chain of incoming edges of
  `target_node_id`

**Why singly-linked, not doubly-linked**: doubly-linked lists require mutating
existing edge records (updating `prev` pointers) on every insert and delete.
This introduces non-MVCC structural changes to edge slots and increases lock
contention. With singly-linked chains:
- **Insert**: new edges are always inserted at the head — only the adjacency
  header is updated (one in-place write under buffer lock). No existing edge
  records are modified.
- **Delete**: logical only — set `xmax`. No structural changes to any edge
  record or adjacency header. The deleted edge remains in the chain but is
  skipped during traversal via MVCC visibility checks.
- **VACUUM**: rebuilds the chain by traversing from head, skipping edges that
  are dead-to-all-snapshots, and updating the adjacency header to point to
  the new head (see §5.7).
- The trade-off: VACUUM must traverse the entire chain to compact it (vs O(1)
  unlink with doubly-linked). This is acceptable because VACUUM already scans
  all pages, and it avoids the MVCC-correctness hazard of physical unlinking
  before commit (see §5.7).

#### 5.2.3 Property Overflow Pages (`PGAT_PROP`)

When a node or edge has properties exceeding 48 bytes, the inline portion is
the first 48 bytes and `prop_overflow_page` points to a chain of property
overflow pages:

```
┌─────────────────────────────────────────────────────────────┐
│ owner_id (8B) │ owner_type (1B: NODE=0, REL=1)             │
│ next_overflow_page (4B)                                     │
│ prop_data[8K − 13B]                                         │
└─────────────────────────────────────────────────────────────┘
```

- Pages are chained; a single property chain is bounded by
  `pg_eddy.max_property_chain_pages` GUC (default: 64, i.e. max ~500 KB per
  entity)

### 5.3 Property Binary Encoding

Properties are stored as a packed array of typed value cells:

```
[key_id: 4B][type_tag: 1B][value: variable]...
```

Type tags and value encodings:

| Tag | Type | Encoding |
|---|---|---|
| `0x01` | `Integer` | 8-byte little-endian signed |
| `0x02` | `Float` | 8-byte IEEE 754 little-endian |
| `0x03` | `Boolean` | 1 byte (`0x00` = false, `0x01` = true) |
| `0x04` | `String ≤ 255B` | 1-byte length prefix + UTF-8 bytes |
| `0x05` | `String > 255B` | 4-byte length prefix + UTF-8 bytes |
| `0x06` | `Date` | 4-byte days since Unix epoch |
| `0x07` | `LocalDateTime` | 8-byte microseconds since Unix epoch |
| `0x08` | `DateTime` | 8-byte UTC microseconds + 2-byte TZ offset |
| `0x09` | `Duration` | 16 bytes (months: 4B, days: 4B, nanos: 8B) |
| `0x0A` | `Point2D` | 4-byte SRID + 8B X + 8B Y |
| `0x0B` | `Point3D` | 4-byte SRID + 8B X + 8B Y + 8B Z |
| `0x0C` | `List` | 4-byte element count + elements (recursive encoding) |
| `0x0D` | `Map` | 4-byte pair count + (key_id 4B + value recursive) pairs |
| `0x0E` | `Null` | 0 bytes of payload |

This encoding avoids JSONB parse overhead for numeric comparisons and type
checks in Cypher `WHERE` clauses.

**Property ordering and index comparator rules**: when properties are used as
B-tree index keys (`idx_node_prop_{key}`, `idx_rel_prop_{key}`), the binary
encoding must produce a byte ordering that matches openCypher's comparison
semantics. Rules:
- **Type-tag ordering**: type tags are ordered to match Cypher's cross-type
  comparison rules: `Map < Node < Relationship < List < Path < String <
  Boolean < Integer = Float < Null` (per openCypher spec §9.12)
- **Integers and Floats**: encoded in a sort-compatible binary format
  (sign-magnitude with flipped sign bit for positive ordering)
- **Strings**: encoded using the database's `LC_COLLATE` locale by default;
  a `pg_eddy.string_collation` GUC (default: `'default'`) can force `'C'`
  locale for byte-order comparison (faster, locale-independent)
- **Float canonicalization**: `NaN` sorts after all non-NaN values;
  `-0.0` compares equal to `+0.0` (per IEEE 754); the encoded form must
  reflect this
- **Null**: sorts last in ascending order, first in descending (matches
  PostgreSQL `NULLS LAST` / `NULLS FIRST` defaults and openCypher ORDER BY)

### 5.4 Table AM Registration

The AM is registered in `_PG_init` via a raw `TableAmRoutine` struct:

```rust
// src/storage/am.rs
static NODE_AM_ROUTINES: TableAmRoutine = TableAmRoutine {
    type_: NodeTag::T_TableAmRoutine,
    slot_callbacks:    pg_eddy_slot_callbacks,
    scan_begin:        pg_eddy_scan_begin,
    scan_end:          pg_eddy_scan_end,
    scan_rescan:       pg_eddy_scan_rescan,
    scan_getnextslot:  pg_eddy_scan_getnextslot,
    // ... MVCC, tuple insert/update/delete, index build callbacks
};
```

The `pg_eddy` extension creates two AM objects at `CREATE EXTENSION` time:

```sql
CREATE ACCESS METHOD pg_eddy_node TYPE TABLE HANDLER pg_eddy_node_handler;
CREATE ACCESS METHOD pg_eddy_edge TYPE TABLE HANDLER pg_eddy_edge_handler;
```

Internal node and edge heap tables are then created `USING pg_eddy_node` and
`USING pg_eddy_edge` respectively.

**pgrx interop**: The AM callbacks are `unsafe extern "C"` functions written in
Rust, exposing `pg_sys` types directly. Safe wrappers in `src/storage/am.rs`
validate all pointers before use. This is the primary `unsafe` boundary in the
codebase.

### 5.5 MVCC and WAL

**MVCC visibility**: pg_eddy uses PostgreSQL's standard visibility rules.
`xmin`/`xmax`/`infomask*` fields in MVCC node/edge records are written and
interpreted identically to heap tuples via `HeapTupleIsVisible()`. Adjacency
headers (§5.2.1) are not MVCC-versioned — they reflect the current structural
state; dead edges in the adjacency list are filtered during traversal by
checking each edge slot's MVCC visibility.

**WAL — custom resource manager, not GenericXLog**: pg_eddy registers a
custom WAL resource manager via `RegisterCustomRmgr()` (available since PG14,
well-supported in PG18). `GenericXLogStart`/`GenericXLogFinish` is explicitly
rejected: it logs entire 8 KB pages. A single edge insert would produce ~24 KB
of WAL (edge page + two adjacency-header page images) — roughly 400× more
than necessary. Custom records are 30–200 bytes per operation.

Custom WAL record types (`src/storage/wal_records.rs`):

| Record type | Payload | Typical size |
|---|---|---|
| `XLOG_PG_EDDY_NODE_INSERT` | page_id, slot_idx, node_id, label_ids[], prop_data[] | 80–200 B |
| `XLOG_PG_EDDY_NODE_UPDATE_PROPS` | page_id, slot_idx, new_prop_data[] | 50–200 B |
| `XLOG_PG_EDDY_NODE_DELETE` | page_id, slot_idx, xmax, xmax_xid | 30 B |
| `XLOG_PG_EDDY_EDGE_INSERT` | page_id, slot_idx, full edge slot | 100 B |
| `XLOG_PG_EDDY_EDGE_DELETE` | page_id, slot_idx, xmax, xmax_xid | 30 B |
| `XLOG_PG_EDDY_ADJ_UPDATE` | node_page, adj_slot_idx, new_out_head (page+slot), new_in_head (page+slot), new_out_degree, new_in_degree, graph_partition_id | 40 B |
| `XLOG_PG_EDDY_LABEL_SET` | node_page, node_slot, new_label_ids[] | 20–80 B |

Each record type has a redo function registered in the rmgr's `rm_redo`
callback. Redo functions are pure page-level operations: pin buffer, apply
delta, mark dirty, unpin.

**RMGR ID reservation**: during development, use `RM_EXPERIMENTAL_ID` (128).
Before publishing any release that users might run in production, reserve a
permanent ID on the [PostgreSQL Custom RMGRs wiki page](https://wiki.postgresql.org/wiki/CustomWALResourceManagers).
The `src/test/modules/test_custom_rmgrs` in the PostgreSQL source tree is the
reference implementation baseline for custom RMGR registration.

**Full-page writes (FPW)**: on the first write to a page after a checkpoint,
PostgreSQL prepends the full page image to the WAL record. pg_eddy uses
`XLogRegisterBuffer()` with `REGBUF_STANDARD` on all record types so FPW is
handled correctly.

**Logical decoding / CDC** (implementation deferred — see
[`plans/ivm_plan.md`](ivm_plan.md) §4):
pg_eddy will register a custom logical decoding output plugin
(`src/storage/wal_decode.rs`) that intercepts pg_eddy WAL records and emits
structured change events. This plugin will serve two purposes:
1. **External CDC consumers** (Debezium, Kafka, custom integrations) — any
   standard logical replication client can consume pg_eddy changes via this
   plugin
2. **pg-trickle WAL CDC path** — pg-trickle's bgworker can consume this
   plugin directly instead of `pgoutput` to enable WAL-based CDC for pg_eddy
   tables (see [`plans/ivm_plan.md`](ivm_plan.md) §4)

The specification is documented here so that the WAL record format (which
*is* implemented from Phase 1 for crash recovery) remains compatible with
future logical decoding. The output plugin itself is not built until the
graph engine and trigger-based CDC are proven.

**Binary event frame format** (specified now, implemented post-v1.0):
```
BEGIN(xid: u32)
  NodeInserted { node_id: u64, label_ids: [u32], properties: bytes }
  NodeUpdated  { node_id: u64, changed_properties: bytes }
  NodeDeleted  { node_id: u64 }
  EdgeInserted { rel_id: u64, rel_type_id: u32, src: u64, tgt: u64, properties: bytes }
  EdgeDeleted  { rel_id: u64 }
COMMIT(xid: u32, commit_lsn: u64)
```

**CDC event filtering**: the output plugin emits **only logical mutation
events** (the five types above). Internal storage operations — `ADJ_UPDATE`
(adjacency header updates), `ADJ_CHAIN_REBUILD` (VACUUM chain compaction),
and `VACUUM_RECLAIM` (slot reclamation) — are **not** emitted. These are
physical storage maintenance, not logical data changes.

For trigger-based CDC and WAL CDC architecture, see
[`plans/ivm_plan.md`](ivm_plan.md).

### 5.6 Indexes

pg_eddy registers standard PostgreSQL B-tree indexes on top of the custom AM:

| Index | Key | Purpose |
|---|---|---|
| `idx_node_label` | `(label_id, node_id)` | MATCH by label |
| `idx_rel_type_out` | `(rel_type_id, source_node_id)` | MATCH outgoing by type |
| `idx_rel_type_in` | `(rel_type_id, target_node_id)` | MATCH incoming by type |
| `idx_node_prop_{key}` | `(encoded_value, node_id)` | WHERE on indexed property |
| `idx_rel_prop_{key}` | `(encoded_value, rel_id)` | WHERE on indexed relationship property |

User-defined property indexes are created via:
```sql
SELECT pg_eddy.create_node_index('Person', 'email');
SELECT pg_eddy.create_rel_index('FOLLOWS', 'since');
```

### 5.7 VACUUM and Adjacency Chain Compaction

VACUUM is not housekeeping for pg_eddy — it is part of the core storage engine.
Because edge deletes are logical only (set xmax, no structural changes), dead
edges accumulate in adjacency chains. VACUUM's `relation_vacuum` callback must
compact these chains to reclaim space and maintain traversal performance.

**When an edge is dead-to-all-snapshots** (xmax is visible to all active
transactions and no snapshot can see it):

1. **Edge page cleanup**: the edge slot is marked as free space, available for
   reuse by future `tuple_insert` calls.

2. **Adjacency chain rebuild**: for each node whose adjacency chain contained
   a dead edge, VACUUM traverses the chain from head, builds a new chain
   skipping dead edges, and updates the adjacency header with the new head
   pointer and corrected degree counter. This is done under exclusive buffer
   lock on the node page + each edge page visited.

   The chain rebuild is O(degree) per node. For hub nodes with millions of
   edges, this can be expensive. Mitigations:
   - `pg_eddy.vacuum_freeze_threshold` GUC: only compact a node's chain when
     the dead-edge count exceeds this threshold (default: 50000)
   - VACUUM processes one node at a time; `CHECK_FOR_INTERRUPTS()` is called
     between nodes
   - VACUUM reports per-node compaction stats via `pg_eddy.am_stats()`

3. **Property overflow page reclamation**: overflow pages belonging to dead
   nodes/edges are returned to the free space map.

**Transaction abort handling**: when a transaction that inserted an edge
aborts, the edge's xmin is marked as aborted. The adjacency header's head
pointer still references this edge (it was inserted at the head). Traversal
skips it (xmin not committed → invisible). VACUUM removes it during chain
rebuild. The degree counter overcount is corrected at the same time.

**Invariant**: between VACUUM runs, adjacency chains may contain dead or
aborted edges. This is correct — traversal filters them via MVCC visibility.
The only cost is slightly longer chain traversals until VACUUM compacts.

**Fragmentation metric**: `pg_eddy.am_stats()` reports
`dead_edge_ratio_per_node` = dead edges / total edges in chain. When this
exceeds 0.3 for any node, pg_eddy emits a `NOTICE` recommending VACUUM.

---

## 6. OpenCypher Query Engine

### 6.1 Parser (`src/cypher/parser.rs`)

A hand-written recursive-descent parser that produces a concrete syntax tree
(CST) and then lowers it to an abstract syntax tree (AST). The grammar follows
the [openCypher Reference Grammar](https://s3.amazonaws.com/artifacts.opencypher.org/openCypher9.pdf)
(the canonical grammar used by the TCK).

Key parser choices:
- No external parser generator; hand-written for predictable error messages and
  easy integration with the Rust type system
- Unicode-aware lexer: handles the full openCypher identifier character set
  (including non-ASCII)
- Error recovery: the parser records errors and attempts to continue to surface
  multiple diagnostics in one pass
- Lexer and parser are `#[cfg(test)]`-fuzzed from v0.4.0 (see §10.4)

### 6.2 AST (`src/cypher/ast.rs`)

```rust
pub enum Expr {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Str(String),
    Null,
    Variable(String),
    PropertyAccess { base: Box<Expr>, key: String },
    FunctionCall { name: String, args: Vec<Expr>, distinct: bool },
    BinaryOp { op: BinaryOp, left: Box<Expr>, right: Box<Expr> },
    UnaryOp { op: UnaryOp, expr: Box<Expr> },
    ListExpr(Vec<Expr>),
    MapExpr(Vec<(String, Expr)>),
    Pattern(NodePattern, Vec<RelChain>),
    Case { operand: Option<Box<Expr>>, whens: Vec<(Expr, Expr)>, else_: Option<Box<Expr>> },
    // ...
}

pub enum Clause {
    Match  { pattern: Vec<Pattern>, optional: bool, where_: Option<Expr> },
    Return { items: Vec<ReturnItem>, distinct: bool, order_by: Vec<OrderItem>, skip: Option<Expr>, limit: Option<Expr> },
    With   { items: Vec<ReturnItem>, distinct: bool, where_: Option<Expr>, order_by: Vec<OrderItem>, skip: Option<Expr>, limit: Option<Expr> },
    Create { pattern: Vec<Pattern> },
    Merge  { pattern: Pattern, on_match: Vec<SetClause>, on_create: Vec<SetClause> },
    Set    (Vec<SetClause>),
    Remove (Vec<RemoveClause>),
    Delete { expressions: Vec<Expr>, detach: bool },
    Unwind { expr: Expr, alias: String },
    Call   { procedure: String, args: Vec<Expr>, yield_: Option<Vec<YieldItem>> },
    Foreach { variable: String, list: Expr, clauses: Vec<Clause> },
    LoadCsv { url: Expr, alias: String, with_headers: bool, field_terminator: Option<Expr> },
}
```

### 6.3 Logical Plan (`src/cypher/logical_plan.rs`)

The AST is lowered to a relational algebra IR:

- `Scan(NodeScan | RelScan | LabelScan | ...)` — leaf nodes
- `Expand(dir: Dir, rel_type: Option<TypeSpec>)` — neighbour expansion
- `Filter(expr)` — WHERE predicates
- `Project(items)` — SELECT / WITH
- `Aggregate(groups, aggregates)` — GROUP BY equivalent
- `Sort(items)` / `Limit(n)` / `Skip(n)`
- `Apply(lhs, rhs)` — correlated sub-pattern (for OPTIONAL MATCH etc.)
- `Union(lhs, rhs, all: bool)`
- `Unwind(expr, alias)`
- `Create(pattern)` / `Merge(pattern, on_match, on_create)` / `Delete(exprs, detach)`
- `Set(clauses)` / `Remove(clauses)`

The planner applies rewrites in a fixed order:
1. Label and type inference: propagate label constraints from WHERE into Scan nodes
2. Predicate pushdown: move filter expressions as close to scan sources as possible
3. Pattern decomposition: split complex patterns into binary joins
4. **Node isomorphism enforcement**: for every pair of distinct node variables
   in a single MATCH pattern, the SQL generator emits `a.node_id <> b.node_id`
   inequality predicates. A pattern with N node variables produces N(N-1)/2
   inequalities. This enforces the openCypher requirement that distinct node
   variables cannot bind to the same node — omitting it produces subtly wrong
   results on patterns like `MATCH (a)-->(b)-->(a)`. Relationship isomorphism
   within a single path (no repeated relationship) is enforced separately in
   variable-length paths via the `rel_ids` exclusion array (see §6.5).
5. Variable-length path planning (see §6.5)

### 6.4 Physical Plan and SQL Generation (`src/cypher/sql_gen.rs`)

The physical planner selects execution strategies and emits SQL (executed via
SPI):

| Logical Operator | Physical Strategy | SQL Produced |
|---|---|---|
| `LabelScan(label, where)` | Index scan on `idx_node_label` | `SELECT ... FROM pg_eddy.nodes WHERE label_id = $1 AND ...` |
| `Expand(OUT, type)` | LATERAL SRF adjacency-follow | `JOIN LATERAL pg_eddy.expand(n.node_id, 'OUT', $type) e ON true` |
| `Expand(IN, type)` | LATERAL SRF adjacency-follow | `JOIN LATERAL pg_eddy.expand(n.node_id, 'IN', $type) e ON true` |
| `Expand(ALL, *)` | LATERAL SRF (both directions) | `JOIN LATERAL pg_eddy.expand(n.node_id, 'BOTH', NULL) e ON true` |
| `Filter(expr)` | SPI parameter binding | `WHERE <encoded_predicate>` |
| `Aggregate` | PostgreSQL `GROUP BY` | Standard aggregate SQL |
| `Sort/Limit/Skip` | PostgreSQL `ORDER BY` / `LIMIT` / `OFFSET` | Direct pushdown |
| `VarLengthExpand` | `WITH RECURSIVE` CTE + CYCLE | See §6.5 |

**`pg_eddy.expand()` — the guaranteed adjacency-follow operator**: rather than
emitting `JOIN pg_eddy.edges ON source = n.id` (which would rely on the
PostgreSQL planner to accidentally use the AM's adjacency-follow scan), the
SQL generator emits a LATERAL set-returning function call:

```sql
-- pg_eddy.expand(): SRF that follows the adjacency chain directly
pg_eddy.expand(
    node_id   BIGINT,
    direction TEXT,      -- 'OUT', 'IN', or 'BOTH'
    rel_type  TEXT       -- NULL for all types
) RETURNS TABLE(
    rel_id          BIGINT,
    other_node_id   BIGINT,
    rel_type_id     INT,
    rel_properties  JSONB
)
```

Internally, `expand()` reads the adjacency header for the given node, follows
the singly-linked edge chain (checking MVCC visibility at each slot), and
yields one row per visible edge. This guarantees O(degree) execution cost per
hop regardless of planner decisions. The function is `ROWS 20` (average degree
hint for the planner's row-count estimates).

For multi-hop MATCH patterns, the SQL generator chains LATERAL joins:
```sql
SELECT a.node_id, b.node_id, c.node_id
FROM   pg_eddy.nodes a
JOIN LATERAL pg_eddy.expand(a.node_id, 'OUT', 'KNOWS') e1 ON true
JOIN   pg_eddy.nodes b ON b.node_id = e1.other_node_id
JOIN LATERAL pg_eddy.expand(b.node_id, 'OUT', 'KNOWS') e2 ON true
JOIN   pg_eddy.nodes c ON c.node_id = e2.other_node_id
WHERE  a.label_id = $person_label
  AND  a.node_id <> b.node_id
  AND  a.node_id <> c.node_id
  AND  b.node_id <> c.node_id;  -- node isomorphism
```

This pattern ensures every hop uses adjacency-follow, filter/aggregate/sort
use PostgreSQL's native operators, and the generated SQL is inspectable via
`pg_eddy.cypher_explain()`.

All user-supplied string values (node/rel properties used in comparisons) are
passed as SPI bind parameters (`$N`), never interpolated into SQL text.
SQL injection via Cypher property values is structurally impossible.

### 6.5 Variable-Length Paths

Cypher patterns like `(a)-[:KNOWS*1..5]->(b)` compile to bounded recursive CTEs:

```sql
WITH RECURSIVE path(start_id, end_id, rel_ids, depth) AS (
    -- anchor
    SELECT source, target, ARRAY[rel_id], 1
    FROM   pg_eddy.edges
    WHERE  source = $1 AND type_id = $knows_type_id
  UNION ALL
    -- recursive
    SELECT p.start_id, e.target, p.rel_ids || e.rel_id, p.depth + 1
    FROM   path p
    JOIN   pg_eddy.edges e ON e.source = p.end_id
    WHERE  e.type_id = $knows_type_id
      AND  p.depth < $max_hops
      AND  NOT (e.rel_id = ANY(p.rel_ids))   -- no repeated relationships
)
CYCLE end_id SET is_cycle USING path_ids
SELECT start_id, end_id, rel_ids FROM path WHERE NOT is_cycle;
```

- PG18's `CYCLE` clause gives hash-based cycle detection (O(1) per step)
- `NOT (e.rel_id = ANY(p.rel_ids))` enforces openCypher's no-repeated-edges
  semantics within a single path
- Unbounded (`*`) paths are capped by `pg_eddy.max_path_depth` GUC (default: 100)

**`shortestPath()` and `allShortestPaths()`** are implemented in Rust
(`src/cypher/path_search.rs`) as BFS over the native AM adjacency lists, not
via SQL. Three correctness and safety requirements that must be met:

- **`CHECK_FOR_INTERRUPTS()`** is called at the top of every BFS iteration
  loop. Without this, a traversal on a large graph cannot be cancelled and will
  hold its backend until completion.
- **Buffer pin discipline**: each adjacency page is pinned with `ReadBuffer()`,
  the required adjacency header and edge slot data is copied into a
  stack-allocated buffer, then `ReleaseBuffer()` is called before processing.
  The BFS never holds more than one buffer pin simultaneously. Holding pins
  across loop iterations would exhaust `max_locks_per_transaction` on
  high-degree nodes.
- **Memory budget**: BFS state (visited set, frontier queue) is allocated from
  a `MemoryContext` bounded by `pg_eddy.traversal_work_mem`. When the frontier
  exceeds this budget, the query raises `PE320` ("traversal memory budget
  exceeded") with the current frontier size. Spill-to-disk is a post-v1.0
  optimization (see §16, VarLengthExpand v2).
- **Relationship uniqueness in `allShortestPaths()`**: the spec requires that
  no relationship appears twice in a single returned path. The visited set
  tracks `(node_id, frozenset(rel_ids_on_path))` rather than node IDs alone.

### 6.6 Null Semantics

OpenCypher null semantics align with SQL in most cases (three-valued logic,
null propagation through arithmetic and comparison) but diverge in list/map
operations and string functions that have no SQL equivalent.

**Translation strategy**:
- Expressions with exact SQL null semantics (`=`, `<>`, `<`, `IS NULL`, `AND`,
  `OR`, `NOT`, arithmetic): translated directly to SQL. SQL and openCypher
  agree on null propagation for these.
- String predicates with null inputs (`STARTS WITH`, `ENDS WITH`, `CONTAINS`,
  `=~`): translated to SQL `LIKE` / `~` with explicit `IS NOT NULL` guards;
  matches openCypher semantics.
- **List and map operations** (`IN [...]`, list indexing, list equality, list
  concatenation): evaluated by the Rust expression evaluator
  (`src/cypher/expressions.rs`), not translated to SQL. Example:
  `[1, null] = [1, null]` returns `null` per spec; SQL has no list equality.
- `COLLECT` aggregate: maps to `array_agg(expr) FILTER (WHERE expr IS NOT NULL)`
  — correctly skips nulls per spec.
- `NULL IN [null, 1]`: openCypher returns `null`; SQL `NULL = ANY(ARRAY[NULL,
  1])` also returns `null`. These align.

**Expression evaluator** (`src/cypher/expressions.rs`): each expression node
in the logical plan carries an `EvalStrategy` flag — `SqlTranslatable`
(emitted into the SQL string) or `RustEvaluated` (called via a PostgreSQL SRF
callback after SQL rows are returned). Mixed plans are supported: SQL generates
the row set, and the Rust evaluator post-filters any `RustEvaluated`
predicates on the returned rows.

**Regression coverage**: `sql/regress/null_semantics.sql` must cover every
scenario in the TCK `NullAcceptance` feature group before v1.0.

### 6.7 SQL Function API

```sql
-- Primary Cypher query interface
pg_eddy.cypher(
    query  TEXT,
    params JSONB DEFAULT '{}'
) RETURNS SETOF JSONB

-- Inspect the generated SQL for a Cypher query (for debugging and EXPLAIN)
pg_eddy.cypher_explain(
    query   TEXT,
    params  JSONB DEFAULT '{}',
    analyze BOOL DEFAULT FALSE
) RETURNS TEXT

-- Node CRUD (used by the Cypher engine and available directly)
pg_eddy.create_node(labels TEXT[], properties JSONB) RETURNS BIGINT
pg_eddy.get_node(node_id BIGINT) RETURNS JSONB
pg_eddy.update_node(node_id BIGINT, properties JSONB) RETURNS VOID
pg_eddy.delete_node(node_id BIGINT, detach BOOL DEFAULT FALSE) RETURNS VOID

-- Edge CRUD
pg_eddy.create_edge(source BIGINT, target BIGINT, type TEXT, properties JSONB) RETURNS BIGINT
pg_eddy.get_edge(rel_id BIGINT) RETURNS JSONB
pg_eddy.update_edge(rel_id BIGINT, properties JSONB) RETURNS VOID
pg_eddy.delete_edge(rel_id BIGINT) RETURNS VOID

-- Graph management
pg_eddy.clear() RETURNS VOID          -- truncate all nodes and edges
pg_eddy.node_count() RETURNS BIGINT
pg_eddy.edge_count() RETURNS BIGINT
pg_eddy.schema_info() RETURNS JSONB   -- label/type/property key registry summary

-- Adjacency-follow expansion (used by SQL generator, also available directly)
pg_eddy.expand(
    node_id   BIGINT,
    direction TEXT,      -- 'OUT', 'IN', or 'BOTH'
    rel_type  TEXT       -- NULL for all types
) RETURNS TABLE(rel_id BIGINT, other_node_id BIGINT, rel_type_id INT, rel_properties JSONB)
```

---

## 7. pg-trickle Integration (IVM) — Separate Plan

> IVM / pg-trickle integration has been extracted to a dedicated plan:
> [`plans/ivm_plan.md`](ivm_plan.md).
>
> This work depends on a stable, feature-complete graph engine (≥v0.12.0
> with Cypher write clauses and ≥80% TCK compliance). The IVM plan covers
> trigger-based CDC, incremental graph views, constraint views, and the
> future WAL CDC output plugin architecture.

---

## 8. Module Breakdown

### 8.1 Extension Bootstrap (`src/lib.rs`)

- pgrx `#[pg_extern]` entry points for all public SQL functions
- `_PG_init()`: shared memory registration (v0.6.0+), AM registration, label
  cache warm-up, background worker startup
- GUC parameters: see §13 for the canonical GUC reference
- Error taxonomy: `src/error.rs` — `thiserror`-based `PgEddyError` enum with
  `PE###` error codes (see §14)
- `shared_preload_libraries = 'pg_eddy'` required from v0.1.0 (custom AM and
  WAL resource manager must be registered at postmaster start via `_PG_init`)

### 8.2 Catalog (`src/catalog/`)

- `src/catalog/labels.rs` — label registry CRUD + in-memory cache
- `src/catalog/types.rs` — rel-type registry CRUD + in-memory cache
- `src/catalog/property_keys.rs` — property key registry CRUD + in-memory cache
- `src/catalog/schema.rs` — schema creation / upgrade SQL helpers

### 8.3 Storage (`src/storage/`)

- `src/storage/am.rs` — AM registration, unsafe C callback functions
- `src/storage/node_store.rs` — node page layout, insert/update/delete
- `src/storage/edge_store.rs` — edge page layout, insert/update/delete,
  singly-linked adjacency chain maintenance
- `src/storage/prop_store.rs` — property binary encoding/decoding, overflow pages
- `src/storage/scan.rs` — custom scan implementations (full scan, label scan,
  adjacency-follow scan)
- `src/storage/wal_decode.rs` — logical decoding output plugin for CDC (post-v1.0)
- `src/storage/mvcc.rs` — MVCC visibility helpers (wrappers over `HeapTupleIsVisible`)

### 8.4 Cypher Engine (`src/cypher/`)

- `src/cypher/lexer.rs` — tokeniser
- `src/cypher/parser.rs` — recursive-descent parser → AST
- `src/cypher/ast.rs` — AST types
- `src/cypher/algebra.rs` — logical plan IR
- `src/cypher/planner.rs` — AST → logical plan; predicate pushdown; label inference
- `src/cypher/sql_gen.rs` — logical plan → SQL text
- `src/cypher/executor.rs` — SPI execution + result decoding
- `src/cypher/functions.rs` — built-in Cypher functions (`id()`, `labels()`,
  `type()`, `keys()`, `size()`, `length()`, `range()`, string functions, math functions, etc.)
- `src/cypher/expand.rs` — `pg_eddy.expand()` LATERAL SRF: adjacency-follow
  operator that guarantees O(degree) traversal per hop
- `src/cypher/expressions.rs` — expression evaluation (for non-SQL-translatable
  expressions computed in Rust)
- `src/cypher/plan_cache.rs` — Cypher→SQL translation cache (keyed on
  structural hash of normalised AST; default size 512 entries)

### 8.5 Ecosystem (`src/ecosystem/`) — Future

> See [`plans/ivm_plan.md`](ivm_plan.md). This module will be implemented
> when IVM work begins.

- `src/ecosystem/trickle.rs` — pg-trickle detection and graph view management (future)

### 8.6 Statistics & Monitoring (`src/stats/`)

- `src/stats/mod.rs` — label/type counts, property distribution, scan stats
- `src/stats/monitoring.rs` — `pg_eddy.stats()` JSONB function

### 8.7 Admin (`src/admin/`)

- `src/admin/maintenance.rs` — `pg_eddy.vacuum()`, `pg_eddy.reindex()`
- `src/admin/constraints.rs` — uniqueness and existence constraint management

---

## 9. Phased Roadmap

**Strategic phasing**: the roadmap focuses on the core graph engine thesis —
prove that adjacency-follow inside PostgreSQL's buffer manager is
fundamentally faster than heap+index approaches. **If this fails, nothing
else matters.**

IVM / pg-trickle integration is planned separately (see
[`plans/ivm_plan.md`](ivm_plan.md)) and depends on a stable, feature-complete
graph engine.

Each milestone produces a compelling product:

- **v0.5**: "The fastest traversal-oriented LPG inside PostgreSQL" (proven
  by AGE benchmarks)
- **v0.8–v1.0**: "A high-performance LPG with OpenCypher and hybrid
  SQL+graph queries"

**TCK targets**: the percentages below are **estimates**, not hard gates.
The goal is to reach **100% openCypher TCK compliance**. Progress will vary
as feature groups land; any shortfall is addressed in the TCK gap closure
phase.

---

### Phase 0 — AM Skeleton (v0.1.0) ✅ Released 2026-05-09

**Goal**: The custom AM is registered and the extension loads. Prove AM
registration works end-to-end before writing any storage logic. If this phase
fails, stop and reconsider the approach.

**Deliverables**:
- [x] Cargo workspace: `pg_eddy/` (extension), `pg_eddy_http/` (placeholder
      HTTP binary for future Bolt/REST API)
- [x] `pg_eddy.control` with `trusted = false`, `superuser = true`; no
      `schema =` field — PostgreSQL 18 rejects schema names beginning with
      `pg_` (reserved for system use; `ERRCODE_RESERVED_NAME`). Extension
      objects install in whatever schema the user specifies at
      `CREATE EXTENSION pg_eddy SCHEMA <name>` time (default: `public`).
      The internal schema `_pg_eddy` is valid (underscore prefix is not
      reserved and is used by convention for extension-internal objects).
- [x] `shared_preload_libraries = 'pg_eddy'` required from this version
- [x] Custom WAL resource manager skeleton registered via `RegisterCustomRmgr()`
      at `_PG_init` (no-op redo; proves the registration path works; appears
      in `pg_stat_wal`)
- [x] `CREATE ACCESS METHOD pg_eddy_node TYPE TABLE HANDLER pg_eddy_node_handler`
      and `pg_eddy_edge` in the extension SQL
- [x] Node and edge backing tables created `USING pg_eddy_node` /
      `USING pg_eddy_edge` at `CREATE EXTENSION` time
- [x] All AM callbacks registered as stubs returning "not implemented" except
      full-table scan (`scan_begin` / `scan_getnextslot` / `scan_end`), which
      returns empty
- [x] Internal schema `_pg_eddy` created; label/type/property key registry
      tables (standard heap)
- [x] CI: GitHub Actions with `cargo pgrx test pg18`, `cargo clippy`,
      `cargo deny`
- [x] `justfile` with `dev`, `test`, `lint`, `package` targets
- [x] `rust-toolchain.toml` pinned to pgrx 0.18-required stable toolchain
- [x] `AGENTS.md`, `CONTRIBUTING.md`, `LICENSE` (Apache 2.0)

**Exit criteria**: `CREATE EXTENSION pg_eddy` succeeds with
`shared_preload_libraries = 'pg_eddy'`; `SELECT * FROM pg_eddy.nodes` returns
empty without panicking; WAL resource manager appears in `pg_stat_wal`.

---

### Phase 1 — Node Storage (v0.2.0) ✅ Released 2026-05-09

**Goal**: Nodes can be created, read back, and survive crash recovery. The
split-region page layout (§5.2.1) and custom WAL records (§5.5) are proven
correct before adding edges.

**Deliverables**:
- [x] Node page layout: Region 1 (fixed-size adjacency header array, in-place
      updated under exclusive buffer lock) + Region 2 (MVCC node records,
      variable-length, see §5.2.1)
- [x] `tuple_insert` for nodes: allocate slot in Region 2, initialise
      adjacency header in Region 1, WAL-log `XLOG_PG_EDDY_NODE_INSERT`
- [x] WAL redo function for `XLOG_PG_EDDY_NODE_INSERT`
- [x] Full sequential scan with MVCC visibility via `HeapTupleIsVisible()`
- [x] Property binary encoding (`src/storage/prop_store.rs`): all scalar types
      (Integer, Float, Boolean, String, Date, LocalDateTime, Duration), List,
      Map, Null — encode/decode round-trip tests via `proptest`
- [ ] Property overflow pages for properties exceeding 48 bytes
      (implementing in Phase 4 — overflow blocks in same node relation,
       `prop_overflow_page` field already reserved in node record layout)
- [x] Label registry tables + backend-local `HashMap<String, i64>` cache
- [x] `pg_eddy.create_node(labels TEXT[], properties JSONB) RETURNS BIGINT`
- [x] `pg_eddy.get_node(node_id BIGINT) RETURNS JSONB`
- [x] `pg_eddy.node_count() RETURNS BIGINT`
- [ ] Crash-safe test: insert 10K nodes, `pg_ctl stop -m immediate`, verify
      all nodes recovered correctly (requires TAP test infrastructure;
      deferred to Phase 4 infrastructure work)

**Exit criteria**: 1M nodes created and read back correctly; crash-recovery
test passes; WAL records are exclusively `XLOG_PG_EDDY_NODE_INSERT` (verify
with `pg_waldump` — no `Generic` record type present).

---

### Phase 2 — Edge Storage + Adjacency Lists (v0.3.0) ✅ Released 2026-05-09

**Goal**: Edges are stored with singly-linked adjacency chains. Edge deletes
are logical only (set xmax); physical compaction is deferred to VACUUM.
`XLOG_PG_EDDY_ADJ_UPDATE` is proven correct: insert an edge, crash, recover,
verify the adjacency chain is intact.

**Deliverables**:
- [x] Edge page layout: MVCC records + singly-linked chain pointers (see §5.2.2)
- [x] `tuple_insert` for edges: write edge slot with `next_out`/`next_in`
      pointing to the current chain heads; update source/target adjacency
      headers in-place (new head = this edge) under exclusive buffer lock;
      WAL-log `XLOG_PG_EDDY_EDGE_INSERT` + two `XLOG_PG_EDDY_ADJ_UPDATE`
- [x] `tuple_delete` for edges: **logical delete only** — set xmax; WAL-log
      `XLOG_PG_EDDY_EDGE_DELETE`. No adjacency header or chain pointer
      changes. The deleted edge remains in the chain but is skipped by
      traversal via MVCC visibility checks. Physical removal happens during
      VACUUM (§5.7).
- [x] WAL redo functions for `XLOG_PG_EDDY_EDGE_INSERT`,
      `XLOG_PG_EDDY_EDGE_DELETE`, `XLOG_PG_EDDY_ADJ_UPDATE`
- [x] Lock ordering: always acquire source node page lock before target node
      page lock when updating two adjacency headers (prevents deadlocks)
- [x] Adjacency-follow scan: given a node ID and direction, follow the
      singly-linked edge chain from the adjacency header, checking MVCC
      visibility at each edge slot, without an index
- [ ] **Slot callback verification**: deferred to Phase 3 (requires working
      slot callbacks with actual column data)
- [ ] **Early pg-trickle smoke test**: deferred (see
      [`plans/ivm_plan.md`](ivm_plan.md))
- [x] `pg_eddy.create_edge(source BIGINT, target BIGINT, type TEXT, properties JSONB) RETURNS BIGINT`
- [x] `pg_eddy.delete_edge(rel_id BIGINT) RETURNS BOOLEAN`
- [x] `pg_eddy.neighbours(node_id BIGINT, direction TEXT, rel_type TEXT) RETURNS SETOF BIGINT`
- [x] `pg_eddy.expand(node_id BIGINT, direction TEXT, rel_type TEXT)` —
      LATERAL SRF that follows the adjacency chain and returns full edge
      info (see §6.4); this is the guaranteed O(degree) expansion primitive
      used by the Cypher SQL generator
- [ ] MVCC delete correctness test: deferred to Phase 3
- [ ] Concurrency test: deferred to Phase 3
- [ ] Crash-safe edge test: deferred to Phase 3

**Exit criteria**: edge CRUD works ✅; adjacency-follow returns the correct
neighbour set ✅; crash recovery and concurrency tests deferred to Phase 3.

---

### Phase 3 — MVCC and VACUUM (v0.4.0) ✅ Released 2026-05-09

**Goal**: Prove MVCC correctness and storage durability on the custom AM.
**This is the storage correctness gate.** Build nothing further until all
MVCC and VACUUM tests pass.

**Deliverables**:
- [x] Node update: logical-delete old record + insert new MVCC version on the
      same page (adj_slot_idx preserved); WAL-log `XLOG_PG_EDDY_NODE_DELETE` +
      `XLOG_PG_EDDY_NODE_INSERT`. Error if new record won't fit on same page
      (cross-page update support deferred to Phase 4).
- [x] Node delete: set xmax on node record; WAL-log `XLOG_PG_EDDY_NODE_DELETE`.
      Adjacency header is not cleared immediately (VACUUM will reclaim it
      after all incident edges are also dead-to-all).
- [x] Fix `adj_slot_idx` storage: after `PageAddItemExtended`, write the
      correct adj_slot_idx (= off - FirstOffsetNumber) into the in-page copy
      so every node permanently knows its Region 1 slot index.
- [x] Full MVCC xmin/xmax visibility in `read_node_at_offset`:
      `HEAP_XMIN_COMMITTED`, `TransactionIdIsCurrentTransactionId`,
      `TransactionIdDidCommit` for xmin; `HEAP_XMAX_INVALID` for xmax.
- [x] Public `node_store::find_node_location` returning `(blkno, off, adj_slot_idx)`;
      `edge_store` uses this instead of its own private copy.
- [x] VACUUM: `relation_vacuum` callback calls `vacuum_relation()` — scans
      pages, identifies dead slots (xmax committed before OldestNonRemovableXid),
      marks them LP_DEAD, WAL-logs via `XLOG_PG_EDDY_VACUUM_PAGE`. Dead edges
      are skipped in chain traversal (LP_DEAD slots read next-pointer and continue).
      Physical compaction (PageRepairFragmentation) deferred to Phase 4.
- [ ] **REPLICA IDENTITY**: tables have no SQL columns so standard
      `REPLICA IDENTITY DEFAULT` does not apply; full implementation deferred
      to Phase 4 when proper slot callbacks with column data are added.
- [x] `pg_eddy.update_node(node_id BIGINT, labels TEXT[], properties JSONB) RETURNS BOOLEAN`
- [x] `pg_eddy.delete_node(node_id BIGINT) RETURNS BOOLEAN`
- [x] `pg_eddy.am_stats() RETURNS JSONB` — live/dead counts for nodes and edges
- [ ] MVCC isolation test: T1 inserts a node; T2's concurrent snapshot does
      not see it until T1 commits (multi-session test deferred to Phase 4)
- [ ] Concurrency test: deferred to Phase 4
- [ ] Crash-safe recovery test: deferred to Phase 4

**Exit criteria**: node CRUD (create/update/delete) works ✅; VACUUM marks
dead slots LP_DEAD and chain traversal skips them ✅; `am_stats()` returns
correct live/dead counts ✅; 17/17 tests pass ✅.

---

### Phase 4 — Indexes, Constraints, and Full CRUD API (v0.5.0) ✅ COMPLETE (ccc7691, tagged v0.5.0)

**Goal**: Complete the storage layer. Everything needed to build the query
engine on top. Also delivers deferred items from Phases 1–3.

**Deferred items completed in v0.5.0**:
- [x] Property overflow pages (deferred Phase 1) — overflow blocks in the
      same node relation; `prop_overflow_page` field stores block number;
      REGBUF_FORCE_IMAGE WAL; vacuum skips overflow blocks
- [x] Physical VACUUM compaction (deferred Phase 3) — `PageRepairFragmentation`
      on node pages after LP_DEAD marking; zero out dead adj headers;
      WAL-logged as XLOG_PG_EDDY_NODE_COMPACT (full page image)
- [ ] REPLICA IDENTITY — still deferred; tables have no SQL columns so
      standard mechanism does not apply; full implementation requires slot
      callbacks with column data (Phase 5+)
- [ ] Crash-safe / concurrency tests — delivered once TAP infrastructure
      below is in place
- [ ] **TAP test infrastructure** — required before any crash-safe or
      multi-session concurrency test can run; see §10.6 for layout:
      - Add `Makefile` at repo root that delegates to `pg_prove` (from
        `postgresql-18-pgtap` or `cpanm TAP::Parser::SourceHandler::pgTAP`);
        `just tap` runs this
      - Create `tests/tap/` directory; each test is a `.pl` Perl script using
        `PostgreSQL::Test::Cluster` (ships with PG 18 dev package)
      - `tests/tap/001_crash_recovery.pl` — starts a cluster, inserts 10K
        nodes, sends `SIGQUIT` (immediate shutdown), restarts, verifies node
        count matches
      - `tests/tap/002_edge_crash_recovery.pl` — same pattern for edges and
        adjacency chains
      - `tests/tap/003_mvcc_isolation.pl` — two psql sessions via
        `$node->background_psql()`; T1 inserts, T2 reads under snapshot;
        verifies T2 does not see T1's uncommitted write
      - `tests/tap/004_concurrent_inserts.pl` — N parallel psql sessions each
        inserting M nodes; verifies total count = N×M with no duplicates
      - CI job `.github/workflows/tap.yml` runs `just tap` against a
        temporary PostgreSQL 18 cluster; fails on any TAP `not ok`
        (**delivered in v0.5.1** — see Phase 4.x)
- [x] Internal label B-tree index: `_pg_eddy.label_index(label_id, node_id)`
      maintained by Rust/SPI in create_node, update_node, delete_node;
      enables O(|matching nodes|) label scans without a full page sweep
- [x] `pg_eddy.add_label(node_id BIGINT, label TEXT) RETURNS BOOLEAN`
- [x] `pg_eddy.remove_label(node_id BIGINT, label TEXT) RETURNS BOOLEAN`
- [x] `pg_eddy.detach_delete_node(node_id BIGINT) RETURNS BOOLEAN` —
      removes all incident edges then deletes the node
- [x] `pg_eddy.find_nodes(label TEXT, property_filter JSONB) RETURNS SETOF BIGINT`
      — uses label_index for fast label lookup; optionally filters by props
- [x] `pg_eddy.schema_info() RETURNS JSONB` — label, rel-type, property-key
      counts and names from the registry tables
- [x] Tests for all v0.5.0 deliverables (24/24 pgrx tests pass)

**Key fixes in v0.5.0**:
- WAL opcode values: all info bytes now use only the high nibble (bits 4-7);
  PostgreSQL reserves bits 2-3 of the low nibble (causes PANIC if set).
  Old broken values: NODE_DELETE=0x02, EDGE_DELETE=0x11, NODE_COMPACT=0x04,
  NODE_INSERT_OVF=0x05. New correct values: each op has unique high nibble.
- WAL protocol: all page modifications (overflow + node) in one critical section.
- Buffer ordering: find_or_extend_page before write_overflow_block.

**Exit criteria v0.5.0**: property overflow, physical VACUUM, label index,
add/remove label, detach-delete, find_nodes, schema_info all work and tested;
24/24 pgrx tests pass. ✅ MET

---

### Phase 4.x — WAL Hardening, Benchmark Gate, and Storage Completeness (v0.5.1–v0.5.x)

**Goal**: Close the remaining gaps before the query engine. The benchmark gate
is a hard stop: if pg_eddy is not measurably faster than AGE on adjacency-
follow, fix the storage engine *here* rather than building Cypher on top of a
slow foundation. Patch releases are numbered v0.5.1, v0.5.2, … until the gate
passes.

---

#### v0.5.1 — TAP Infrastructure + Crash Safety + AGE Benchmark Baseline ✅ COMPLETE (5baa748, tagged v0.5.1)

**Motivation**: The WAL code paths introduced in v0.2.0–v0.5.0 (node insert,
edge insert, adjacency update, overflow pages, compaction) have never been
exercised under crash or concurrent-write conditions. TAP tests prove WAL
correctness without relying on pgrx's single-session framework.

**Deliverables**:
- [x] TAP test infrastructure
  - `cpanm TAP::Parser::SourceHandler::pgTAP` + `IPC::Run` in dev setup;
    `justfile` gains a `tap` recipe (`prove -v tests/tap/*.pl` with
    `PG_REGRESS`, `PERL5LIB`, `PATH` set)
  - CI job `.github/workflows/tap.yml`: installs PG18+dev, builds release
    extension, installs to system PG, runs `prove -v tests/tap/`
- [x] `tests/tap/001_crash_recovery.pl` — inserts 10 K nodes, sends `SIGQUIT`
      (immediate shutdown), restarts, verifies `count_nodes() = 10000`
- [x] `tests/tap/002_edge_crash_recovery.pl` — same pattern for edges and
      adjacency chains; also checks adjacency-follow across crash boundary
- [x] `tests/tap/003_mvcc_isolation.pl` — T1 inserts; T2 in REPEATABLE READ
      does not see T1's committed write; T2 sees it after its own COMMIT
- [x] `tests/tap/004_concurrent_inserts.pl` — N=4 parallel sessions × M=1000
      inserts; verifies `count_nodes() = 4000` with all IDs distinct
- [x] AGE benchmark — `benchmarks/README.md` with raw numbers (2026-05-09,
      dev container, 1/50 scale; see file for full environment table)
- [x] Rel-type catalog indexes: `_pg_eddy.edge_type_src(type_id, src_node_id)`
      and `_pg_eddy.edge_type_dst(type_id, dst_node_id)` with B-tree indexes;
      `find_edges(src, dst, rel_type)` fast-path function
- [x] `count_nodes()` / `count_edges()` SQL aliases (`pg_extern name=`)

**Key bug fixes in v0.5.1**:
- WAL redo PANIC: `redo_node_insert` called `XLogReadBufferForRedo` for block 1
  on every `NODE_INSERT`, but only `NODE_INSERT_OVF` records have block 1.
  Fixed with `is_ovf` guard — without this fix the server PANICs on restart
  after any node insert.
- MVCC isolation: `read_node_at_offset` ignored the snapshot and used
  `TransactionIdDidCommit` (which sees all committed txns). Fixed with
  `XidInMVCCSnapshot(xmin, snapshot)` — required for correct REPEATABLE READ
  and SERIALIZABLE behaviour.
- Schema naming: `schema = 'pg_eddy'` rejected by PostgreSQL (the `pg_` prefix
  is reserved). Removed; functions install in `public`. TAP tests updated to
  call without schema prefix.

**Exit criteria v0.5.1**: all 4 TAP scripts pass (11/11 TAP + 25/25 pgrx) ✅;
AGE benchmark published ✅; benchmark gate decision recorded below ✅.

**AGE benchmark gate — DECISION: PROCEED to v0.6.0**

Results (2026-05-09, `benchmarks/README.md`):

| Operation | pg_eddy | AGE | Ratio |
|---|---|---|---|
| Node insert (1K nodes) | 0.129 s | 0.026 s | **0.20×** (slower) |
| 1-hop adjacency follow | 12.52 ms | 12.24 ms | **0.98×** (parity) |
| 2-hop neighbour expand | 11.49 ms | 49.08 ms | **4.27×** (faster) |

- **2-hop expand at 4.27×** clears the ≥2× gate → proceed to v0.6.0.
- **1-hop at parity** (0.98×): no action needed.
- **Insert 5× slower**: bottleneck is per-edge SPI writes to `edge_type_src`/
  `edge_type_dst`. Filed as P1 for v0.5.2. **Does not block v0.6.0** because
  the gate criterion is traversal speed, not insert throughput.

---

#### v0.5.2 — Storage Performance (P1 insert bug; deferred to post-v0.6.0)

**Trigger**: v0.5.1 benchmark shows insert throughput **5× slower** than AGE
(0.20× ratio). The traversal gate passed, so this **does not block v0.6.0**.
However it is a P1 bug that must be resolved before v1.0 — users will notice
slow write throughput when building graphs.

**Deferred to after v0.6.0** because:
- The benchmark gate criterion is traversal speed, not insert throughput.
- Fixing insert throughput first would delay the Cypher engine, the
  primary user-facing deliverable.
- The fix (batch catalog writes, deferred index maintenance, or optional fast
  insert path bypassing per-row SPI) is independent of the query engine.

**Root cause hypothesis**: each `create_edge` call does two individual SPI
`INSERT` statements into `edge_type_src` / `edge_type_dst`. At 5 000 edges
this is 10 000 SPI calls vs AGE's single `UNWIND CREATE` Cypher statement.

**Investigation areas** (when resumed):
- [ ] Batch catalog writes: accumulate inserts in a local buffer, flush once
      per statement via SPI with `UNNEST` + `INSERT … SELECT`
- [ ] Deferred index maintenance: write index rows lazily at query time or
      on commit via `RegisterXactCallback`
- [ ] Optional fast insert: skip catalog index writes for bulk loads; expose
      as `create_edge_fast()` with a warning about `find_edges()` accuracy
- [ ] Profile with `perf` / `flamegraph` to confirm SPI overhead is the
      dominant cost and not buffer-manager contention

**Exit criteria v0.5.2**: insert throughput within 2× of AGE at 1K+ edges;
`benchmarks/README.md` updated with new numbers.

---

#### v0.5.3+ — Additional Storage Completeness (deferred to after v0.6.0)

Items that do not block the Cypher engine and are best designed alongside the
query planner:
- [ ] `pg_eddy.create_node_index(label, property_key)` — per-property B-tree
      index (requires AM index callbacks; design alongside the query planner
      so predicate pushdown can use it from day one)
- [ ] `pg_eddy.create_unique_constraint(label, property_key)` and
      `create_existence_constraint(label, property_key)`
- [ ] `pg_eddy.export_cypher_script()` and bulk CSV import (`load_csv_nodes`,
      `load_csv_edges` with `fast := TRUE` option)
- [ ] `pg_dump` / `pg_restore` round-trip test on 1M-node graph
- [ ] Performance CI gate (automated, per-PR): label-scan `<5ms` on 1M nodes;
      1-hop expand `<1ms` on 10M edges
- [ ] REPLICA IDENTITY support (requires slot callbacks with column data)

**Recommendation**: fold v0.5.3 items into v0.6.x milestones as storage
capabilities the query planner needs (e.g. property indexes naturally belong
in v0.6.0 alongside `WHERE` clause support). Do not create a separate v0.5.3
release — start v0.6.0 immediately.

---

### Phase 5 — Cypher Parser and Basic Query Engine (v0.6.0–v0.7.0)

**Goal**: `pg_eddy.cypher()` executes MATCH/RETURN queries using the native
AM. Node isomorphism and null semantics are correct from the first release.

**v0.6.0 deliverables** ✅ COMPLETE (commit 8dda1c5, tag v0.6.0, 2026-05-09):
- [x] Cypher lexer: all openCypher token types, Unicode identifiers, numeric
      literals, string escapes
- [x] Cypher parser: single-clause `MATCH`/`RETURN`; node and relationship
      patterns; `WHERE` with comparisons, `IS NULL`, `AND`/`OR`/`NOT`
- [x] AST types (`src/cypher/ast.rs`)
- [x] Logical planner: `LabelScan` + `Expand(OUT/IN/BOTH)` + `Filter` +
      `Project` + `CrossProduct`
- [x] **Node isomorphism**: planner emits `id(a) <> id(b)` filter for every
      distinct node variable pair (implemented in planner+executor, not SQL
      generator — interpreter approach is cleaner for v0.6.0)
- [x] `pg_eddy.cypher(query TEXT, params JSONB) RETURNS SETOF JSONB`
- [x] `pg_eddy.cypher_explain(query TEXT) RETURNS TEXT`
- [x] Built-in functions: `id()`, `labels()`, `type()`, `properties()`,
      `keys()`, `coalesce()`, `toString()`, `toInteger()`, `toFloat()`
- [x] 10 new pgrx integration tests (61/61 pass); 26 Rust unit tests
- [ ] **Deferred to v0.7.0**: TCK harness, fuzz targets, MatchAcceptance run
- [ ] **Design note**: interpreter executor replaces SQL generator — avoids
      SQL injection risk entirely (no string interpolation into SQL)

**v0.7.0 deliverables** (complete):
- [x] openCypher TCK harness (`tests/tck/`): skip-first pass-rate tracker;
      107/107 in-scope scenarios pass (100%); runs in CI on every PR (35d9f7c)
- [ ] Fuzz targets for lexer and parser (`fuzz/` crate)
- [x] `IN [...]` list membership predicate
- [x] `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `=~` (regex) string predicates
- [x] `ORDER BY`, `SKIP`, `LIMIT` (applied in executor after projection)
- [x] `RETURN DISTINCT` already partially wired; complete with window dedup
- [x] Relationship variable access in RETURN (`RETURN type(r)`, `r.prop`)
- [x] Null semantics evaluator: openCypher null propagation through
      arithmetic, comparisons, and list indexing
- [x] Built-in functions: `size()`, `length()`, `head()`, `tail()`, `last()`,
      `toBoolean()`, plus full math and string function suites
- [x] TCK: 107/3881 overall (2.8%); 107/107 in-scope (100%);
      `WITH`/`OPTIONAL MATCH` deferred to v0.8.0

**Exit criteria (combined Phase 5)**:
- `pg_eddy.cypher()` executes MATCH/WHERE/RETURN on empty and schema-only
  graphs; property access, label tests, string predicates, null comparisons
- Node isomorphism enforced; null semantics correct per openCypher spec
- TCK pass rate ~15% estimated (`WITH`/`OPTIONAL MATCH` deferred to Phase 6)
- No SQL injection possible (interpreter evaluates params directly as Values)
- Parser fuzz runs without panics (cargo fuzz)

---

### Phase 6 — Full Read Language (v0.8.0–v0.11.0)

**Goal**: Complete the read language in four milestones ordered by feature
complexity and TCK payoff. `WITH`/`OPTIONAL MATCH` move here from Phase 5
because they share architectural complexity with `UNWIND` and `CASE`.
Aggregation and variable-length paths follow naturally. The AGE comparison
benchmark is deferred to Phase 7 so data can be loaded via Cypher `CREATE`,
producing a realistic end-to-end comparison rather than a SQL-API
microbenchmark (the v0.5.1 benchmark already proved raw traversal speed at
the storage layer).

**v0.8.0 — Composition clauses**:
- [x] `WITH` clause: mid-query projection and filtering between MATCH chains
- [x] `OPTIONAL MATCH` (rows with no match produce NULL bindings)
- [x] `UNWIND expr AS var`
- [x] `CASE` expressions (simple and searched)
- [x] TCK: 172/3881 overall (4.4%); 172/172 in-scope (100%)

**v0.9.0 — Aggregation and functions**:
- [x] Aggregation: `COUNT(*)`, `COUNT(DISTINCT)`, `SUM`, `AVG`, `MIN`, `MAX`,
      `COLLECT`, `COLLECT(DISTINCT)`, `stDev()`, `stDevP()`
- [x] List comprehensions: `[x IN list WHERE ... | expr]`
- [x] List predicates: `any()`, `all()`, `none()`, `single()`
- [x] XOR operator; exponentiation (`^`) left-associative; subscript; slice
- [x] Null propagation: `compare_values` returns `Option<bool>`; boolean ordering
- [x] List sort ordering: cross-type (null > list > number > string > bool)
- [x] OPTIONAL MATCH null-safe isomorphism filter
- [x] Column naming for `count(DISTINCT)`, `IS NULL`, `IS NOT NULL`, Compare/Arith
- [x] String functions: `toLower()`, `toUpper()`, `trim()`, `ltrim()`,
      `rtrim()`, `substring()`, `replace()`, `split()` (done in v0.7.0);
      remaining: `left()`, `right()`
- [x] Math functions: `abs()`, `ceil()`, `floor()`, `round()`, `sqrt()`,
      `sign()`, `log()`, `log10()`, `exp()`, `sin()`, `cos()`, `tan()`,
      `asin()`, `acos()`, `atan()`, `atan2()` (done in v0.7.0)
- [ ] `percentileCont()`, `percentileDisc()`, `rand()`, `randomUUID()`
- [x] TCK: 188/3880 overall (4.8%); 188/188 in-scope (100%)

**v0.10.0 — Variable-length paths**:
- [x] Variable-length path patterns: full `[*m..n]` grammar (all bound combinations,
      all directions, rel-type filters) with BFS executor and no-repeated-edges guarantee
- [x] `shortestPath()` and `allShortestPaths()` — parsed, routed to BFS (full path
      packaging in next release)
- [x] Path expressions: `nodes(path)`, `relationships(path)`, `length(path)`
- [x] Named paths: `p = (a)-[r]->(b)` → `Value::Path { nodes, rels }`
- [x] Pattern comprehensions: `[(n)-[:KNOWS]->(m) | m.name]`
- [x] `VarLengthExpand` and `NamedPath` plan nodes
- [x] TCK harness Background parsing fix (3,692 scenarios now correctly classified)
- Note: Match5/Match6 TCK scenarios require CREATE (skipped); 188/188 in-scope pass

**v0.11.0 — Subqueries**:
- [x] `EXISTS { ... }` pattern predicate, scalar subqueries
- [x] `CALL { ... }` subqueries (correlated and uncorrelated)
- [x] `CALL procedure(args) YIELD ...`
- [ ] Target: pass `CallSubqueryAcceptance`, `ExistsAcceptance`; TCK ~65% (requires CREATE, deferred to v0.12.0)
- [x] v0.12.0 unlocked all CREATE-dependent TCK scenarios; pass rate improved

**Exit criteria**: TCK pass rate ~65% estimated; `shortestPath()` is cancellable and
memory-bounded; aggregation matches Neo4j for all TCK scenarios.

---

### Phase 7 — Write Language and Benchmark (v0.12.0–v0.14.0)

**Goal**: Full openCypher write language, then the AGE comparison benchmark
(now meaningful because data can be loaded via Cypher `CREATE`), then schema
DDL. The benchmark is placed here — not at v0.7.0 — because a realistic
comparison requires Cypher `CREATE` for data loading; the v0.5.1 storage-
layer micro-benchmark already proved raw adjacency-follow speed.

**v0.12.0 — Write clauses**:
- [x] `CREATE (n:Label {prop: value})`, `CREATE (a)-[:TYPE]->(b)`
- [x] `MERGE ... ON CREATE SET ... ON MATCH SET ...` with uniqueness constraint
      enforcement
- [x] `SET n.prop = value`, `SET n += {map}`, `SET n = {map}`
- [x] `SET n:Label`, `REMOVE n:Label`, `REMOVE n.prop`
- [x] `DELETE n`, `DETACH DELETE n`
- [x] Target: `CreateAcceptance`, `MergeAcceptance`, `SetAcceptance`,
      `DeleteAcceptance`; TCK pass rate improved significantly

**v0.12.x — Insert performance + AGE comparison benchmark**:

> This milestone sits immediately after `CREATE` because a realistic
> benchmark loads data via Cypher `CREATE`, not the SQL API. The v0.5.1
> benchmark proved adjacency-follow speed at the storage layer; this one
> proves end-to-end Cypher performance on a standard graph workload.

- [x] Insert performance fix (deferred from v0.5.2): batch catalog writes to
      `edge_type_src`/`edge_type_dst`; implemented as `CatalogWriteBuffer` in
      `executor.rs`; removes per-edge SPI round-trip
- [x] Fixed `exec_cross_product` variable scoping bug: UNWIND variables now
      visible in downstream MATCH inline property filters
- [x] Load LDBC SNB 1K-node / 5K-edge dataset via Cypher `UNWIND+CREATE`;
      pg_eddy SQL `create_edge()` API used for edge loading (no property index yet)
- [x] Run LDBC IS-1 and IS-3 against AGE on identical hardware; results in
      `benchmarks/README.md`:
      - IS-3 1-hop expand: pg_eddy 92.67 ms vs AGE 169.41 ms → **1.83× faster**
      - IS-1 node lookup: pg_eddy 90.84 ms vs AGE 12.37 ms (slower; no prop index)
- [x] CI performance gate: IS-3 ratio > 1.0 (pg_eddy slower) fails benchmark script
      (exits 1); current result WARN (within 2×)

**v0.13.0 — Storage Stabilisation + Parser Hardening** *(replaces Schema DDL as next milestone)*:

> **Why this milestone was inserted**: TCK analysis after v0.12.1 found that
> 53% of all 1487 failures (790 scenarios) are caused by a `PageAddItemExtended
> failed on block 1` storage error and cascading `could not read blocks`
> errors. An additional 15% fail due to accumulated graph state between TCK
> scenarios (the TCK harness calls BEGIN/ROLLBACK in separate psql connections,
> making them no-ops). These two bugs together block more TCK progress than
> any missing feature. Schema DDL is moved to v0.15.0.

**Storage bug** (P0 — causes 53% of all TCK failures):
- [x] Fix MAXALIGN in `find_or_extend_page` (node_store.rs + edge_store.rs):
      the free-space check uses `item_size + sizeof(ItemIdData)` but
      `PageAddItemExtended` allocates `MAXALIGN(item_size) + sizeof(ItemIdData)`.
      On 64-bit PostgreSQL, MAXALIGN = 8 bytes. When a page has exactly 46–51
      bytes free (a typical remainder for small nodes), the check passes but the
      actual insert fails. Fix: `MAXALIGN(item_size) + sizeof(ItemIdData)`.
- [x] Fix TCK harness graph reset: implemented `clear()` `#[pg_extern]` that
      calls `RelationTruncate` on nodes/edges AM tables, SPI `TRUNCATE` on
      catalog index tables, and restarts ID sequences. Fixed `run_tck.pl` to
      call `SELECT clear()` (public schema, no `eval {}` silencing) at the
      start of each scenario.

**Parser gaps** (causes 149 parse-error TCK failures):
- [x] Map literals in expression context: `{key: value}` is currently only
      parsed at the pattern level. Recognise `{` as start of a MapExpr when
      inside `WHERE`, `RETURN`, `WITH`, `SET`, `CREATE`, and general expression
      positions. Covers `RETURN {name: n.name}`, `WHERE n = {x: 1}`, etc.
- [x] Hex and octal integer literals: `0x1A2B`, `0o777` (and uppercase `0X`,
      `0O`). The lexer now consumes hex/octal digit sequences and
      produces an `IntegerLit` token.
- [ ] Pattern expressions in RETURN / WHERE: `(a)-[:R]->(b)` used as an
      expression value (not a MATCH pattern). Currently the parser emits
      `LArrow` unexpectedly when seeing `<-` inside an expression context.
      Parse as a `PatternExpr` and evaluate as a boolean path predicate.
- [x] Large integer literals: `9223372036854775808` overflows `i64::MAX`;
      now falls back to `FloatLit` rather than panicking.

**Validation gaps** (causes 225 TCK failures — expected errors not raised):
- [x] Reject re-binding of already-bound variables in `CREATE`:
      `CREATE (n) CREATE (n)` now raises `SyntaxError` (variable `n` already
      bound).
- [x] Reject direction-less relationship in CREATE: `CREATE (a)-[r:R]-(b)` (no
      arrow) now raises `SyntaxError`.
- [ ] Reject relationship in node position / node in relationship position
      type mismatches — raise `TypeError` per openCypher spec.

**Other high-value fixes** (from 115 wrong-result / data-isolation failures):
- [x] Fix `MATCH` on empty graph: resolved by `clear()` reset between scenarios.
- [x] Fix `OPTIONAL MATCH` on empty graph: same — resolved by `clear()`.
- [x] Fix control-query wrong row counts: resolved by `clear()` reset.

**Target**: TCK ≥ 65% after this milestone (up from 32.3% in v0.12.1).
**Actual result**: **1526/3880 (39.3%)** — pattern-expression parsing and
type-mismatch errors deferred; two of six validation gaps remain open.

**Exit criteria**: `PageAddItemExtended` error no longer appears in TCK output;
TCK harness correctly resets graph between scenarios; map literals parse in all
expression positions; hex/octal literals parse; TCK pass rate ≥ 65%.
**Status**: Criteria met except TCK% target (39.3% vs 65%); remaining gaps
(pattern-expr, type-mismatch errors) deferred to a future patch.

---

**v0.14.0 — Property Indexes + Temporal Types**:

> Moves temporal functions earlier (from v0.16.0) because they account for
> 14% of TCK failures (201 scenarios), and property indexes fix the IS-1
> benchmark regression (7× slower than AGE due to full-table-scan node lookup).

- [ ] `pg_eddy.create_node_index(label TEXT, property_key TEXT)` — per-property
      B-tree index; `DROP INDEX ON :Label(prop)`; integrated with query planner
      so `WHERE n.prop = $val` uses the index instead of full scan
- [ ] `pg_eddy.create_unique_constraint(label TEXT, property_key TEXT)` —
      uniqueness enforcement at write time
- [x] Temporal constructors: `datetime()`, `date()`, `time()`, `localtime()`,
      `localdatetime()`, `duration()` — parse ISO 8601 strings / component maps
- [x] Temporal arithmetic: `duration.inSeconds()`, `duration.inDays()`,
      `duration.inMonths()`, `duration.between()` — duration extraction methods
- [x] `FOREACH (x IN list | clause)` — simple iteration with write clauses
- [ ] Target: `TemporalAcceptance`, `ForeachAcceptance`; TCK ≥ 80%

**Actual result**: 1628/3880 (42.0%); +102 scenarios vs v0.13.0 (39.3%).
Temporal constructors and FOREACH are implemented; property index work deferred
to a later release.

**Exit criteria**: IS-1 node lookup within 2× of AGE (property index used);
temporal constructors and arithmetic pass all `Temporal2`/`Temporal4`/`Temporal10`
TCK groups; TCK pass rate ≥ 80%.

---

**v0.15.0 — Storage Correctness + Error Validation** *(replaces Schema DDL)*:

> **Why this milestone was reshuffled**: TCK failure analysis after v0.14.0
> shows that **70% of all 1047 failures** (732 scenarios) are storage errors:
> 575 `could not read blocks` + 111 `PageAddItemExtended failed` from setup
> queries, plus 46 from query execution. An additional 227 failures (22%) are
> missing error validation (queries succeed when they should raise
> `SyntaxError`/`TypeError`/`ArgumentError`). Together these two categories
> account for **92% of all failures**. Fixing them unblocks the true TCK pass
> rate. Property indexes and Schema DDL move to v0.16.0.

**Storage bugs** (P0 — causes 70% of all TCK failures):
- [ ] Fix `clear()` to properly reset AM relation storage — the
      `RelationTruncate(rel, 0)` call truncates the file but subsequent
      CREATE scenarios hit `could not read blocks 1..1` and
      `PageAddItemExtended failed on block 1` errors. Root cause: either the
      buffer manager retains stale references, or the adjacency header region
      on block 0 is not properly initialized after truncation. Investigate
      and fix.
- [ ] Fix missing MAXALIGN in `update_node` free-space check: the check uses
      `new_item.len() + sizeof(ItemIdData)` but `PageAddItemExtended` requires
      `MAXALIGN(new_item.len()) + sizeof(ItemIdData)`. Use
      `(new_item.len() + 7) & !7` (matches `find_or_extend_page`).
- [ ] Cross-page node update: when an updated node record doesn't fit on
      its current page, delete-and-reinsert on a new page. Currently this
      raises PE201 ("PageAddItemExtended failed for node update"). The fix
      is bounded — PROP_INLINE_MAX=48 and MAX_LABELS=32 cap record size, so
      this only fires on very full pages where old+new records must coexist.

**Error validation** (22% of failures — expected errors not raised):
- [ ] Boolean operators on non-booleans: `1 AND true`, `'x' OR false`,
      `NOT 1` must raise `TypeError` (76 scenarios: Boolean1/2/3/4)
- [ ] Undefined variables in ORDER BY: `WITH n ORDER BY m` must raise
      `SyntaxError` (40 scenarios: WithOrderBy1/2/3)
- [ ] Invalid argument types for `range()`: raise `ArgumentError` (19 scenarios)
- [ ] Property access on non-graph-elements: `1.prop` must raise `TypeError`
      (14 scenarios: Graph6)
- [ ] Node variable bound to a value: `WITH 1 AS n MATCH (n)` must raise
      `SyntaxError` (6 scenarios: Match1)
- [ ] Aggregation in ORDER BY after non-aggregating WITH: raise `SyntaxError`
      (25 scenarios: WithOrderBy2)

**Target**: TCK ≥ 75% after this milestone (up from 42.0% in v0.14.0).

**Exit criteria**: `could not read blocks` and `PageAddItemExtended` errors
no longer appear in TCK output; boolean type-checking raises TypeError;
undefined ORDER BY variables raise SyntaxError; TCK pass rate ≥ 75%.

---

**v0.16.0 — Property Indexes + Schema DDL** *(combined)*:
- [ ] `pg_eddy.create_node_index(label TEXT, property_key TEXT)` — per-property
      B-tree index; integrated with query planner so `WHERE n.prop = $val`
      uses the index instead of full scan
- [ ] `pg_eddy.create_unique_constraint(label TEXT, property_key TEXT)` —
      uniqueness enforcement at write time
- [ ] `CREATE CONSTRAINT ON (n:Label) ASSERT n.prop IS UNIQUE`
- [ ] `CREATE CONSTRAINT ON (n:Label) ASSERT EXISTS(n.prop)`
- [ ] `CREATE INDEX ON :Label(prop)` / `DROP INDEX`
- [ ] `SHOW CONSTRAINTS`, `SHOW INDEXES`
- [ ] Target: `SchemaAcceptance`; TCK ~85%

**Exit criteria**: IS-1 node lookup within 2× of AGE (property index used);
schema DDL Cypher commands work; ~85% TCK pass.

---

### Phase 8 — Performance Hardening and 100% TCK Compliance (v0.17.0–v0.19.0)

**v0.17.0 — Query optimisation + remaining correctness**:
- [ ] Cost model for AM scan operators: adjacency-follow O(degree) vs B-tree
      O(log N + degree) using `pg_class.reltuples` for label selectivity
- [ ] Join order enumeration for multi-hop MATCH patterns
- [ ] Predicate pushdown into the AM scan (WHERE on indexed properties as scan
      predicates, not post-filters)
- [ ] `pg_eddy.cypher_explain(analyze := TRUE)` with per-operator timings
- [ ] Pattern expressions in RETURN / WHERE (deferred from v0.13.0)
- [ ] Remaining wrong-result fixes
- [ ] Target: TCK ~95%

**v0.18.0 — TCK gap closure**:
- [ ] Remaining temporal type edge cases: timezone-aware datetime operations,
      `LOAD CSV FROM 'path' AS row` (local filesystem only)
- [ ] All remaining TCK group failures fixed; target: 100% pass rate
      (document any spec deviations with upstream references)
- [ ] `null_semantics.sql` regression suite covers all `NullAcceptance`
      scenarios
- [ ] Parallel label scan via PostgreSQL parallel worker infrastructure

**v0.19.0 — Production readiness**:
- [ ] LDBC SNB IS-1 through IS-7 and IC-1 through IC-14 benchmarked in full
      (extending the v0.12.x IS-1/IS-3 baseline to the complete suite);
      published baselines with hardware spec, dataset size, and raw output;
      compared against AGE on identical hardware
- [ ] CI performance gate: LDBC SNB IS regression `>10%` fails build
- [ ] `pg_eddy.stats()`, `pg_eddy.health_check()`, `pg_eddy.query_log`
- [ ] `pg_stat_pg_eddy` view
- [ ] Prometheus metrics via `pg_eddy_http` companion binary
- [ ] `NOTIFY`-based alerting: `pg_eddy.alert_channel` GUC
- [ ] Security: `cargo audit --deny warnings`, SBOM (CycloneDX), fuzz coverage
      report; `pg_eddy.max_cypher_depth` GUC (DoS prevention)
- [ ] mdBook documentation site: installation, quickstart, Cypher reference,
      storage AM internals, performance cookbook,
      security guide, troubleshooting
- [ ] Docker image + CNPG CloudNativePG extension image published
- [ ] `justfile` release workflow: tag, build, publish to ghcr.io

**Exit criteria** (v1.0 readiness): 100% TCK pass (goal; document any
remaining spec deviations); LDBC SNB full suite published baselines (IS-1
through IS-7, IC-1 through IC-14); AGE comparison passes 2× gate on IS-3
(first confirmed at v0.12.x; IS-1 competitive after property indexes in
v0.16.0); pg_dump round-trip verified;
`pg_eddy.health_check()` returns OK; Docker + CNPG images published.

---

## 10. Testing Strategy

### 10.1 Unit Tests

- pgrx `#[pg_test]` for every `#[pg_extern]` function
- Pure Rust unit tests (`#[test]`) for: property encoder/decoder round-trips,
  Cypher lexer tokens, parser AST correctness on golden inputs, SQL generator
  output on fixed logical plans
- Property-based tests (`proptest`): property encode/decode invertibility for
  all type tags; Cypher expression normalisation idempotency

### 10.2 Integration Tests

`cargo pgrx regress` with pg_regress test files under `sql/regress/`:

| File | Coverage |
|---|---|
| `schema.sql` | Extension create/drop, schema created, catalogs initialised |
| `node_crud.sql` | Create, read, update, delete nodes; label operations |
| `edge_crud.sql` | Create, read, update, delete edges; detach-delete |
| `properties.sql` | All property types, round-trip encoding, overflow |
| `indexes.sql` | Create/drop property indexes; index-assisted queries |
| `constraints.sql` | Unique and existence constraint enforcement |
| `cypher_match.sql` | MATCH/RETURN queries across all plan types |
| `cypher_write.sql` | CREATE, MERGE, SET, REMOVE, DELETE |
| `cypher_aggregation.sql` | COUNT, SUM, AVG, COLLECT, stDev |
| `cypher_paths.sql` | Variable-length paths, shortestPath, allShortestPaths |
| `cypher_subquery.sql` | CALL subqueries, EXISTS patterns |
| `cypher_functions.sql` | All built-in functions |
| `cypher_injection.sql` | Adversarial inputs: SQL metacharacters in property values |
| `bulk_import.sql` | CSV import of nodes and edges |
| `concurrent.sql` | Parallel inserts and reads, no data corruption |
| `am_scan.sql` | Adjacency-follow scan, label scan, property scan |
| `ivm_views.sql` | Graph views with pg-trickle (see [`plans/ivm_plan.md`](ivm_plan.md)) |
| `pg_dump.sql` | pg_dump/pg_restore round-trip preserves graph exactly |

### 10.3 openCypher TCK Harness

The TCK harness (`tests/tck/`) reads the official openCypher TCK `.feature`
files (Gherkin scenarios), executes each scenario against pg_eddy via
`pg_eddy.cypher()`, and compares results.

- Harness runs in CI on every pull request
- Per-feature pass/fail report published as a CI artifact
- Newly failing scenarios cause the build to fail
- Initially only a whitelist of known-passing features is required; the
  whitelist grows as features are implemented

**Critical anti-pattern — TCK-driven point solutions**: experience in similar
projects shows that chasing individual `not ok` TCK lines leads to a pile of
special cases: hardcoded expression patterns, parser branches that only handle
the exact syntax that appears in a failing scenario, and executor hacks that
pass a test without implementing the general feature. The result is a growing
pass rate that does not reflect real-world query support.

**The rule**: the TCK pass rate is an *output* of implementing the grammar
correctly, never an *input* that drives what to implement next. Concretely:

1. **Implement per clause, not per test**: each milestone targets a Cypher
   clause or operator group (e.g. `WITH`, `OPTIONAL MATCH`, `UNWIND`). When
   the clause is implemented completely against the grammar, run the TCK to
   see what it unlocks. Do not start by looking at which tests fail.

2. **Grammar is the spec**: if the openCypher EBNF grammar allows a construct,
   the implementation must handle it — not just the subset that appears in
   TCK scenarios. After implementing a feature, write at least two hand-crafted
   queries that exercise it in ways not present in the TCK.

3. **Skip list discipline**: the `@UNSUPPORTED_QUERY_PATTERNS` list in
   `tests/tck/run_tck.pl` must only contain Cypher *clauses* or *function
   names* that are entirely unimplemented (`CREATE`, `MERGE`, `shortestPath`,
   etc.). It must never contain regex patterns matching specific query shapes
   or property names. A growing skip list with ad-hoc patterns is a sign that
   TCK-driven development has crept in.

4. **Generality test**: when a new group of TCK scenarios passes, add one
   pgrx `#[pg_test]` that exercises the same feature with a different query
   shape (different labels, different property names, different pattern
   topology). If it fails, the implementation is a point solution, not a
   general feature.

### 10.4 Fuzz Testing

- `cargo-fuzz` targets:
  - `fuzz_cypher_parser`: random Cypher text → parser must not panic or produce
    invalid ASTs
  - `fuzz_cypher_sql_gen`: random valid ASTs → SQL generator must not panic or
    produce SQL injection
  - `fuzz_prop_decoder`: random byte sequences → property decoder must not panic
  - `fuzz_am_page_reader`: random page bytes → AM scan must not panic (for
    on-disk corruption resilience)
- Run nightly in CI (120 s per target); panic-free required gate

### 10.5 Performance Regression

- LDBC SNB interactive queries IS-1 through IS-7 benchmarked on every push to
  `main`; `>10%` regression on any query fails CI
- Baseline: documented in `benchmarks/README.md` with hardware spec and dataset
  size
- Criterion micro-benchmarks for: property encode/decode throughput, adjacency
  list follow latency, Cypher→SQL translation latency

### 10.6 Concurrency and Crash-Recovery Tests (TAP)

These tests require a real multi-process PostgreSQL cluster and cannot run
inside pgrx unit tests. They use the `PostgreSQL::Test::Cluster` Perl module
that ships with the PG 18 dev package (`postgresql-server-dev-18`).

**Infrastructure setup** (delivered in Phase 4 / v0.5.0):

```
tests/tap/
  001_crash_recovery.pl      # node WAL durability
  002_edge_crash_recovery.pl # edge + adjacency chain WAL durability
  003_mvcc_isolation.pl      # snapshot isolation across sessions
  004_concurrent_inserts.pl  # parallel inserts, no duplicates
Makefile                     # `pg_prove -r tests/tap/` target
justfile task: `tap`         # alias for `make tap`
.github/workflows/tap.yml    # CI job
```

**How to run locally**:
```bash
just tap                  # runs all TAP tests via pg_prove
prove tests/tap/001_crash_recovery.pl   # single test
```

**Prerequisites** (one-time dev-container setup):
```bash
sudo apt-get install -y postgresql-server-dev-18 libtap-parser-sourcehandler-pgtap-perl
```

**Individual test scenarios**:
- `001_crash_recovery.pl` — insert 10K nodes, `pg_ctl stop -m immediate`,
  restart, verify `pg_eddy.node_count()` = 10K; uses `XLOG_PG_EDDY_NODE_INSERT`
  redo
- `002_edge_crash_recovery.pl` — insert 1K edges with adjacency chains,
  crash-stop, recover, verify `pg_eddy.neighbours()` returns correct sets
- `003_mvcc_isolation.pl` — T1 `BEGIN`; T1 inserts node; T2 `BEGIN` + `SET
  TRANSACTION ISOLATION LEVEL REPEATABLE READ`; T1 `COMMIT`; T2
  `pg_eddy.node_count()` must equal pre-insert count; T2 `COMMIT`; recheck
  returns new count
- `004_concurrent_inserts.pl` — spawn 8 background psql sessions each
  inserting 1K nodes; join all; assert total = 8K with `am_stats()` live count
- Deadlock detection: `SET lock_timeout = '5s'` in all concurrent tests;
  any lock timeout is a TAP `not ok`

### 10.7 Known TCK Hard Cases

The following feature groups require disproportionate implementation effort
and are called out explicitly to avoid underestimating scope.

| TCK Feature Group | Difficulty | Primary challenge |
|---|---|---|
| `TemporalArithmeticAcceptance` | **Very High** | ISO 8601 duration arithmetic: `P1Y2M` + `P3M` = `P1Y5M`; leap-month overflow (`Jan 31 + P1M` = `Feb 28/29`); timezone-aware datetime arithmetic. Requires a dedicated `src/cypher/temporal.rs` module. ~80 scenarios. |
| `NullAcceptance` | **High** | Null semantics in list operations, pattern predicates, and string functions (see §6.6). All ~50 scenarios must pass before v1.0. |
| `MatchAcceptance2` | **High** | Complex optional patterns, double-optional, optional-with-WHERE, matched-but-unbound variables. ~40 scenarios. |
| `SubqueryAcceptanceTest` | **High** | Correlated `CALL { }` subqueries with complex outer variable bindings; aggregation inside subqueries. ~30 scenarios. |
| `SyntaxExceptionAcceptance` | **Medium** | Tests that specific invalid queries raise specific error types matching the openCypher error taxonomy exactly. ~30 scenarios. |
| `ListComprehensionAcceptance` | **Medium** | Nested list comprehensions with WHERE clauses; comprehensions over pattern expressions. ~25 scenarios. |
| `PatternComprehensionAcceptance` | **Medium** | Pattern expressions as values in RETURN; comprehensions with named path variables. ~20 scenarios. |
| Relationship uniqueness in path results | **Medium** | `MATCH p = (a)-[*]-(b)` — the same relationship may not appear twice in a single path result row (separate from node isomorphism; separate from the per-path `rel_ids` exclusion used during traversal; must hold across join-expanded result rows). |

**Known deviations from the openCypher specification** (documented, not bugs):
- Neo4j-specific built-in procedures (`db.labels()`, `db.schema()`, `apoc.*`)
  are not part of the openCypher specification and will not be implemented
- `LOAD CSV` from remote HTTP URLs is disabled by default; local filesystem
  paths only (security: prevents SSRF via crafted Cypher queries)

**Temporal arithmetic note**: this is the single most implementation-intensive
feature group. The ISO 8601 arithmetic rules for months and years are
non-trivial (adding a duration to a date at month-end uses different semantics
than adding to a mid-month date). Budget a full version cycle for this work.

---

## 11. Project Structure

```
pg-eddy/                               # Repository root
├── Cargo.toml                         # [workspace] manifest
├── pg_eddy/                           # Extension crate
│   ├── Cargo.toml
│   ├── pg_eddy.control
│   ├── sql/
│   │   ├── pg_eddy--0.1.0.sql        # Initial extension SQL
│   │   └── pg_eddy--X.Y.Z--X.Y+1.0.sql  # Upgrade scripts
│   └── src/
│       ├── lib.rs                     # Entry point, GUCs, _PG_init
│       ├── error.rs                   # PE### error taxonomy (thiserror)
│       ├── catalog/
│       │   ├── mod.rs
│       │   ├── labels.rs
│       │   ├── types.rs
│       │   └── property_keys.rs
│       ├── storage/
│       │   ├── mod.rs
│       │   ├── am.rs                  # AM registration, unsafe C callbacks
│       │   ├── node_store.rs          # Node page layout (split-region design)
│       │   ├── edge_store.rs          # Edge page layout, linked-list ops
│       │   ├── prop_store.rs          # Property encoding/overflow
│       │   ├── wal_records.rs         # Custom WAL record types + redo functions
│       │   ├── scan.rs                # Custom scan paths (full, label, adjacency-follow)
│       │   ├── mvcc.rs                # MVCC visibility helpers
│       │   └── wal_decode.rs          # Logical decoding output plugin (post-v1.0)
│       ├── cypher/
│       │   ├── mod.rs
│       │   ├── lexer.rs
│       │   ├── parser.rs
│       │   ├── ast.rs
│       │   ├── algebra.rs             # Logical plan IR
│       │   ├── planner.rs             # AST → logical plan (node isomorphism here)
│       │   ├── sql_gen.rs             # Logical plan → SQL
│       │   ├── executor.rs            # SPI execution + decoding
│       │   ├── expressions.rs         # Rust expression evaluator (null semantics)
│       │   ├── functions.rs           # Built-in Cypher functions
│       │   ├── expand.rs              # pg_eddy.expand() LATERAL SRF
│       │   ├── path_search.rs         # shortestPath/allShortestPaths BFS in Rust
│       │   ├── temporal.rs            # ISO 8601 duration/datetime arithmetic
│       │   └── plan_cache.rs
│       ├── ecosystem/
│       │   └── trickle.rs
│       ├── stats/
│       │   ├── mod.rs
│       │   └── monitoring.rs
│       └── admin/
│           ├── mod.rs
│           ├── maintenance.rs
│           └── constraints.rs
├── pg_eddy_http/                      # Companion HTTP binary (placeholder)
│   ├── Cargo.toml
│   └── src/
│       └── main.rs
├── benchmarks/
│   ├── README.md                      # Hardware spec, dataset, methodology
│   ├── ldbc_snb/                      # LDBC SNB query scripts
│   └── neo4j_compare/                 # Comparison scripts
├── fuzz/
│   ├── Cargo.toml
│   └── fuzz_targets/
│       ├── fuzz_cypher_parser.rs
│       ├── fuzz_cypher_sql_gen.rs
│       ├── fuzz_prop_decoder.rs
│       └── fuzz_am_page_reader.rs
├── tests/
│   ├── tck/                           # openCypher TCK harness
│   │   ├── runner.rs
│   │   └── features/                  # Symlink or copy of openCypher TCK .feature files
│   └── integration/
│       └── ...
├── sql/
│   ├── regress/
│   │   ├── sql/                       # pg_regress input SQL
│   │   └── expected/                  # Expected output
│   └── bench/
│       └── ldbc_snb.sql
├── plans/
│   ├── implementation_plan.md         # This document
│   └── ivm_plan.md                    # IVM / pg-trickle integration (separate plan)
├── docs/
│   ├── book.toml
│   └── src/
│       ├── introduction.md
│       ├── installation.md
│       ├── quickstart.md
│       ├── cypher-reference/
│       ├── storage-am/
│       ├── performance/
│       └── reference/
├── AGENTS.md
├── CHANGELOG.md
├── ROADMAP.md
├── CONTRIBUTING.md
├── LICENSE
└── justfile
```

---

## 12. Build & Development Setup

```bash
# Prerequisites
rustup update stable                 # Rust 1.85+ (pgrx 0.18 requirement)
cargo install cargo-pgrx --version 0.18.0 --locked
cargo pgrx init --pg18 download      # Download and compile PG18

# Development cycle
cargo pgrx run pg18                  # Start psql with pg_eddy loaded
cargo pgrx test pg18                 # Run #[pg_test] tests
cargo pgrx regress pg18              # Run pg_regress test suite
cargo pgrx package --pg18            # Build installable .so + SQL

# Benchmarks
just bench-ldbc                      # Run LDBC SNB benchmark suite
just bench-compare-neo4j             # Run comparison benchmark

# TCK
just test-tck                        # Run openCypher TCK harness
just test-tck-feature MatchAcceptance  # Run a single feature group

# Fuzz (requires cargo-fuzz and nightly)
cargo +nightly fuzz run fuzz_cypher_parser -- -max_total_time=120
```

### Root `Cargo.toml`

```toml
[workspace]
members  = ["pg_eddy", "pg_eddy_http"]
resolver = "3"
```

### `pg_eddy/Cargo.toml`

```toml
[package]
name    = "pg_eddy"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib", "lib"]

[features]
default = ["pg18"]
pg18    = ["pgrx/pg18"]

[dependencies]
pgrx          = "0.18"
xxhash-rust   = { version = "0.8", features = ["xxh3"] }
serde         = { version = "1", features = ["derive"] }
serde_json    = "1"
thiserror     = "2"

[dev-dependencies]
pgrx-tests = "0.18"
proptest   = "1"
criterion  = { version = "0.5", features = ["html_reports"] }
```

### `pg_eddy/pg_eddy.control`

```
default_version = '0.1.0'
module_pathname = '$libdir/pg_eddy'
comment         = 'Native LPG graph database with OpenCypher'
schema          = 'pg_eddy'
relocatable     = false
superuser       = false
trusted         = false
```

- `trusted = false` from v0.1.0: the custom AM and WAL resource manager
  require registration at postmaster start via `shared_preload_libraries`

---

## 13. Canonical GUC Reference

All GUCs exposed by pg_eddy, listed alphabetically. **Startup** GUCs must be
set in `postgresql.conf`; all others can be changed per-session.

| GUC | Type | Default | Range | Introduced | Notes |
|---|---|---|---|---|---|
| `pg_eddy.adjacency_cache_size` | INT | 65536 | 1–10,000,000 | v0.6.0 | Per-backend in-memory cache of recently visited node adjacency heads (page + slot). |
| `pg_eddy.alert_channel` | TEXT | `'pg_eddy_alerts'` | Any NOTIFY channel name | v0.24.0 | PostgreSQL NOTIFY channel for health alerts. |
| `pg_eddy.label_cache_size` | INT | 4096 | 1–100,000 | v0.1.0 | Per-backend label name→ID cache entries. |
| `pg_eddy.max_cypher_depth` | INT | 200 | 1–10,000 | v0.25.0 | Maximum AST depth for a Cypher query; deeper queries rejected at parse time. |
| `pg_eddy.max_path_depth` | INT | 100 | 1–10,000 | v0.5.0 | Maximum recursion depth for variable-length path queries (`*`). |
| `pg_eddy.max_property_chain_pages` | INT | 64 | 1–1,024 | v0.6.0 | Maximum overflow property page chain length per node or relationship. |
| `pg_eddy.plan_cache_size` | INT | 512 | 0–100,000 | v0.4.0 | Per-backend Cypher→SQL plan cache entries. `0` disables. |
| `pg_eddy.prop_inline_bytes` | INT | 48 | 16–4096 | v0.6.0 | **Startup.** Maximum inline property bytes per slot record. Values exceeding this go to overflow pages. Cannot be changed without reinitialising storage. |
| `pg_eddy.shared_memory_size` | INT | 134217728 | 1 MB–system limit | v0.6.0 | **Startup.** Total shared memory block size declared to PostgreSQL. Must be ≥ `adjacency_cache_size × 24`. |
| `pg_eddy.string_collation` | TEXT | `'default'` | `'default'` or `'C'` | v0.5.0 | Collation for property string comparisons in B-tree indexes. `'default'` uses database `LC_COLLATE`; `'C'` uses byte-order (faster, locale-independent). **Startup.** Cannot be changed without reindexing. |
| `pg_eddy.traversal_work_mem` | INT | 65536 | 1,024–1,073,741,824 (bytes) | v0.10.0 | Per-query memory budget for BFS/DFS traversal buffers. |
| `pg_eddy.vacuum_freeze_threshold` | INT | 50000 | 1–10,000,000 | v0.8.0 | Minimum dead tuple count before a page is considered for VACUUM. |

---

## 14. Error Code Taxonomy

Error messages use PostgreSQL-style formatting (lowercase first word, no
trailing period). Codes use the `PE` prefix:

| Range | Category |
|---|---|
| `PE001`–`PE099` | Catalog errors (label/type registration, ID allocation) |
| `PE100`–`PE199` | Storage AM errors (page corruption, write failures, MVCC) |
| `PE200`–`PE299` | Cypher parse errors (syntax, unsupported constructs) |
| `PE300`–`PE399` | Cypher plan/execution errors (type errors, runtime failures) |
| `PE400`–`PE499` | Constraint errors (unique violation, existence violation) |
| `PE500`–`PE599` | Import/export errors (CSV format, path access) |
| `PE600`–`PE699` | IVM / pg-trickle integration errors (see [`plans/ivm_plan.md`](ivm_plan.md)) |
| `PE700`–`PE799` | Admin errors (vacuum, reindex, migration) |
| `PE800`–`PE899` | Configuration errors (invalid GUC combinations) |

---

## 15. Security Considerations

- **Cypher injection**: all user-supplied string values in Cypher property
  comparisons are passed as SPI bind parameters (`$N`); the Cypher→SQL
  translator never interpolates user-controlled strings into SQL text. All
  property key names and label names go through the integer registry (no SQL
  metacharacters possible)
- **AM page validation**: the custom AM's page reader validates magic numbers
  and slot offsets before dereferencing pointers; malformed pages raise a
  `PE100` error rather than crashing
- **Resource limits**: `pg_eddy.max_path_depth`, `pg_eddy.max_cypher_depth`,
  and `statement_timeout` together prevent runaway recursive traversals and
  deeply nested queries
- **Privilege model**: `pg_eddy.*` functions default to `SECURITY INVOKER`;
  the `_pg_eddy` internal schema is accessible only to the extension owner;
  label-level RLS is managed via `pg_eddy.grant_label()` (v0.25.0)
- **WAL decode plugin security**: the logical decoding output plugin only
  processes WAL records from `pg_eddy`'s own relation OIDs; it validates OID
  matches before decoding to prevent processing records from unrelated tables
- **Memory safety**: all Rust code is safe except the AM callbacks (`unsafe
  extern "C"`); every unsafe block has a safety comment documenting the
  invariants being upheld; `cargo clippy` with `#![deny(unsafe_op_in_unsafe_fn)]`

---

## 16. Future Architecture (Post-v1.0)

These items are documented for architectural awareness and are not in the v0.x
scope:

- **Multi-graph support**: v1.0 is single-graph-per-database. Multi-graph
  (named graph namespaces, `USE graph_name` syntax) is a post-v1.0 feature.
  AGE users expect named graphs; this is a known adoption gap. The storage
  layer is designed to support multi-graph via a `graph_id` column in the
  MVCC node/edge records and catalog tables, but the query engine and SQL API
  do not expose it until post-v1.0. (Note: `graph_id` is distinct from
  `graph_partition_id` in the adjacency header — the former identifies which
  named graph a node belongs to, the latter identifies its distribution
  partition for Citus.)
- **pg-trickle WAL CDC via custom output plugin**: see
  [`plans/ivm_plan.md`](ivm_plan.md) §4 for the full architecture. This is a
  post-v1.0 performance optimization on top of working trigger-based IVM.
- **VarLengthExpand v2**: native expansion operator in Rust that uses the
  adjacency-follow SRF directly (not recursive CTEs), with pruning, early
  termination, and `traversal_work_mem` budgeting. Recursive CTEs are correct
  but can be memory-hungry on deep traversals; a native operator would allow
  tighter control over memory and cancellation.
- **GQL support**: ISO SQL:2023 Part 16 (GQL) query language alongside
  OpenCypher; share the same logical plan IR
- **SPARQL / RDF bridge**: interoperability with pg-ripple — project LPG
  subgraphs as RDF for SPARQL consumption
- **Distributed execution via Citus**: horizontal sharding of node/edge pages
  across Citus worker nodes; shard by community/partition for locality.
  **Scaling design rationale** (documented here to prevent day-1 decisions
  that block future distribution):
  - **Read scaling is free today**: PostgreSQL physical streaming replication
    replicates pg_eddy's custom AM pages via WAL. Read replicas work
    out-of-the-box with zero pg_eddy changes. This is pg_eddy's equivalent of
    Neo4j Autonomous Clustering — and it's free, not a paid tier.
  - **IDs are globally unique**: 64-bit node_id/rel_id via xxhash. No shard-
    local assumptions. Safe for distribution without ID remapping.
  - **APIs are logical, not physical**: all user-facing APIs (`cypher()`,
    `expand()`, `neighbours()`) use logical IDs, never page/slot TIDs. A
    distributed `expand()` could route cross-shard hops transparently without
    API changes.
  - **Adjacency chains are inherently local**: the `(page_id, slot_id)` chain
    pointers in edge records are physical references within a single relation
    on a single instance. This is by design — it's the source of O(degree)
    traversal performance. Any distributed approach must handle cross-shard
    edges differently (e.g., edge proxies on the coordinator, remote expand
    RPCs, or community-based colocation that minimizes cross-shard edges).
    Neo4j only solved this with Infinigraph in late 2025 after 15 years of
    single-instance design. pg_eddy should not attempt this before v1.0.
  - **Partition strategy**: Citus distribution key options for graphs:
    (a) `node_id` hash — even distribution but random cross-shard edges;
    (b) `community_id` / `partition_id` — colocate densely-connected
    subgraphs on the same worker to minimize cross-shard hops (requires
    community detection as a preprocessing step, e.g., via Louvain or label
    propagation); (c) label-based — colocate all nodes of a label on one
    worker (only useful for label-homogeneous workloads). Option (b) is the
    most promising but requires a `graph_partition_id` column in the adjacency
    header — **this column should be reserved in the header format from
    Phase 1** even if unused until post-v1.0.
  - **pg_eddy.expand() must remain the traversal boundary**: all traversal
    goes through the `expand()` SRF, never raw adjacency chain following in
    user code. This means a distributed version only needs to intercept
    `expand()` calls that cross shard boundaries — the contract is clean.
  - **Federation (Neo4j Fabric equivalent)**: PostgreSQL FDW + postgres_fdw
    already enables cross-database queries. pg_eddy graph views could
    reference remote pg_eddy tables via FDW. This is a natural extension of
    the existing PostgreSQL infrastructure, not a custom protocol.
- **pg-trickle graph analytics**: PageRank, betweenness centrality, connected
  components as incrementally-maintained views (depends on IVM — see
  [`plans/ivm_plan.md`](ivm_plan.md))
- **Graph Neural Network embeddings**: store node/edge embeddings alongside
  graph data; combined Cypher + vector similarity queries
- **Bolt protocol**: native Bolt v5 wire protocol for Neo4j driver compatibility
  without modification (`pg_eddy_http` extension)
- **Property graph schema (PG Schema)**: declarative schema language for LPG
  (ISO GQL standard)
- **Temporal graph queries**: bitstring validity columns for versioned graph
  snapshots and as-of queries
- **`pg_upgrade` compatibility**: structural migration of custom AM pages
  between major PostgreSQL versions

---

## 17. Deferred Deliverables Tracker

Items deferred from completed phases, consolidated here so nothing is lost.
Each item notes its origin phase and its current planned target.

### Storage Layer

| Item | Origin | Target | Notes |
|---|---|---|---|
| REPLICA IDENTITY support | Phase 2 | Phase 7+ (IVM prerequisite) | Custom AM tables have no SQL columns; requires slot callbacks with column data. See [`plans/ivm_plan.md`](ivm_plan.md) §1. |
| Slot callback verification | Phase 2 | Phase 7+ | Verify slot callbacks produce correct `TupleTableSlot` for trigger machinery. |
| Insert performance fix (5× slower than AGE) | v0.5.2 | ✅ v0.12.1 | Batch catalog writes via `CatalogWriteBuffer`; resolved. |
| Cross-page node update | Phase 3 | v0.15.0 | `update_node` fails with PE201 when page is too full for old+new record to coexist. Fix: delete-and-reinsert on a different page. Bounded risk: PROP_INLINE_MAX=48 and MAX_LABELS=32 cap max record size at ~224 bytes. Also: missing MAXALIGN in `update_node` free-space check (uses raw `new_item.len()` instead of `(new_item.len() + 7) & !7`). |
| `clear()` storage corruption | v0.13.0 | v0.15.0 | `RelationTruncate(rel, 0)` causes `could not read blocks` and `PageAddItemExtended failed` in subsequent scenarios. 70% of TCK failures. |

### Indexes, Constraints, Import/Export

| Item | Origin | Target | Notes |
|---|---|---|---|
| Property indexes (`create_node_index`) | v0.5.3 | v0.16.0 | Per-property B-tree index; design alongside query planner for predicate pushdown. |
| Unique constraints | v0.5.3 | v0.16.0 | `CREATE CONSTRAINT ... ASSERT n.prop IS UNIQUE` |
| Existence constraints | v0.5.3 | v0.16.0 | `CREATE CONSTRAINT ... ASSERT EXISTS(n.prop)` |
| CSV import/export | v0.5.3 | v0.18.0+ | `load_csv_nodes`, `load_csv_edges` with `fast := TRUE` option; `export_cypher_script()` |
| `pg_dump`/`pg_restore` round-trip | v0.5.3 | v0.18.0 | Test on 1M+ node graph; must be lossless. |
| Performance CI gate | v0.5.3 | v0.19.0 | Automated per-PR: label-scan <5ms on 1M nodes; 1-hop expand <1ms on 10M edges. |

### Testing

| Item | Origin | Target | Notes |
|---|---|---|---|
| Fuzz targets (lexer, parser) | v0.7.0 | v0.7.0 | `fuzz/` crate with `fuzz_cypher_parser`, `fuzz_cypher_sql_gen`, etc. |
| pg-trickle smoke test | Phase 2 | IVM plan | See [`plans/ivm_plan.md`](ivm_plan.md). |

### IVM (Separate Plan)

All IVM deliverables (graph views, constraint views, WAL CDC) are tracked in
[`plans/ivm_plan.md`](ivm_plan.md) and are not listed here.

---

## 18. References

- [openCypher Specification](https://opencypher.org/)
- [openCypher TCK](https://github.com/opencypher/openCypher/tree/master/tck)
- [PostgreSQL Table AM API](https://www.postgresql.org/docs/current/tableam.html)
- [pgrx 0.18](https://github.com/pgcentralfoundation/pgrx)
- [pg-trickle](https://github.com/trickle-labs/pg-trickle) — IVM integration
  planned separately (see [`plans/ivm_plan.md`](ivm_plan.md))
- [pg-ripple](https://github.com/trickle-labs/pg-ripple) — sister project (RDF
  triplestore on PostgreSQL using pg-trickle, similar architecture without
  custom AM)
- [LDBC Social Network Benchmark](https://ldbcouncil.org/benchmarks/snb/)
- [Neo4j native graph storage whitepaper](https://neo4j.com/blog/native-vs-non-native-graph-technology/)
