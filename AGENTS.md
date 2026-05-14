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
just lint                          # lint (uses -D warnings; bare clippy misses warnings)
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

## Performance Benchmarks

**Any commit that touches the storage layer, executor, or catalog must be
benchmarked to confirm it does not introduce a performance regression.**

Run the benchmark against a **release build** only — debug builds are 30–60×
slower and produce meaningless numbers:

```bash
# 1. Install release build
cargo pgrx install --release --features pg18

# 2. Clear any leftover temp cluster from a previous run
rm -rf tmp_check/t_run_ldbc_benchmark_ldbc_bench_data

# 3. Run benchmark (requires AGE extension installed)
PG_REGRESS='/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress' \
PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl" \
PATH="/usr/lib/postgresql/18/bin:${PATH}" \
perl benchmarks/run_ldbc_benchmark.pl
```

**Pass gates** (both must hold):
- IS-1 (node lookup with property index): pg_eddy **≤ 2× AGE latency**
- IS-3 (1-hop expand): pg_eddy **≥ 2× faster than AGE**

**Baseline** (as of v0.27.0, 2026-05-14, 1 000 nodes / 5 000 edges):
- IS-1: 13.89 ms (pg_eddy) vs 13.31 ms (AGE) → **1.04× — PASS**
- IS-3: 14.23 ms (pg_eddy) vs 180.30 ms (AGE) → **12.67× faster — PASS**

If either gate fails, **do not tag the release** until the regression is
diagnosed and fixed. Record the new numbers in `benchmarks/README.md`.

## Versioning Policy

There are **two independent version axes**:

### Schema version (`Cargo.toml` + `pg_eddy.control`)

`pg_eddy.control` uses `default_version = '@CARGO_VERSION@'` — a pgrx-supported
placeholder that is substituted with the Cargo.toml version at build/install time.
**Never hardcode the version in the control file.** This means the two files can
never drift out of sync, and `pg_eddy.control` never needs editing on a version bump.

**Until the extension is distributed to external users, do not bump the schema
version or add migration files for pure Rust-only changes.** Only increment the
schema version (and add the corresponding `pg_eddy--OLD--NEW.sql` migration file
and a new `pg_eddy--NEW.sql` full-DDL file) when there is an actual catalog/schema
change: new tables, sequences, indexes, or SQL function signature changes. Rust
implementation changes that don't touch the catalog require no schema version bump,
no new SQL files, and no control file edit.

Once the extension is being distributed and users will run
`ALTER EXTENSION pg_eddy UPDATE` on live databases, every release — even
Rust-only ones — should get a schema version bump with an empty (but present)
migration file so PostgreSQL's upgrade path stays linear.

### Software release version (git tags + CHANGELOG)

Git tags (`v0.8.0`, etc.) and CHANGELOG entries are **independent** of the schema
version. You can tag `v0.9.0` in git and add a CHANGELOG entry for it while
`Cargo.toml` and `pg_eddy.control` remain at `0.8.0` (the last schema-changing
release). The two axes only coincide when a release includes a catalog change.

**Current state (as of the policy change):** `Cargo.toml` and `pg_eddy.control`
are both at `0.8.0` — but that is not because `0.8.0` was a schema release. They
were historically bumped in lockstep with every git release tag (the old
behaviour), producing empty migration files for `0.5.1→0.6.0`, `0.6.0→0.7.0`,
and `0.7.0→0.8.0`. The last real catalog change was `0.5.1`. Going forward, the
schema version stays frozen at `0.8.0` until there is a genuine catalog change,
regardless of how many git release tags are created.

## Release Checklist

Before releasing a new version:

1. **Check deliverables against plan** — ⚠️ **REQUIRED** — Read the
   `plans/implementation_plan.md` checklist for the version being released.
   Verify every `[ ]` item is actually implemented before committing. Mark
   each done item `[x]` in the plan. Note any deferred items with a reason.
2. **Update schema version files** — Only if there are catalog/schema changes (see
   Versioning Policy above): bump `Cargo.toml` version, update `pg_eddy.control`
   `default_version` to match, add a migration SQL file (`pg_eddy--X.Y.Z--X.Y.W.sql`),
   and add a new full-DDL file (`pg_eddy--X.Y.W.sql`). Skip entirely for Rust-only
   releases — git tag and CHANGELOG entries are independent of the schema version.
3. **Update CHANGELOG.md** — Document all changes, fixes, and features for the
   release.
4. **Run unit tests** — ⚠️ **REQUIRED GATE** — Execute `cargo pgrx test pg18`
   and ensure all tests pass (all unit tests in `pg_eddy/src/` must pass).
   Do not proceed to tagging if any tests fail.
5. **Run clippy** — ⚠️ **REQUIRED GATE** — Execute `just lint` (which runs
   `cargo clippy --features pg18 -- -D warnings`) and ensure there are zero warnings.
   Do not use bare `cargo clippy --features pg18` — it does not treat warnings as errors.
   Do not proceed to tagging if clippy fails.
6. **Run performance benchmark** — ⚠️ **REQUIRED GATE for any commit touching storage,
   executor, or catalog** — Install a release build (`cargo pgrx install --release
   --features pg18`), clear the temp dir, and run `perl benchmarks/run_ldbc_benchmark.pl`
   (with the env vars from the Performance Benchmarks section above). Both IS-1 (≤2×
   AGE latency) and IS-3 (≥2× faster than AGE) gates must pass. Record the new numbers
   in `benchmarks/README.md`. Do not tag if either gate fails.
7. **Run TAP tests** — Execute `prove tests/tap/*.pl` to verify crash recovery
   and edge cases work correctly.
8. **Run TCK tests** — Execute `perl tests/tck/run_tck.pl` and update the badge
   in `README.md` line 6 if the pass rate changes: 
   `[![OpenCypher TCK](https://img.shields.io/badge/OpenCypher%20TCK-NN%2FNNNN%20passed-orange.svg)](tests/tck/)`
   Replace `NN/NNNN` with the new count (e.g., `82/3881 passed`).
9. **Create git tag** — Tag the release with `git tag vX.Y.Z` and push to
   repository. (Only after all gates above pass.)
