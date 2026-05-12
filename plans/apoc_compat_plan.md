# APOC Compatibility Plan

## Overview

This document describes a potential `pg_eddy_apoc` companion extension that
provides a compatibility shim for the most commonly-used APOC procedures,
enabling teams migrating from Neo4j to pg-eddy to reduce the surface area of
changes required in their application code.

This is explicitly **not** a full APOC reimplementation. APOC has ~500+
procedures; the goal is to cover the ~20–30 that appear most frequently in
real-world Neo4j applications and that have no direct openCypher equivalent.

**Prerequisites before starting this work:**
- pg-eddy v1.0 with full (or near-full) openCypher TCK conformance
- `CALL` procedure support in the Cypher query engine (see §4)
- At least one production user reporting APOC as a migration blocker

---

## 1. What APOC Is (and Isn't)

APOC (Awesome Procedures on Cypher) is a Neo4j plugin that exposes hundreds of
utility procedures callable via `CALL apoc.something(args) YIELD col`. It is
**not** part of the openCypher standard.

Categories:

| Category | Examples | Notes |
|---|---|---|
| Graph algorithms | `apoc.path.shortestPath`, `apoc.path.spanningTree` | High value for migration |
| Path expansion | `apoc.path.subgraphAll`, `apoc.path.expandConfig` | High value |
| Data conversion | `apoc.convert.toJson`, `apoc.convert.fromJsonMap` | Low value — SQL already does this |
| Data import/export | `apoc.load.json`, `apoc.export.csv` | Low value — use `COPY`, FDW |
| Schema/meta | `apoc.meta.graph`, `apoc.schema.assert` | Medium value |
| Dynamic Cypher | `apoc.cypher.run`, `apoc.do.when` | Very hard; deferred |
| String/collection utils | `apoc.text.join`, `apoc.coll.union` | Low value — SQL already does this |
| Graph refactoring | `apoc.refactor.mergeNodes` | Medium value |
| Triggers | `apoc.trigger.add` | Very hard; deferred |

---

## 2. Architecture

### 2.1 CALL procedure dispatch

pg-eddy's Cypher engine must support the `CALL` clause to invoke procedures:

```cypher
CALL apoc.path.shortestPath(startNode, endNode, {relationshipFilter: 'KNOWS'})
YIELD path
RETURN path
```

The `CALL` clause is part of the openCypher grammar (and the TCK tests it via
`Call*.feature`). When the procedure name is namespaced under `apoc.*`, the
engine dispatches to the pg_eddy_apoc extension's registered procedure table.

Procedure registration happens at `_PG_init` time via a registration API
exported by pg-eddy core:

```rust
// pg-eddy core exports:
pub fn register_procedure(name: &str, handler: ProcedureHandler);
```

`pg_eddy_apoc` calls this at init time for each implemented procedure.

### 2.2 Procedure handler interface

```rust
pub type ProcedureHandler = fn(
    args: &[CypherValue],
    graph_ctx: &GraphContext,
) -> Result<Box<dyn Iterator<Item = ProcedureRow>>, PgEddyError>;
```

Procedures return a streaming iterator of rows, each row being a map of
`yield_name -> CypherValue`. This matches the `YIELD` clause semantics.

### 2.3 Extension structure

```
pg_eddy_apoc/       — separate crate, depends on pg_eddy core
  Cargo.toml
  pg_eddy_apoc.control
  src/
    lib.rs          — _PG_init, procedure registration
    path.rs         — apoc.path.*
    algo.rs         — apoc.algo.*  (pagerank, betweenness, etc.)
    refactor.rs     — apoc.refactor.*
    meta.rs         — apoc.meta.*
  sql/
    pg_eddy_apoc--1.0.0.sql
```

---

## 3. Target Procedure List (Phase 1)

These are the procedures most commonly cited in Neo4j migration guides and
community surveys. Ordered by implementation priority.

### 3.1 Path finding (`apoc.path`, `apoc.algo`)

| Procedure | Description | Underlying algorithm |
|---|---|---|
| `apoc.path.shortestPath(start, end, config)` | Single shortest path | Dijkstra / BFS |
| `apoc.path.allSimplePaths(start, end, config)` | All simple paths up to depth limit | DFS with visited set |
| `apoc.path.spanningTree(start, config)` | BFS spanning tree from a node | BFS |
| `apoc.path.subgraphAll(start, config)` | All nodes/rels reachable from start | BFS/DFS |
| `apoc.algo.dijkstra(start, end, relType, costProp)` | Weighted shortest path | Dijkstra |
| `apoc.algo.astar(start, end, relType, costProp, latProp, lonProp)` | A* spatial shortest path | A* |

`config` is a map that can include:
- `relationshipFilter` — rel type / direction filter string (APOC DSL)
- `labelFilter` — node label filter string
- `maxLevel` / `minLevel` — depth bounds
- `limit` — max results

### 3.2 Graph algorithms (`apoc.algo`)

| Procedure | Description |
|---|---|
| `apoc.algo.pageRankStats(config)` | PageRank; yields `node, score` |
| `apoc.algo.betweenness(nodes, rels, directed)` | Betweenness centrality |
| `apoc.algo.closeness(nodes, rels, weight)` | Closeness centrality |
| `apoc.algo.cover(nodes)` | Minimum weight spanning cover |

These are CPU-intensive; initial implementation can use pure Rust graph
algorithm crates (e.g. `petgraph`) over a snapshot of the relevant subgraph
extracted from the AM.

### 3.3 Graph refactoring (`apoc.refactor`)

| Procedure | Description |
|---|---|
| `apoc.refactor.mergeNodes(nodes, config)` | Merge a list of nodes into one |
| `apoc.refactor.mergeRelationships(rels, config)` | Merge parallel relationships |
| `apoc.refactor.setType(rel, newType)` | Change a relationship's type |
| `apoc.refactor.rename.label(old, new)` | Rename a label across all nodes |
| `apoc.refactor.rename.type(old, new)` | Rename a rel type across all rels |

### 3.4 Meta / schema (`apoc.meta`)

| Procedure | Description |
|---|---|
| `apoc.meta.graph()` | Returns the schema graph (labels, rel types, property keys) |
| `apoc.meta.stats()` | Node/rel counts per label/type |
| `apoc.meta.nodeTypeProperties()` | Property key/type inventory per node label |
| `apoc.meta.relTypeProperties()` | Property key/type inventory per rel type |

These can be implemented as queries against pg-eddy's catalog tables.

---

## 4. Prerequisite: CALL Clause in Core

The Cypher engine must implement `CALL` before any APOC procedure is usable.
The openCypher TCK `Call*.feature` files specify the required semantics.

Minimum CALL semantics required:
- `CALL proc(args)` — void call
- `CALL proc(args) YIELD col1, col2` — with result columns
- `CALL proc(args) YIELD col1 WHERE predicate` — with filtering
- `CALL` as a leading clause and as a mid-pipeline clause

This is tracked in the main implementation plan under the CALL clause milestone.

---

## 5. What Is Explicitly Out of Scope

These APOC categories are deferred indefinitely:

- **Dynamic Cypher** (`apoc.cypher.run`, `apoc.do.when`, `apoc.do.case`) —
  requires an embedded Cypher interpreter callable at runtime, not just at
  parse/plan time. Requires significant additional infrastructure.
- **Triggers** (`apoc.trigger.*`) — requires PostgreSQL event trigger
  integration and a persistent procedure registry. Complex; no clear timeline.
- **Data import/export** (`apoc.load.*`, `apoc.export.*`) — redundant with
  `COPY`, FDW, and `pg_dump`. Not worth implementing.
- **Utility functions** (`apoc.text.*`, `apoc.coll.*`, `apoc.map.*`,
  `apoc.date.*`) — redundant with SQL built-ins and openCypher standard
  functions. Document SQL equivalents in the migration guide instead.
- **Virtual graphs** (`apoc.graph.from*`) — no clear PostgreSQL mapping.

---

## 6. Migration Guide (accompanies the extension)

Rather than implementing the out-of-scope categories, the plan includes a
`docs/apoc_migration.md` reference that maps each major APOC procedure to its
pg-eddy or SQL equivalent:

```
apoc.text.join(list, delimiter)  →  array_to_string(list, delimiter)  [SQL]
apoc.convert.toJson(value)       →  value::text / to_json(value)      [SQL]
apoc.load.json(url)              →  pg_read_file() + jsonb_populate_* [SQL]
apoc.date.format(timestamp, ...) →  to_char(timestamp, format)        [SQL]
```

This is lower effort and higher leverage than implementing the procedures.

---

## 7. Phasing

### Phase A: Infrastructure (no user-visible APOC yet)
- Implement `CALL` clause in core Cypher engine (may already be required for TCK)
- Design and implement the procedure registration API in pg-eddy core
- Create `pg_eddy_apoc` crate skeleton with `_PG_init` registration loop

### Phase B: Meta procedures (quick wins)
- `apoc.meta.graph`, `apoc.meta.stats`, `apoc.meta.nodeTypeProperties`,
  `apoc.meta.relTypeProperties`
- These only query the catalog — no algorithm implementation needed
- Publish migration guide alongside

### Phase C: Path finding
- `apoc.path.shortestPath`, `apoc.path.spanningTree`, `apoc.path.subgraphAll`
- Uses the AM's adjacency-follow traversal primitives directly
- `apoc.algo.dijkstra` (weighted variant)

### Phase D: Graph algorithms
- PageRank, betweenness, closeness using `petgraph` over subgraph snapshots
- `apoc.path.allSimplePaths`

### Phase E: Refactoring
- `apoc.refactor.mergeNodes`, `apoc.refactor.rename.*`
- These are write operations; need careful MVCC/WAL handling

---

## 8. Open Questions

1. **Versioning**: should `pg_eddy_apoc` version independently from `pg_eddy`
   core, or track it? Independent versioning is cleaner but adds release
   overhead.

2. **Procedure registration API stability**: the handler interface above needs
   to be considered a stable API if third parties could register their own
   procedures. Consider whether to expose it publicly.

3. **Config map parsing**: APOC's `config` map parameter is an informal DSL
   (e.g. `relationshipFilter: 'KNOWS|LIKES>'`). Parsing this correctly for all
   cases is non-trivial. A strict subset of the DSL may be all that's needed.

4. **petgraph dependency**: loading a subgraph into petgraph for centrality
   algorithms means holding a full copy of the relevant subgraph in memory.
   For large graphs this may be prohibitive. An AM-integrated streaming
   algorithm would be better but is much harder to implement.
