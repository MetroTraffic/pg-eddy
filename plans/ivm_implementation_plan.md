# pg_eddy — CDC / IVM Implementation Plan

> **Status**: Active planning document, derived from `plans/ivm_plan.md`.
> Companion plan for the pg-trickle side lives at
> `MetroTraffic/pg-trickle/plans/pg_eddy_wal_cdc_plan.md`.
>
> **Blocked on prerequisites** (from `ivm_plan.md`):
> - Cypher `CREATE`, `SET`, `DELETE` working via SPI (v0.12.0+)
> - REPLICA IDENTITY support on custom AM tables (slot callbacks with column data)
> - TCK ≥80% (write language functional)

## Phase 0 — Prerequisite verification

Land a gate-check test suite proving the CDC substrate works before any IVM code:

- **Trigger-firing test**: attach a plain `AFTER INSERT/UPDATE/DELETE FOR EACH ROW`
  trigger to a pg_eddy AM table; assert it fires for every Cypher write clause
  (proves all writes route through SPI → executor → `ExecAR*Triggers()`).
- **Slot deconstruction test**: trigger function reads all OLD/NEW columns via
  `slot_getallattrs`; assert values are complete and correctly typed.
- **REPLICA IDENTITY**: declare `node_id`/`rel_id` primary keys at the AM level
  (or `REPLICA IDENTITY FULL`); assert OLD tuples are populated on UPDATE/DELETE.
- **Transition tables**: `REFERENCING NEW TABLE / OLD TABLE` on statement-level
  AFTER triggers works against the custom AM.

**Deliverable**: TAP tests; slot-callback / replica-identity fixes only if gaps
are found.

## Phase 1 — Catalog + SQL API skeleton

- New module `src/ivm/` (`mod.rs`, `catalog.rs`, `api.rs`).
- `_pg_eddy.graph_views` catalog table:
  `(view_name, cypher_text, params jsonb, schedule, refresh_mode, stream_table_oid, created_at)`.
  Schema change → schema version bump + upgrade migration + full-DDL file.
- `#[pg_extern]` stubs: `create_graph_view()`, `drop_graph_view()`,
  `list_graph_views()`, `refresh_graph_view()` with signatures per `ivm_plan.md` §2.
- Graceful error when pg-trickle is not installed (detect via `pg_extension`).
- Keep pure logic (validation, Cypher→SQL invocation) separated from SPI for
  unit-testability.

## Phase 2 — `create_graph_view()` end-to-end (trigger CDC)

- Compile Cypher `MATCH ... RETURN` through the existing SQL generator into a
  SELECT over `pg_eddy.nodes`/`pg_eddy.edges`, with `$param` substitution from
  the `params` jsonb.
- Call pg-trickle's stream-table creation API with that SELECT, **explicitly
  forcing `cdc_mode = 'trigger'`** (permanent constraint — `pgoutput` cannot
  decode `XLOG_PG_EDDY_*` records).
- Wire `drop/list/refresh_graph_view()` to pg-trickle APIs + catalog maintenance.
- Integration verification checklist (from `ivm_plan.md` §3):
  1. Stream table over `pg_eddy.nodes`/`edges` works.
  2. Cypher `CREATE` → stream table updates on next tick.
  3. `delete_node()` → OLD row correctly captured in change buffer.
  4. Negative test: `cdc_mode = 'wal'` captures **zero** changes (document the
     incompatibility).
  5. `create_graph_view()` provably forces trigger mode.

## Phase 3 — Refresh modes + scheduling

- **DIFFERENTIAL**: verify per-tick processing touches only changed rows
  (pg-trickle engine behavior; pg_eddy's job is correct change-buffer population).
- **IMMEDIATE**: transition-table-based in-transaction refresh; constraint views
  (any row = violation, raised in-transaction).
- **DAG-aware scheduling**: views over views refresh in topological order
  (delegate to pg-trickle dependency tracking; verify it sees pg_eddy views).
- Bulk import contract: `load_csv_nodes/edges(fast := TRUE)` bypasses triggers →
  emit `WARNING` when graph views exist; document manual
  `refresh_graph_view()` requirement.

## Phase 4 — Hardening & release gates

- 72-hour soak test: concurrent Cypher writes + view reads, zero drift vs. full
  recompute.
- Benchmark run (release build) quantifying write-path regression with triggers
  attached (expected 20–55 µs/row trigger overhead).
- Docs: SQL reference for the four functions, CDC-mode constraint, bulk-import
  caveat.

## Phase 5 — WAL CDC via custom output plugin (future, optional)

Only after Phases 2–4 are stable. pg_eddy side:

- `src/storage/wal_decode.rs`: logical decoding output plugin
  (`_PG_output_plugin_init`) decoding custom RMGR records into the binary event
  frame format (`ivm_plan.md` §5); emit only the five logical mutation events,
  filter ADJ_UPDATE / ADJ_CHAIN_REBUILD / VACUUM_RECLAIM.
- One `pg_eddy_cdc_slot` replication slot per database.
- IMMEDIATE mode stays trigger-based (WAL CDC is inherently async).

pg-trickle side is tracked in the companion plan
(`MetroTraffic/pg-trickle/plans/pg_eddy_wal_cdc_plan.md`).

**Spike milestones**: (1) slot + plugin wiring, (2) bgworker consumer,
(3) benchmark harness comparing trigger vs. WAL CDC at 10K/100K/1M edge inserts
(target 4–10× write-side improvement).

## PR sequencing

| # | Repo | Scope |
|---|------|-------|
| 1 | pg-eddy | Phase 0 gate-check TAP tests (+ any slot/RI fixes) |
| 2 | pg-eddy | Phase 1 catalog + API stubs (schema version bump) |
| 3 | pg-eddy | Phase 2 create/drop/list/refresh + integration tests |
| 4 | pg-eddy | Phase 3 IMMEDIATE + constraint views + bulk-import warning |
| 5 | pg-eddy | Phase 4 soak + benchmark + docs |
| 6 | pg-eddy | Phase 5a output plugin (spike) |
| 7 | pg-trickle | Phase 5b `pg_eddy_wal` cdc_mode + wal_decoder consumer |
