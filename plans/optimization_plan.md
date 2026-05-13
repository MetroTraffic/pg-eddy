# pg_eddy — Performance Optimization Plan

This document catalogues identified optimization opportunities for pg_eddy,
ordered by estimated impact. Items can be selectively pulled into the
implementation plan as time and priorities permit.

---

## Executive Summary

The current architecture delivers 1.8–4.3× faster traversal than AGE, but
absolute performance is dominated by three systemic overheads:

1. **O(N) node lookup** — `find_node_by_id` scans all blocks linearly
2. **No catalog cache** — every property key / label name requires an SPI
   round-trip
3. **Relation open/close churn** — OID resolution + CString alloc per call

Addressing just these three (#1–#3 below) would likely yield a 10–50×
improvement on multi-hop queries without any architectural changes.

---

## Tier 1 — Critical Path (Expected 10–50× on multi-hop)

### OPT-1: Node ID → Location Index

**Problem**: `find_node_by_id` and `find_node_location` perform a full
sequential scan of all node pages to locate a single node. Called once per
destination node in `exec_expand`. On a 100K node graph with 50 pages, a
3-hop query touching 1000 edges does ~50,000 page reads.

**Solution**: Maintain a B-tree index on `node_id` that maps to
`(block_number, item_offset)`. Options:

| Approach | Pros | Cons |
|---|---|---|
| PostgreSQL B-tree on a shadow heap table | Leverages existing infrastructure; crash-safe for free | Extra heap table, MVCC overhead on index entries |
| In-page hash directory (fixed first N pages) | Zero external dependency; O(1) lookup | Complex to implement; space waste on sparse graphs |
| Per-backend `HashMap<i64, (u32, u16)>` | Fastest read path; no I/O | Memory-proportional to node count; invalidation on VACUUM |
| Dedicated B-tree pages (custom AM extension) | Tightest integration; compact | Significant engineering effort |

**Recommended approach**: Start with the shadow B-tree on a catalog table
(`_pg_eddy.node_location(node_id BIGINT PRIMARY KEY, block_num INT, offset INT)`)
updated on insert/VACUUM. This leverages PostgreSQL's proven B-tree and is
O(log N) per lookup. Migrate to a per-backend hash cache later if profiling
warrants it.

**Estimated impact**: 20–100× speedup on `find_node_by_id` calls (from O(N)
to O(log N)). Translates to 5–20× end-to-end improvement on multi-hop MATCH.

---

### OPT-2: Per-Transaction Catalog Cache

**Problem**: Every call to `label_name()`, `prop_key_name()`, `rel_type_name()`
issues a full SPI query. During property decoding, this means N SPI round-trips
per node (N = number of properties). On a 5-property node, loading 1000 nodes
for a traversal result = 5000+ SPI queries just for key name resolution.

**Solution**: Add a per-transaction (or per-statement) cache:

```rust
thread_local! {
    static LABEL_CACHE: RefCell<HashMap<i32, String>> = RefCell::new(HashMap::new());
    static KEY_CACHE: RefCell<HashMap<i32, String>> = RefCell::new(HashMap::new());
    static RELTYPE_CACHE: RefCell<HashMap<i32, String>> = RefCell::new(HashMap::new());
}
```

Invalidate at transaction end via a PostgreSQL `ResourceOwner` callback or
simply clear at `pg_eddy.cypher()` entry point.

**Forward path**: Replace with `syscache` integration once stable (register
custom cache IDs for the three registry tables). This gives automatic
invalidation on concurrent DDL.

**Estimated impact**: 3–10× reduction in wall-clock time for property-rich
traversals. Eliminates the dominant per-row overhead.

---

### OPT-3: Relation Handle Reuse (Query-Scoped Open)

**Problem**: `open_nodes_relation()` and `open_edges_relation()` are called
20+ times per `exec_expand` invocation (once for adjacency follow, once per
destination node, once for overflow). Each call:
- Allocates 2 `CString`s
- Resolves namespace OID via hash lookup
- Resolves relation OID via hash lookup
- Calls `table_open`

**Solution**: Open relations once at the start of `execute()` (or even at
`pg_eddy.cypher()` entry point) and pass them through the executor as a
context struct:

```rust
struct ExecContext {
    node_rel: pg_sys::Relation,
    edge_rel: pg_sys::Relation,
    snapshot: pg_sys::Snapshot,
    // caches from OPT-2
}
```

Close at the end of the top-level call. This also simplifies the code
significantly.

**Estimated impact**: 2–5× reduction in per-hop overhead from eliminating
repetitive OID resolution and CString allocation. Compounds with OPT-1.

---

## Tier 2 — Structural Improvements (Expected 2–10×)

### OPT-4: Lazy Property Decoding (Projection Pushdown)

**Problem**: `Value::Node` and `Value::Edge` eagerly decode ALL properties
into a `serde_json::Map`, even if the query only uses one property (or none).
For `MATCH (a)-[:KNOWS]->(b) RETURN b.name`, all properties of `b` are
decoded, but only `name` is needed.

**Solution**: Two-phase approach:

**Phase A — Deferred decode**: Store `prop_bytes: Vec<u8>` in the `Value`
variant and decode on first property access:

```rust
enum Value {
    Node {
        node_id: i64,
        labels: Vec<String>,
        prop_bytes: Vec<u8>,  // raw bytes, decoded lazily
        properties: OnceCell<serde_json::Map<String, serde_json::Value>>,
    },
    ...
}
```

**Phase B — Projection-aware scan**: Planner analyzes RETURN/WHERE/ORDER BY
to determine which properties are accessed per variable. The executor only
decodes those keys. This requires a `decode_keys(bytes, &[key_id])` function
that skips non-matching keys.

**Estimated impact**: 2–5× for property-heavy workloads (nodes with 10+
properties where only 1–2 are accessed). Minimal impact on property-light
graphs.

---

### OPT-5: Row Representation — Arena / Rc-Based

**Problem**: Every row is a `HashMap<String, Value>`. During multi-hop
expansion:
- `input_row.clone()` deep-copies the entire HashMap + all Values
- Node/Edge values contain owned `Vec<String>`, `serde_json::Map`, etc.
- A 3-hop query with 1000 results clones ~3000 growing HashMaps

**Solution**: Refactor row representation:

**Phase A — Rc-shared node data**: Wrap frequently-shared data in `Rc`:
```rust
enum Value {
    Node(Rc<NodeData>),  // shared across multiple result rows
    Edge(Rc<EdgeData>),
    ...
}
```
This makes cloning O(1) for node/edge values (just Rc increment).

**Phase B — Columnar intermediate representation**: For the inner executor
loop, use a column-oriented representation:
```rust
struct ColumnBatch {
    columns: HashMap<String, Vec<Value>>,  // one Vec per variable
}
```
This eliminates per-row HashMap overhead entirely and enables vectorized
filtering.

**Estimated impact**: 3–10× for multi-hop queries with large fan-out.
Phase A alone gives 2–3× with minimal refactoring.

---

### OPT-6: Edge Chain Batching (Same-Page Coalescing)

**Problem**: `follow_chain` reads one edge slot at a time, doing
`ReadBuffer` + `LockBuffer` + `UnlockReleaseBuffer` per edge — even when
multiple consecutive edges are on the same page.

**Solution**: When following a chain and the next pointer is on the same
page as the current position, continue reading without releasing the buffer
lock. Only release when crossing a page boundary.

```rust
// Pseudocode
let mut current_buf = ReadBuffer(head_pg);
LockBuffer(current_buf, SHARE);
loop {
    // read edge at current slot
    let (next_pg, next_sl) = read_next_ptr(...);
    if next_pg == current_pg {
        // Same page — continue without release
        continue;
    }
    UnlockReleaseBuffer(current_buf);
    if next_sl == 0 { break; }
    current_buf = ReadBuffer(next_pg);
    LockBuffer(current_buf, SHARE);
}
```

**Estimated impact**: 1.5–3× on traversal of high-degree nodes where edges
cluster on a small number of pages (common after bulk insert).

---

### OPT-7: Adjacency-Follow Pre-filtering (Type + Label Pushdown)

**Problem**: `adjacency_follow` returns all edges, then the executor
filters by type and destination label. For a node with 10K edges but only
50 of type `:KNOWS`, we decode 10K edges and discard 9950.

**Current state**: Type filtering (`rel_type_filter`) IS pushed into
`follow_chain` already (✓). But destination label filtering is not — every
edge's destination node is loaded just to check its labels.

**Solution**: Add a **rel-type secondary chain** (already noted in the
implementation plan as `rel_type_index`): a separate chain per (node,
rel_type) pair that enables direct jump to edges of the desired type without
traversing unrelated edges.

For destination label pushdown: when `dst_labels` is specified, use the
label index (`_pg_eddy.label_index`) to get the set of valid destination
node IDs, then skip edges whose target is not in the set. This converts
post-filtering to pre-filtering.

**Estimated impact**: Proportional to selectivity. For a node with 10K
edges and query filtering to 50, this is ~200× fewer edge decodes.

---

## Tier 3 — Query Planning (Expected 2–5×)

### OPT-8: Cost-Based Join Ordering

**Problem**: Multi-pattern MATCH statements are planned left-to-right in
declaration order. `MATCH (a:Person {name:"Alice"})-[:KNOWS]->(b)-[:LIVES_IN]->(c:City)`
always starts from `a`, even if starting from `c:City` (assuming fewer cities
than people) and working backward would be faster.

**Solution**: Implement a simple cost model:
- Estimate cardinality per label: `count_label_nodes(label)` (already exists)
- Estimate degree per type: `avg_degree(rel_type)` (new statistic from VACUUM)
- Try all permutations of starting-label anchors (for ≤4 pattern nodes,
  enumerate; for >4, use a greedy heuristic)
- Choose the plan with minimum estimated intermediate cardinality

**Estimated impact**: 2–10× on queries with selective predicates on non-first
pattern elements. No effect on single-anchor patterns.

---

### OPT-9: Filter Pushdown Past Expand

**Problem**: `WHERE b.age > 30` in `MATCH (a)-[:KNOWS]->(b) WHERE b.age > 30`
is currently applied after the expand produces all `(a, b)` pairs. The
destination node is fully materialized before the filter runs.

**Solution**: During planning, analyze WHERE predicates and decompose into:
1. **Expand-inline filters** — predicates that reference only the destination
   variable and can be evaluated immediately after loading the destination node
   (before inserting into the result set)
2. **Post-expand filters** — predicates that reference multiple variables or
   use aggregation

The executor already has `dst_props` (inline property filters from pattern
syntax). Extend this to include pushed-down WHERE predicates.

**Estimated impact**: Proportional to filter selectivity. If 90% of
destination nodes are filtered out, this avoids 90% of row clones and
downstream processing.

---

### OPT-10: Predicate Short-Circuit in Property Decode

**Problem**: When checking `WHERE n.name = "Alice"`, the current flow:
1. Decode ALL properties of `n` into a `serde_json::Map`
2. Look up `"name"` in the map
3. Compare

If `name` is the first property in the binary encoding, we still decode all
remaining properties uselessly.

**Solution**: Implement `decode_single_key(bytes, target_key_id) -> Option<Value>`
that scans the binary property array and returns immediately upon finding
the target key, skipping the rest.

**Estimated impact**: 1.5–3× for filter-heavy queries on nodes with many
properties (10+). Minimal effect on nodes with few properties.

---

## Tier 4 — I/O and Concurrency (Expected 1.5–3×)

### OPT-11: ReadAhead / Prefetch for Sequential Scan

**Problem**: `NodeScanState` reads one page at a time. On I/O-bound
workloads (graph larger than `shared_buffers`), each page read is a
synchronous I/O wait.

**Solution**: Use PostgreSQL 18's `read_stream` API (asynchronous I/O) for
the sequential scan path. Register a callback that returns the next N block
numbers, enabling the kernel to prefetch pages.

For adjacency chains (less predictable access pattern), use
`PrefetchBuffer()` on the next chain page while processing the current one.

**Estimated impact**: 1.5–3× on I/O-bound scans. No effect when data fits
in `shared_buffers`.

---

### OPT-12: Parallel Adjacency-Follow

**Problem**: `exec_expand` processes input rows serially. Each row's
adjacency follow is independent of others.

**Solution**: Use PostgreSQL's parallel query infrastructure:
1. Register parallel scan callbacks in the AM
2. Allow the executor to split input rows across parallel workers
3. Each worker follows adjacency chains independently; results merge at
   the gather node

Alternatively, a simpler in-process approach: use Rust's `rayon` or manual
chunking to process multiple starting nodes concurrently (requires careful
handling of buffer manager re-entrancy).

**Estimated impact**: Linear with worker count (2–4×) for large fan-out
queries. Overhead dominates for small result sets.

---

### OPT-13: Buffer Access Strategy for Chain Following

**Problem**: Long adjacency chain traversals (high-degree nodes with edges
spread across many pages) can evict hot pages from `shared_buffers` (buffer
cache pollution).

**Solution**: Use a `BAS_BULKREAD` buffer access strategy for
`follow_chain` when the degree estimate (from the adjacency header)
exceeds a threshold (e.g., 1000 edges). This tells the buffer manager to
use a small ring buffer, preventing eviction of other hot data.

**Estimated impact**: Prevents performance cliffs on large traversals;
improves system-wide throughput under concurrent workloads.

---

## Tier 5 — Write Path Optimizations (Expected 2–5×)

### OPT-14: Batch Node/Edge Insert

**Problem**: `create_node()` and `create_edge()` operate one-at-a-time.
Each call: allocates a sequence value (SPI), opens a relation (2× CString +
OID resolve), finds/extends a page, WAL-logs, and closes.

**Solution**: Expose a batch API:
```sql
SELECT pg_eddy.batch_create_nodes(
    labels_array TEXT[][],
    props_array JSONB[]
) RETURNS BIGINT[];
```

Internally: open relation once, batch-allocate sequence values, fill pages
contiguously, WAL-log page-at-a-time (one record per full page rather than
per node).

**Estimated impact**: 3–5× on bulk insert throughput. Critical for ETL and
benchmark loads.

---

### OPT-15: Deferred Adjacency Header Update

**Problem**: Each `insert_edge` updates two adjacency headers (source
out-chain, target in-chain), each requiring an exclusive lock on the node
page. Under concurrent edge inserts to the same node, this creates lock
contention.

**Solution**: Buffer adjacency header updates in a per-transaction list.
At commit time, batch-apply all updates, sorting by page to minimize lock
acquire/release cycles. This reduces the lock window from the full
`insert_edge` duration to a brief batch-update pass.

**Estimated impact**: 2–3× on concurrent edge insert throughput to
high-degree nodes.

---

## Tier 6 — Memory and Allocation (Expected 1.5–2×)

### OPT-16: Property Bytes Pool Allocator

**Problem**: Every `NodeRecord` and `EdgeRecord` allocates a fresh
`Vec<u8>` for `prop_bytes`. On high-throughput traversals, this creates
millions of small allocations.

**Solution**: Use a reusable buffer pool (e.g., `Vec<u8>` arena per query)
that recycles byte vectors. The `decode` function writes into a
caller-supplied buffer rather than returning a new `Vec`.

**Estimated impact**: 1.3–1.5× from reduced allocator pressure. Most
impactful on tight traversal loops.

---

### OPT-17: String Interning for Labels and Types

**Problem**: `label_name()` returns `String` (heap-allocated) for every
node's labels. With 10 distinct labels across 100K nodes, we allocate
100K+ identical strings.

**Solution**: Use a string interning table (per-query or per-backend):
```rust
struct Interner {
    map: HashMap<i32, &'static str>,  // leaked on purpose, or Rc<str>
}
```

Return `Rc<str>` or `&str` references instead of owned `String`. Combined
with OPT-2 (catalog cache), this eliminates both the SPI overhead and the
allocation overhead for label/type names.

**Estimated impact**: 1.2–1.5× from reduced allocation; compounds with
OPT-2.

---

## Tier 7 — Advanced / Future (Expected 2–10×)

### OPT-18: Compressed Property Pages (PGLZ / LZ4)

**Problem**: Property overflow pages store raw bytes. For text-heavy
properties (descriptions, addresses), compression could reduce I/O by 2–4×.

**Solution**: Apply PostgreSQL's PGLZ (or LZ4 in PG18) to overflow page
content. Inline properties (≤512B) remain uncompressed for direct access.
Add a 1-byte compression-method tag to the overflow page header.

**Estimated impact**: 1.5–2× reduction in I/O for text-heavy graphs.
CPU overhead is negligible for modern LZMA/LZ4.

---

### OPT-19: Edge Colocation (Community-Aware Placement)

**Problem**: Edges from a node are placed on the first available edge page
with free space. Over time (after deletes and re-inserts), edges from the
same node may scatter across many pages, degrading traversal locality.

**Solution**: Use the `graph_partition_id` field (already reserved in the
adjacency header) to influence edge page selection. Edges between nodes in
the same community cluster on the same page. A background maintenance
process (`pg_eddy.repack_edges()`) can defragment chains.

**Estimated impact**: 2–5× on I/O-bound workloads with natural community
structure (social graphs, citation networks).

---

### OPT-20: Materialized Shortest-Path Cache

**Problem**: `shortestPath()` runs a full BFS per invocation. Repeated
shortest-path queries between nearby nodes redo redundant work.

**Solution**: Maintain a bounded LRU cache of recent BFS results:
```rust
thread_local! {
    static SP_CACHE: RefCell<LruCache<(i64, i64), Vec<i64>>> = ...;
}
```

Cache entries are invalidated on edge insert/delete (clear affected source
or target entries). Bounded to prevent memory bloat.

**Estimated impact**: 5–10× for workloads with repeated shortest-path
queries to the same neighbourhood (e.g., recommendation engines).

---

### OPT-21: SIMD-Accelerated Property Comparison

**Problem**: Property comparison in WHERE clauses decodes values and
compares them one-at-a-time.

**Solution**: For equality predicates on fixed-size types (Integer, Float,
Boolean), compare the raw bytes directly without decoding:
```rust
// Instead of: decode(prop_bytes) → i64 → compare
// Do: memcmp(prop_bytes[offset..offset+8], expected_bytes, 8)
```

For string predicates, use SIMD-accelerated `memcmp` or `memmem` (already
provided by the system libc on x86-64).

**Estimated impact**: 1.3–1.5× for filter-heavy queries with many
comparisons per row.

---

### OPT-22: Incremental Statistics for Cost-Based Planning

**Problem**: No per-label, per-type, or per-property statistics are
maintained. The planner cannot make informed decisions about join order or
scan strategy.

**Solution**: Maintain statistics updated during VACUUM:
- `n_nodes_per_label`: count of live nodes per label
- `avg_degree_per_type`: average out/in degree per relationship type
- `ndistinct_per_prop`: number of distinct values per indexed property
- `histogram_bounds`: equi-depth histogram for numeric properties

Store in `_pg_eddy.graph_stats` catalog table. The planner reads these to
choose between LabelScan, PropertyIndexScan, and join ordering.

**Estimated impact**: Enables OPT-8 (cost-based join ordering). Without
statistics, the planner cannot optimize beyond declaration order.

---

## Implementation Priority Matrix

| ID | Effort | Impact | Dependencies | Suggested Release |
|---|---|---|---|---|
| OPT-2 | Small (1–2 days) | High | None | Next patch |
| OPT-3 | Small (1–2 days) | High | None | Next patch |
| OPT-1 | Medium (3–5 days) | Critical | None | Next minor |
| OPT-6 | Small (1 day) | Medium | None | Next patch |
| OPT-10 | Small (1–2 days) | Medium | None | Next patch |
| OPT-4 | Medium (3–5 days) | High | Refactors Value enum | Next minor |
| OPT-5 | Medium (3–5 days) | High | Refactors Row type | Next minor |
| OPT-7 | Medium (3–5 days) | High | OPT-1 | Next minor |
| OPT-9 | Medium (2–3 days) | Medium | Planner changes | Next minor |
| OPT-14 | Medium (3–5 days) | High (writes) | None | Next minor |
| OPT-8 | Large (5–10 days) | High | OPT-22 | v1.0 |
| OPT-22 | Medium (3–5 days) | Enabling | None | v1.0 |
| OPT-11 | Medium (3–5 days) | Medium | PG18 AIO API | v1.0 |
| OPT-15 | Medium (3–5 days) | Medium (writes) | None | v1.0 |
| OPT-12 | Large (10+ days) | High | Parallel AM callbacks | Post-v1.0 |
| OPT-17 | Small (1–2 days) | Low | OPT-2 | With OPT-2 |
| OPT-16 | Small (1–2 days) | Low | None | When profiling confirms |
| OPT-13 | Small (1 day) | Low | None | When profiling confirms |
| OPT-18 | Medium (3–5 days) | Medium | Overflow pages | Post-v1.0 |
| OPT-19 | Large (10+ days) | High (I/O) | Partitioning | Post-v1.0 |
| OPT-20 | Medium (3–5 days) | Workload-dep. | None | Post-v1.0 |
| OPT-21 | Small (1–2 days) | Low | None | When profiling confirms |

---

## Quick Wins (Ship This Week)

The following three optimizations require minimal code changes and address
the dominant bottlenecks:

1. **OPT-2**: Add `thread_local!` caches for label/key/reltype names,
   cleared at cypher() entry. ~50 lines of Rust.
2. **OPT-3**: Refactor `exec_expand` and `exec_var_length_expand` to accept
   pre-opened `Relation` handles from a shared `ExecContext`. ~100 lines of
   refactoring.
3. **OPT-6**: Add same-page coalescing to `follow_chain`. ~20 lines changed.

Combined estimated improvement on 2-hop MATCH: **5–15×**.

---

## Measurement Plan

Before and after each optimization, measure:

1. **Micro-benchmark**: `benchmarks/run_ldbc_benchmark.pl` — IS-1, IS-3
2. **TCK wall-clock**: `time just tck` — total runtime of all TCK scenarios
3. **Per-operation timing**: `EXPLAIN (ANALYZE)` on:
   - `SELECT pg_eddy.cypher('MATCH (a:Person {name:"Alice"})-[:KNOWS]->(b) RETURN b')`
   - `SELECT pg_eddy.cypher('MATCH (a:Person)-[:KNOWS*2..3]->(b) RETURN count(b)')`
4. **Memory**: RSS before/after 10K-node traversal (detect allocation bloat)
5. **WAL volume**: `pg_waldump` comparison for 1000-edge insert batch

Record results in `benchmarks/optimization_results.md` with commit hash,
date, and hardware spec.
