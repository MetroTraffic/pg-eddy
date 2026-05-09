# AGENTS.md — Guidance for AI coding agents working on pg-eddy

## Project Overview

pg_eddy is a PostgreSQL 18 extension written in Rust using pgrx 0.18 that
implements a custom Table Access Method (AM) for a high-performance native
Labelled Property Graph (LPG) store.

See [plans/implementation_plan.md](plans/implementation_plan.md) for the
complete design and implementation roadmap.

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

## Testing

The project includes three test suites:

- **Unit tests** — `cargo pgrx test pg18` for in-extension Rust tests
- **TAP tests** — `prove tests/tap/*.pl` for crash recovery and edge cases
- **OpenCypher TCK** — `perl tests/tck/run_tck.pl` for query engine conformance

**Important**: Whenever the TCK pass rate changes, update the badge in `README.md`
line 6: `[![OpenCypher TCK](https://img.shields.io/badge/OpenCypher%20TCK-NN%2FNNNN%20passed-orange.svg)](tests/tck/)`
Replace `NN/NNNN` with the new count (e.g., `82/3881 passed`).

## Release Checklist

Before releasing a new version:

1. **Update deliverables** — Update `Cargo.toml` version, create migration SQL
   files if needed (`pg_eddy--X.Y.Z--X.Y.W.sql`), and update `pg_eddy.control`.
2. **Update CHANGELOG.md** — Document all changes, fixes, and features for the
   release.
3. **Run tests** — Execute `cargo pgrx test pg18` to ensure all tests pass.
4. **Run clippy** — Execute `cargo clippy --features pg18` to check for linting
   issues.
5. **Update TCK badge** — If TCK pass rate changed, update `README.md` badge as
   noted in the Testing section above.
6. **Create git tag** — Tag the release with `git tag vX.Y.Z` and push to
   repository.
