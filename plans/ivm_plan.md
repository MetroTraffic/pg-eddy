# pg_eddy — IVM / pg-trickle Integration Plan

> **Status**: Extracted from `implementation_plan.md` on 2026-05-09.
> This plan is **not active** — it depends on a stable, feature-complete
> graph engine (≥v0.12.0 with Cypher write clauses). Pick this up after
> the core graph engine reaches TCK compliance and the AGE benchmark gate
> passes.

## Overview

pg_eddy can integrate with [pg-trickle](https://github.com/trickle-labs/pg-trickle)
for incrementally-maintained graph views — a capability no other PostgreSQL
graph extension offers. This integration is **loosely coupled** from the core
graph engine: pg_eddy works without pg-trickle, and this plan can be executed
independently once the prerequisites are met.

**Prerequisites before starting this plan**:
- Cypher `CREATE`, `SET`, `DELETE` working (v0.12.0+)
- All write clauses go through SPI → executor → triggers fire
- REPLICA IDENTITY support on custom AM tables (slot callbacks with column data)
- TCK ≥80% (write language functional)

---

## 1. pg-trickle CDC Mode

**Executor path requirement (critical)**: pg-trickle's trigger-based CDC fires
`AFTER INSERT/UPDATE/DELETE` row-level triggers, which are invoked by
PostgreSQL's executor in `nodeModifyTable.c` via `ExecARInsertTriggers()`,
`ExecARUpdateTriggers()`, and `ExecARDeleteTriggers()` — **only when writes go
through the standard executor path**.

All Cypher write clauses (CREATE, MERGE, SET, DELETE) must therefore be
executed as standard SQL DML via SPI. The Cypher SQL generator emits
`INSERT`/`UPDATE`/`DELETE` SQL; SPI routes through the executor, which calls
the trigger manager automatically. No special pg_eddy code is needed to fire
triggers — as long as every write path goes through SPI.

**Trigger-based CDC is the only reliable integration path with pg-trickle.**
This requires explicit understanding of how pg-trickle's WAL mode works:

- pg-trickle's WAL-based CDC (`pg_trickle.cdc_mode = 'auto'` or `'wal'`) uses
  **standard PostgreSQL logical replication**: it creates a publication and
  replication slot per source table and calls `pg_logical_slot_get_changes()`
  using the `pgoutput` plugin.
- `pgoutput` decodes **heap AM WAL records** (`XLOG_HEAP_INSERT` etc.). It
  cannot decode pg_eddy's custom RMGR records (`XLOG_PG_EDDY_*`). When
  pg-trickle attempts WAL mode on a pg_eddy custom AM table, `pgoutput` sees
  no decodable changes — **pg-trickle silently captures nothing**.
- Therefore, `pg_trickle.cdc_mode` must be forced to `'trigger'` for all
  pg_eddy source tables. The `create_graph_view()` API sets this explicitly.
- This is a permanent constraint. WAL-based CDC for pg_eddy tables requires
  pg-trickle adding support for a custom output plugin — see §4.
- The write-side cost is therefore fixed at trigger overhead: **20–55 µs/row**
  (not ~5 µs as WAL mode would provide).

**Three additional trigger-based requirements**:

1. **REPLICA IDENTITY**: pg-trickle requires `REPLICA IDENTITY DEFAULT`
   (primary key) or `REPLICA IDENTITY FULL` on source tables to capture OLD
   row values on UPDATE and DELETE. pg_eddy's node and edge tables must
   declare `node_id` and `rel_id` as primary keys at the AM level, or use
   `REPLICA IDENTITY FULL`.

2. **Slot callback / tuple deconstruction**: trigger functions read OLD/NEW row
   values by deconstructing the tuple slot filled by the AM. pg_eddy's slot
   callbacks (`slot_getsomeattrs`, `slot_getallattrs`) must produce a
   complete, correctly-typed `TupleTableSlot` that standard trigger machinery
   can deconstruct.

3. **Transition table support (IMMEDIATE mode)**: pg-trickle's IMMEDIATE mode
   uses `REFERENCING NEW TABLE AS new_rows OLD TABLE AS old_rows` on AFTER
   triggers to capture the full changed row-set within the transaction.

---

## 2. Incremental Graph Views

A Cypher MATCH query can be registered as a pg-trickle stream table:

```sql
SELECT pg_eddy.create_graph_view(
    name     => 'friends_of_alice',
    cypher   => 'MATCH (a:Person {name: $name})-[:KNOWS]->(b:Person)
                 RETURN b.name AS friend, b.age AS age',
    params   => '{"name": "Alice"}',
    schedule => '1s'
);
```

### Constraint Graph Views (IMMEDIATE mode)

Graph integrity constraints can be expressed as Cypher MATCH patterns using
`IMMEDIATE` refresh mode:

```sql
SELECT pg_eddy.create_graph_view(
    name         => 'persons_without_email',
    cypher       => 'MATCH (p:Person) WHERE p.email IS NULL RETURN p',
    refresh_mode => 'IMMEDIATE'
);
-- Any row in this view is a constraint violation, caught in-transaction.
```

### SQL API

```sql
pg_eddy.create_graph_view(
    name         TEXT,
    cypher       TEXT,
    params       JSONB  DEFAULT '{}',
    schedule     TEXT   DEFAULT '1s',
    refresh_mode TEXT   DEFAULT 'AUTO',
    decode       BOOL   DEFAULT FALSE
) RETURNS VOID

pg_eddy.drop_graph_view(name TEXT) RETURNS VOID
pg_eddy.list_graph_views() RETURNS TABLE(name TEXT, cypher TEXT, schedule TEXT, ...)
pg_eddy.refresh_graph_view(name TEXT) RETURNS VOID
```

---

## 3. Deliverables

### IVM — Graph Views

- [ ] `pg_eddy.create_graph_view()`: Cypher MATCH → SQL → pg-trickle stream
      table
- [ ] `pg_eddy.drop_graph_view()`, `pg_eddy.list_graph_views()`,
      `pg_eddy.refresh_graph_view()`
- [ ] `_pg_eddy.graph_views` catalog
- [ ] DIFFERENTIAL refresh: changed nodes/edges only
- [ ] IMMEDIATE refresh: view updated within the same transaction as the write
- [ ] Constraint views (IMMEDIATE mode): any row = violation caught
      in-transaction
- [ ] DAG-aware scheduling: dependent graph views refreshed in topological order
- [ ] 72-hour soak test: no drift after sustained concurrent writes + reads

### Integration Verification

When implementing IVM, verify that:
1. A pg-trickle stream table can be defined over a SQL SELECT on
   `pg_eddy.nodes` and `pg_eddy.edges`
2. A Cypher `CREATE (n:Person {name:'Alice'})` (executed via SPI INSERT) causes
   the stream table to update on the next pg-trickle tick
3. `pg_eddy.delete_node(id)` causes the OLD row data in the pg-trickle change
   buffer to be correctly populated (requires REPLICA IDENTITY and working slot
   callbacks)
4. DIFFERENTIAL mode: only the changed rows are processed per tick
5. IMMEDIATE mode: stream table updates within the same transaction as the
   write, using transition tables
6. Confirm that setting `pg_trickle.cdc_mode = 'wal'` on a pg_eddy source table
   captures **zero** changes (expected — documenting the pg-eddy/pgoutput
   incompatibility)
7. Confirm that `create_graph_view()` correctly forces `cdc_mode = 'trigger'`

### Bulk Import CDC Contract

`pg_eddy.load_csv_nodes()` and `pg_eddy.load_csv_edges()` use SPI by default
(trigger-based CDC works automatically). A `fast := TRUE` option bypasses SPI
for ~3× import throughput; when `fast := TRUE` is used:
- Trigger-based CDC is **not fired** — pg-trickle stream tables will not
  update until a manual `pg_eddy.refresh_graph_view()` call
- The function emits a `WARNING` if pg-trickle is installed and any graph
  views exist

---

## 4. WAL CDC via Custom Output Plugin (Future)

> This is a **performance optimization** on top of working trigger-based IVM.
> Do not start until the trigger-based IVM path is proven stable.

**Architecture**:

```
  pg_eddy custom AM writes
    ↓ WAL (custom RMGR records)
  pg_eddy output plugin (wal_decode.rs) ← registered via
    _PG_output_plugin_init()
    ↓ decodes RMGR records → binary event frames
  Replication slot ('pg_eddy_cdc_slot')
    ↓ pg_logical_slot_peek_binary_changes(plugin := 'pg_eddy')
  pg-trickle wal_decoder bgworker
    ↓ buffers events per xid
    ↓ on COMMIT → writes to pgtrickle_changes.changes_<oid>
  Existing DVM engine (no changes needed)
    ↓ processes change buffer normally
  Slot advanced after successful apply
```

**Key design decisions**:

1. Binary format from day 1: compact binary event frames, not JSON
2. Apply directly into change buffer tables (same format as trigger CDC)
3. IMMEDIATE stays trigger-based (WAL CDC is inherently async)
4. Event filtering: only logical mutation events (not ADJ_UPDATE, VACUUM)
5. Backpressure: peek → apply → advance (at-least-once delivery)
6. One replication slot per database, shared across all pg_eddy tables

**pg-trickle changes required**:
- New `cdc_mode = 'pg_eddy_wal'` in `pgt_dependencies`
- ~200–400 lines in `src/wal_decoder.rs` and `src/cdc.rs`

**Expected improvement**: ~4–10× reduction in write-side CDC overhead for
DIFFERENTIAL/FULL mode graph views.

**Spike plan** (3 milestones):
1. Slot + plugin wiring
2. bgworker consumer
3. Benchmark harness (trigger CDC vs WAL CDC on 10K/100K/1M edge inserts)

---

## 5. Binary Event Frame Format

Specified in the main implementation plan (§5.5) and preserved here for
reference. The WAL record format is designed to be compatible with future
logical decoding from day 1.

```
BEGIN(xid: u32)
  NodeInserted { node_id: u64, label_ids: [u32], properties: bytes }
  NodeUpdated  { node_id: u64, changed_properties: bytes }
  NodeDeleted  { node_id: u64 }
  EdgeInserted { rel_id: u64, rel_type_id: u32, src: u64, tgt: u64, properties: bytes }
  EdgeDeleted  { rel_id: u64 }
COMMIT(xid: u32, commit_lsn: u64)
```

CDC event filtering: only the five logical mutation events above are emitted.
Internal storage operations (ADJ_UPDATE, ADJ_CHAIN_REBUILD, VACUUM_RECLAIM)
are **not** emitted — they are physical storage maintenance, not logical data
changes.
