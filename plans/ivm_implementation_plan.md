# pg_eddy — CDC / IVM Implementation Plan

> **Status**: Trigger-based implementation and fast Phase 4 gates are complete;
> the 72-hour soak remains deferred. Optional Phase 5 semantic WAL CDC is
> implemented and validated.
> Companion plan for the pg-trickle side lives at
> `MetroTraffic/pg-trickle/plans/pg_eddy_wal_cdc_plan.md`.
>
> **Architecture correction**: the custom AM tables have zero SQL columns, so
> PostgreSQL row triggers and tuple-slot deconstruction cannot expose logical
> graph entities directly. The implemented CDC boundary is instead
> `_pg_eddy.ivm_nodes` / `_pg_eddy.ivm_edges`: typed heap mirrors maintained in
> the same transaction as every authoritative custom-AM mutation.

## Phase 0 — Prerequisite verification

The original custom-AM trigger gate was superseded by typed mirrors. The
equivalent delivered gates are:

- [x] Every SQL helper and Cypher mutation route writes typed mirrors.
- [x] Mirror writes share the graph mutation transaction and roll back together.
- [x] Typed mirrors have primary keys and `REPLICA IDENTITY DEFAULT`.
- [x] pg-trickle trigger CDC captures complete OLD node values on DELETE.
- [x] `rebuild_ivm_sources()` restores typed mirrors from authoritative storage.

**Deliverable**: TAP tests; slot-callback / replica-identity fixes only if gaps
are found.

## Phase 1 — Catalog + SQL API skeleton

- [x] New module `src/ivm/` (`mod.rs`, `catalog.rs`, `api.rs`, `mirror.rs`,
  `pg_trickle.rs`).
- [x] `_pg_eddy.graph_views` catalog table:
  `(view_name, cypher_text, params jsonb, schedule, refresh_mode, stream_table_oid, created_at)`.
  Schema change → schema version bump + upgrade migration + full-DDL file.
- [x] `#[pg_extern]` APIs: `create_graph_view()`, `drop_graph_view()`,
  `list_graph_views()`, `refresh_graph_view()` with signatures per `ivm_plan.md` §2.
- [x] Graceful error when pg-trickle is not installed or incompatible.
- [x] Exact MetroTraffic fork/revision verifier and runtime version/API checks.
- [x] Keep pure logic (validation, Cypher-to-SQL invocation) separated from SPI for
  unit-testability.

## Phase 2 — `create_graph_view()` end-to-end (trigger CDC)

- [x] Compile deterministic fixed-length Cypher `MATCH ... RETURN` into a
  SELECT over `pg_eddy.nodes`/`pg_eddy.edges`, with `$param` substitution from
  the `params` jsonb.
- [x] Call pg-trickle's stream-table creation API with that SELECT, **explicitly
  forcing `cdc_mode = 'trigger'`** (permanent constraint — `pgoutput` cannot
  decode `XLOG_PG_EDDY_*` records).
- [x] Wire `drop/list/refresh_graph_view()` to pg-trickle APIs and catalog maintenance.
- Integration verification checklist (from `ivm_plan.md` §3):
    1. [x] Stream tables over typed node/edge mirrors work.
    2. [x] Cypher `CREATE` updates graph views.
    3. [x] `delete_node()` captures complete OLD values in the change buffer.
    4. [x] The obsolete custom-AM WAL-negative test is removed: pg-trickle sees
      ordinary heap mirrors, but pg_eddy still forces trigger mode so behavior
      cannot change with `wal_level` or pg-trickle global configuration.
    5. [x] `create_graph_view()` provably forces and verifies trigger mode.

## Phase 3 — Refresh modes + scheduling

- [x] **DIFFERENTIAL**: verify frontier-bounded processing consumes only changed rows
  (pg-trickle engine behavior; pg_eddy's job is correct change-buffer population).
- [x] **IMMEDIATE**: in-transaction refresh; deferred constraint views
  (any row = violation, raised in-transaction).
- [x] **DAG-aware scheduling**: stream views over internal graph streams refresh in topological order
  (delegate to pg-trickle dependency tracking; verify it sees pg_eddy views).
- [x] **Bulk import contract**: currently not applicable. pg_eddy exposes no
  `load_csv_nodes/edges(..., fast := TRUE)` bypass API; all existing mutation
  routes maintain mirrors. Add the warning contract when such an API is added.

## Phase 4 — Hardening & release gates

- [ ] 72-hour soak test: harness delivered at `tests/soak/ivm_drift.pl`; concurrent
  smoke run passes, but the full 259,200-second gate is deferred for background
  execution and must complete before release.
- [x] Benchmark run (release build) quantifying write-path regression with triggers
  attached (expected 20–55 µs/row trigger overhead).
- [x] Docs: SQL reference, compiler boundary, dependency pin, CDC mode,
  constraints, benchmark, and soak commands.
- [x] Full post-change release gates: clippy with warnings denied; 96/96 pgrx
  tests; 64/64 TAP assertions; 3880/3880 TCK scenarios; LDBC IS-1 at 1.14x
  AGE and IS-3 at 3.50x faster than AGE.

## Phase 5 — WAL CDC via custom output plugin (optional)

- [x] Transactional semantic logical messages at every authoritative mutation
  boundary, with complete OLD/NEW node and edge rows plus graph reset.
- [x] Strict, versioned binary protocol and publication-free `pg_eddy` output
  plugin; ordinary heap changes and physical RMGR maintenance records are ignored.
- [x] One database-scoped replication slot shared by both typed mirrors.
- [x] pg-trickle `pg_eddy_wal` mode, source validation, strict transaction
  grouping, typed D+I buffer application, and durable peek/apply/advance cursor.
- [x] Trigger-covered handoff, restart replay protection, manual/scheduler pumps,
  monitoring, missing-slot/error fallback, operator retry, and shared-slot cleanup.
- [x] `create_graph_view(..., decode => true)` for deferred views. IMMEDIATE and
  constraint views remain trigger-based because WAL CDC is asynchronous.
- [x] End-to-end TAP gate (30 assertions) and write benchmark. At 2,000 rows,
  semantic WAL improved total write latency 1.40x versus trigger CDC and reduced
  incremental CDC overhead below measurement noise.

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
