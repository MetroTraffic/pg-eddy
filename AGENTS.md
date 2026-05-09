# AGENTS.md — Guidance for AI coding agents working on pg-eddy

## Project Overview

pg_eddy is a PostgreSQL 18 extension written in Rust using pgrx 0.18 that
implements a custom Table Access Method (AM) for a high-performance native
Labelled Property Graph (LPG) store.

See [plans/implementation_plan.md](plans/implementation_plan.md) for the
complete design and phased roadmap.

## Current Phase

**Phase 0 — AM skeleton (v0.1.0)**

The extension loads, the WAL resource manager is registered, and both AM
objects (`pg_eddy_node`, `pg_eddy_edge`) are created. All AM callbacks are
stubs (scan returns empty; all mutations error "not implemented").

## Project Layout

```
pg_eddy/          — PostgreSQL extension crate (cdylib)
  src/
    lib.rs         — Extension entry point, _PG_init, health_check
    error.rs       — PgEddyError enum with PE### error codes
    storage/
      mod.rs
      am.rs        — TableAmRoutine handler stubs
      wal.rs       — Custom WAL resource manager skeleton
  sql/
    pg_eddy--0.1.0.sql  — Extension DDL (schemas, AMs, tables)
  pg_eddy.control

pg_eddy_http/     — Placeholder HTTP/Bolt API binary (future)
plans/            — Design documents and implementation plan
.github/          — CI workflows
justfile          — Developer task runner
```

## Key Constraints

- **PostgreSQL 18 only** — pgrx feature flag `pg18`.
- **shared_preload_libraries = 'pg_eddy'** is required (AM + WAL RMGR must
  be registered at postmaster start via `_PG_init`).
- **Rust Edition 2024** — use `unsafe extern "C-unwind"` for all callbacks,
  `#[unsafe(no_mangle)]` instead of `#[no_mangle]`.
- **Safe Rust first** — `unsafe` only at FFI boundaries.
- **Custom WAL RMGR ID 128** — development only; reserve a permanent ID
  before any production release.

## Development Commands

```bash
cargo build --features pg18        # build
cargo pgrx test pg18               # run pgrx unit tests
cargo clippy --features pg18       # lint
cargo pgrx run pg18                # start psql session
```

Or use `just` (see `justfile`).

## Exit Criteria for Phase 0

1. `CREATE EXTENSION pg_eddy` succeeds (with `shared_preload_libraries`).
2. `SELECT * FROM pg_eddy.nodes` returns empty without panicking.
3. `SELECT pg_eddy.health_check()` returns `'pg_eddy OK'`.
4. WAL resource manager appears in `pg_stat_wal`.
