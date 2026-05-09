# Changelog

What's new in pg_eddy — written for everyone, not just developers.

For future plans and upcoming features, see [plans/implementation_plan.md](plans/implementation_plan.md).

## Table of Contents

- [0.2.0](#020--2026-05-09--node-storage) — Node Storage
- [0.1.0](#010--2026-05-09--am-skeleton) — AM Skeleton

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
