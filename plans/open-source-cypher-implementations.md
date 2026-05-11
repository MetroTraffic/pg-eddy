# Open-Source Cypher Implementations — Landscape Analysis

**Date**: 2026-05-11  
**Status**: reference  
**Purpose**: Survey open-source Cypher implementations for ideas pg_eddy can
safely adopt, with license considerations.

---

## 1. License Safety Summary

| Project | License | Safe to study? | Safe to copy code? | Notes |
|---------|---------|:-:|:-:|-------|
| **Apache AGE** | Apache 2.0 | Yes | Yes (with attribution) | Best source — permissive license, same PostgreSQL extension model |
| **KuzuDB** | MIT | Yes | Yes | Archived Oct 2025; code remains available and permissively licensed |
| **FalkorDB** | SSPL v1 | Study only | **No** | Server Side Public License — studying architecture is fine, but copying code or substantial structure could trigger copyleft |
| **Memgraph** | BSL 1.1 / MEL | Study only | **No** | Business Source License converts to Apache 2.0 after change date (2030); until then, production use restricted. Enterprise features under proprietary MEL |
| **Neo4j** | GPL v3 + commercial | **Avoid** | **No** | GPL contamination risk; even studying implementation details can create exposure if it influences design too closely |
| **rs-polygraph** | Apache 2.0 | Yes | Yes | Transpiler, not executor — limited direct applicability but safe to reference |
| **libcypher-parser** | Apache 2.0 | Yes | Yes | C library, standalone Cypher parser with no execution engine |
| **openCypher spec** | Apache 2.0 | Yes | N/A | The specification itself — grammar EBNF, TCK, semantics docs |

**Rule of thumb**: Apache 2.0 and MIT projects are safe to study AND adapt code
from (with attribution). SSPL, BSL, and GPL projects should be studied for
architectural ideas only — never copy code, data structures, or test cases
verbatim.

---

## 2. Apache AGE — The Primary Reference

### 2.1 Overview

Apache AGE (A Graph Extension) is a PostgreSQL extension that adds openCypher
support on top of standard PostgreSQL tables. It's the closest peer to pg_eddy
in terms of deployment model (PostgreSQL extension), language (C, targeting the
PG internals API), and goal (Cypher on PostgreSQL).

- **Language**: C
- **License**: Apache 2.0
- **PostgreSQL versions**: 11–18
- **Stars**: ~2k
- **Active development**: Yes (recent commits include MERGE ON CREATE/MATCH SET,
  VLE cache improvements, index scan support, list comprehension rewrite)

### 2.2 Architecture

```
Cypher query string
    │
    ▼
Parser (Bison/Flex: cypher_gram.y + ag_scanner.l)
    │
    ▼
Cypher AST (PostgreSQL Node types + custom ag_node types)
    │
    ▼
Analyzer/Transformer (cypher_analyze.c + cypher_clause.c + cypher_expr.c)
    │  Converts Cypher AST → PostgreSQL Query tree
    ▼
PostgreSQL Planner (standard PG planner)
    │
    ▼
PostgreSQL Executor (standard PG executor + custom executor nodes)
    │  cypher_create.c, cypher_merge.c, cypher_set.c, cypher_delete.c
    ▼
agtype (custom PostgreSQL type — JSONB-like, stores nodes/edges/paths)
    │
    ▼
Standard PostgreSQL heap tables (one table per vertex/edge label)
```

### 2.3 Key Design Decisions

**Parser: Bison/Flex (generated)**

AGE uses a Bison grammar (`cypher_gram.y`, ~2500 lines) and Flex scanner
(`ag_scanner.l`). This is the standard approach for PostgreSQL extensions — it
mirrors how PostgreSQL itself parses SQL. The grammar produces PostgreSQL-native
`Node` types (A_Expr, FuncCall, BoolExpr, etc.) mixed with custom `ag_node`
types (cypher_match, cypher_create, cypher_path, etc.).

*Relevance to pg_eddy*: pg_eddy uses a hand-rolled recursive-descent parser in
Rust. Both approaches are valid. The Bison grammar gives AGE a clear, auditable
grammar specification (you can read `cypher_gram.y` and see exactly what syntax
is accepted), while pg_eddy's hand-rolled parser gives better error messages and
full control. **No change recommended** — but AGE's grammar file is a useful
cross-reference when implementing new syntax to verify correct precedence and
associativity rules.

**Execution: Cypher → SQL Query Tree → PostgreSQL Executor**

This is AGE's defining architectural choice. Instead of building a custom
executor, AGE **transforms Cypher AST into PostgreSQL's internal Query tree**
(the same structure the SQL parser produces). PostgreSQL's standard planner and
executor then handle the actual query execution.

This means AGE gets for free:
- PostgreSQL's cost-based optimizer
- Index scans (recently added via `cypher_utils.c`)
- Parallel query execution
- MVCC and WAL
- EXPLAIN/ANALYZE support

But it also means:
- Every Cypher concept must be expressible as a PostgreSQL query plan
- Adjacency traversal goes through standard heap scans + joins (no native
  adjacency pointers)
- VLE (variable-length edges) requires a custom function (`age_build_vle_match_edge`)
  that implements DFS/BFS as a set-returning function
- Performance is bounded by PostgreSQL's join engine, not by native graph
  traversal

*Relevance to pg_eddy*: pg_eddy takes the opposite approach — custom Table AM
with native adjacency-list storage and a custom interpreter executor. This is
why pg_eddy is 4.27× faster than AGE on multi-hop traversals. **The lesson is
not to copy AGE's execution model**, but to study specific implementation
patterns:

**Storage: Standard heap tables with agtype column**

Each vertex/edge label gets its own table in a schema named after the graph.
Properties are stored as `agtype` (a custom JSONB-like binary type). The `id`
column uses a 64-bit graphid encoding label_id + entry_id.

*Relevance to pg_eddy*: pg_eddy's custom page format with inline adjacency
lists is fundamentally better for graph traversal. However, AGE's `agtype` type
design is worth studying — it handles heterogeneous property values (int, float,
string, bool, null, list, map, path, vertex, edge) in a single binary format
with efficient access. pg_eddy's JSONB-based property encoding could potentially
be optimized based on agtype's approach.

### 2.4 Specific Implementation Ideas from AGE

| Feature | AGE implementation | pg_eddy relevance |
|---------|-------------------|-------------------|
| **Chained comparisons** (`1 < x < 10`) | Grammar-level: `build_comparison_expression()` in `cypher_gram.y` detects chained A_Expr nodes and flattens them into AND chains | pg_eddy's parser could adopt this pattern — currently handles it, but AGE's approach is well-documented |
| **List comprehension** | Recently reimplemented (#2169) using `cypher_list_comprehension` node → ARRAY_SUBLINK → transformed to correlated subquery in `cypher_clause.c` | pg_eddy's executor handles this directly; AGE's subquery approach is less relevant but the NULL-guard pattern (`CASE WHEN list IS NULL THEN NULL ELSE ... END`) is worth verifying pg_eddy does the same |
| **Predicate functions** (`all/any/none/single`) | `build_predicate_function_node()` — all four use EXPR_SUBLINK with aggregate-based queries: `bool_or + CASE` for all/any/none, `count(*)` for single. NULL-list guard wraps each | pg_eddy should verify its null semantics match — the `CASE WHEN expr IS NULL THEN NULL` wrapper is a spec requirement |
| **VLE (variable-length edges)** | `build_VLE_relation()` in grammar, executed via `age_build_vle_match_edge()` set-returning function with configurable DFS/BFS/FROM_ALL algorithms. Recent VLE cache (#2376) added for performance | pg_eddy's BFS-based `exec_var_length_expand()` is simpler. AGE's **VLE cache** is worth studying — caching traversal results for repeated patterns could benefit pg_eddy |
| **MERGE ON CREATE/MATCH SET** | Recently added (#2347) — `cypher_merge.c` handles `ON CREATE SET` and `ON MATCH SET` as separate set-item lists applied conditionally | pg_eddy already supports MERGE; verifying ON CREATE/MATCH SET semantics match AGE's tested behavior is useful |
| **Index scan** | Recently added (#2351) — `cypher_utils.c` now pushes label + property filters into index scans | pg_eddy doesn't have property indexes yet (planned v0.15.0+). AGE's approach of pushing property predicates into scans is the model to follow |
| **Map projections** | `cypher_map_projection` node with PROPERTY_SELECTOR, VARIABLE_SELECTOR, LITERAL_ENTRY, ALL_PROPERTIES_SELECTOR variants | pg_eddy should ensure map projection support matches this — useful syntax for API consumers |
| **XOR implementation** | `make_xor_expr()`: `XOR(A,B) = (A OR B) AND NOT(A AND B)` | Trivial but worth verifying pg_eddy's XOR uses the same standard decomposition |
| **EXPLAIN support** | Full `EXPLAIN`/`EXPLAIN ANALYZE`/`EXPLAIN VERBOSE` for Cypher queries, built into grammar | pg_eddy could add `EXPLAIN` support for Cypher queries to expose the LogicalPlan — useful for debugging and optimization |

---

## 3. KuzuDB — The Performance Reference

### 3.1 Overview

Kuzu was an embedded graph database built for analytical workloads, developed at
University of Waterloo. **Archived October 2025** (the team is "working on
something new"), but the code remains under MIT license. It's the most
academically rigorous open-source Cypher implementation.

- **Language**: C++ (70%)
- **License**: MIT
- **Status**: Archived (v0.11.3 final release)
- **Stars**: ~3.9k

### 3.2 Architecture

```
Cypher query string
    │
    ▼
ANTLR4 parser (Cypher.g4 grammar)
    │
    ▼
AST
    │
    ▼
Binder (semantic analysis, type resolution, schema binding)
    │
    ▼
Bound Query Tree
    │
    ▼
Planner (cost-based, join order optimization, worst-case optimal joins)
    │
    ▼
Logical Plan (relational algebra operators)
    │
    ▼
Physical Plan (vectorized operators)
    │
    ▼
Vectorized Processor (columnar, SIMD, multi-core)
    │
    ▼
Columnar Storage (CSR adjacency lists, columnar properties)
```

### 3.3 Key Design Decisions

**Columnar storage with CSR adjacency indices**

Kuzu uses Compressed Sparse Row (CSR) format for adjacency lists — the same
format used in high-performance graph analytics (GraphBLAS, etc.). Properties
are stored column-oriented rather than row-oriented. This enables vectorized
processing of property filters.

*Relevance to pg_eddy*: pg_eddy uses row-oriented adjacency lists in custom
pages. CSR is better for analytics (scan many nodes, compute over properties)
while adjacency lists are better for traversal (follow specific edges). pg_eddy's
workload is traversal-first, so the current design is correct. However, if
analytics workloads become important, columnar property storage could be a
future optimization.

**Worst-case optimal join algorithms**

Kuzu implements worst-case optimal (WCO) join algorithms for multi-way join
queries (e.g., triangle detection, clique finding). These avoid the intermediate
blowup that pairwise joins produce on dense graphs.

*Relevance to pg_eddy*: This is a future optimization opportunity. Currently
pg_eddy processes MATCH patterns left-to-right as pairwise joins (CrossProduct
in the planner). For dense graph patterns (triangles, stars), WCO joins would
be dramatically faster. This is a post-v1.0 optimization.

**Vectorized query processing**

Kuzu processes data in vectors (batches of 2048 tuples) rather than one row at
a time. Each operator processes an entire vector, enabling CPU cache efficiency
and SIMD utilization.

*Relevance to pg_eddy*: pg_eddy uses row-at-a-time interpretation
(`Vec<Row>`). Vectorized processing would be a significant performance
improvement for aggregation-heavy queries. This is a major architectural change
(not a simple refactor) and should be considered for a v2.0 redesign rather
than incremental adoption.

**Cost-based planner with factorized processing**

Kuzu's planner uses cardinality estimation to choose join orders and can produce
"factorized" intermediate results (storing common prefixes once rather than
duplicating them across all matching suffixes).

*Relevance to pg_eddy*: pg_eddy currently has no cardinality estimation and
processes clauses left-to-right. Adding statistics-driven join reordering would
be the single biggest planner improvement. This requires:
1. Property indexes (to estimate selectivity)
2. Label cardinality tracking (how many nodes per label)
3. Edge type cardinality tracking (how many edges per type + direction)
4. A cost model that uses these statistics

### 3.4 Specific Ideas from Kuzu

| Feature | Kuzu approach | pg_eddy relevance |
|---------|-------------|-------------------|
| **ANTLR4 parser** | Uses ANTLR4 with a `.g4` grammar file; generates C++ parser | pg_eddy's hand-rolled parser is fine; but Kuzu's `.g4` grammar is another cross-reference for correct syntax |
| **Binder (semantic analysis)** | Separate pass between parsing and planning — resolves variable scopes, validates types, binds to schema | pg_eddy does some of this in the planner (`VarKind` tracking). Splitting semantic analysis into its own pass would improve error messages and catch more errors before execution |
| **Recursive CTE for VLE** | Variable-length paths compiled to recursive Common Table Expressions internally | Interesting alternative to pg_eddy's BFS — could enable the PG optimizer to contribute to VLE planning |
| **Lateral joins** | Used for correlated subqueries (EXISTS { }, CALL { }) | pg_eddy handles these as Apply nodes; Kuzu's lateral join approach may have better performance characteristics |
| **Multi-core parallelism** | Morsel-driven parallelism — work is divided into morsels (chunks of data), workers steal morsels from a shared queue | Future consideration for pg_eddy; within PostgreSQL, background workers could implement this pattern |

---

## 4. FalkorDB — The Performance-Focused Reference (Study Only)

### 4.1 Overview

FalkorDB (formerly RedisGraph) is a Redis module that implements a graph
database using sparse matrices and linear algebra for query execution.

- **Language**: C
- **License**: SSPL v1 (**study architecture only, do not copy code**)
- **Stars**: ~2.5k
- **Active**: Yes

### 4.2 Key Architectural Ideas

**Sparse matrix representation (GraphBLAS)**

FalkorDB represents the adjacency matrix as a sparse matrix and uses
[GraphBLAS](http://graphblas.org/) (a standard for graph algorithms via linear
algebra) for query execution. A MATCH pattern like `(a)-[:KNOWS]->(b)-[:WORKS_AT]->(c)`
becomes a series of sparse matrix multiplications.

*Relevance to pg_eddy*: This is a radically different execution model. For
pg_eddy's traversal-first workload, adjacency lists are faster. But for
analytics (PageRank, betweenness centrality, community detection), GraphBLAS
operations on sparse matrices would be dramatically faster than row-at-a-time
interpretation. This is relevant for a future `pg_eddy_analytics` extension,
not for the core query engine.

**PEG parser**

FalkorDB uses a PEG (Parsing Expression Grammar) parser generated from a `.peg`
file.

*Relevance to pg_eddy*: No direct benefit over pg_eddy's hand-rolled parser.

**Execution plan caching**

FalkorDB caches compiled execution plans for parameterized queries. If the same
query structure appears with different parameter values, the cached plan is
reused.

*Relevance to pg_eddy*: **High value**. pg_eddy currently re-parses and
re-plans every query. For application workloads that repeat the same query
shapes (e.g., `MATCH (n:Person {id: $id}) RETURN n`), plan caching would
eliminate redundant parsing and planning overhead. This is a straightforward
optimization:
1. Hash the query string (or a normalized form)
2. Store `LogicalPlan` in a session-local cache
3. On cache hit, skip parsing and planning; bind parameters and execute

---

## 5. Memgraph — The Production Reference (Study Only)

### 5.1 Overview

Memgraph is a high-performance in-memory graph database written in C++ with
full Cypher compatibility. It's the most production-mature open-source
alternative to Neo4j.

- **Language**: C++ (65%)
- **License**: BSL 1.1 (converts to Apache 2.0 in 2030) (**study only**)
- **Stars**: ~4k
- **Active**: Very active (3.9k stars, 60 contributors, daily commits)

### 5.2 Key Architectural Ideas

**In-memory storage with delta-based MVCC**

Memgraph stores the entire graph in memory with a delta-chain MVCC scheme for
transactions. Each modification creates a delta record; readers reconstruct
their snapshot by walking the delta chain.

*Relevance to pg_eddy*: pg_eddy uses PostgreSQL's built-in MVCC (heap tuples +
visibility map). Memgraph's approach is faster for pure graph operations but
doesn't integrate with PostgreSQL's transaction system. No change recommended —
pg_eddy's integration with PG MVCC is a feature, not a limitation.

**E-graph based planner (v2, in development)**

Memgraph is developing a new planner based on e-graphs (equality graphs) — a
technique from compiler optimization that represents multiple equivalent query
plans simultaneously and extracts the optimal one. Recent PRs include "feat:
Add e-graph rewrite system for planner v2" (#3818).

*Relevance to pg_eddy*: This is cutting-edge research. E-graph based planning
is more powerful than traditional rule-based or cost-based optimization because
it explores the full space of equivalent plans simultaneously. This is a
long-term research direction, not an immediate priority.

**Streaming ingestion (Kafka, Pulsar)**

Memgraph can ingest data from streaming sources and run graph algorithms that
react to changes in real time.

*Relevance to pg_eddy*: Not directly relevant (pg_eddy ingests via SQL/Cypher
DML), but the concept of trigger-based graph recomputation could be interesting
for materialized graph views in the future.

**Custom query modules (Python, Rust, C++)**

Memgraph allows users to write custom graph algorithms as loadable modules in
Python, Rust, or C++. These integrate with the query engine as user-defined
procedures callable via `CALL module.procedure()`.

*Relevance to pg_eddy*: pg_eddy could support this via PostgreSQL's existing
UDF mechanism — graph algorithms exposed as SQL functions that operate on the
graph AM directly. This is a natural extension of `CALL procedure()` support.

---

## 6. libcypher-parser — The Grammar Reference

### 6.1 Overview

[libcypher-parser](https://github.com/cleishm/libcypher-parser) is a standalone
C library that parses Cypher queries into an AST. It does not execute queries —
it only parses. Licensed Apache 2.0.

- **Language**: C
- **License**: Apache 2.0 (safe to study and adapt)
- **Focus**: Complete Cypher grammar coverage

### 6.2 Relevance

libcypher-parser aims for complete openCypher grammar coverage. It's useful as a
reference for parser edge cases — if pg_eddy's parser rejects a valid Cypher
query, checking whether libcypher-parser accepts it can help diagnose whether
the issue is a grammar gap or a deliberate exclusion.

However, libcypher-parser is a C library, and pg_eddy's parser is Rust. The
value is in the grammar specification, not the code.

---

## 7. Comparative Architecture Matrix

| Aspect | pg_eddy | Apache AGE | KuzuDB | FalkorDB | Memgraph |
|--------|---------|------------|--------|----------|----------|
| **Parser** | Hand-rolled RD (Rust) | Bison/Flex (C) | ANTLR4 (C++) | PEG (C) | Hand-rolled (C++) |
| **IR / Plan** | AST → LogicalPlan | AST → PG Query tree | AST → Bound tree → Logical → Physical | AST → Execution plan | AST → Logical → Physical |
| **Executor** | Interpreter (row-at-a-time) | PG executor + custom nodes | Vectorized (batch) | GraphBLAS (matrix) | Interpreted + compiled |
| **Storage** | Custom AM (adjacency pages) | PG heap tables + agtype | Columnar CSR | In-memory sparse matrix | In-memory delta MVCC |
| **Traversal** | Native adjacency follow | Heap scan + join | CSR scan | SpGEMM | Pointer chase |
| **Optimizer** | Heuristic (no cost model) | PG cost-based | Cost-based + WCO joins | Rule-based | Rule-based (e-graph v2 WIP) |
| **Indexes** | Label index only | PG btree + recent property index | Columnar index | None (full scan) | Label + property + text + vector |
| **Parallelism** | Single-threaded | PG parallel query | Morsel-driven | Single-threaded | Multi-threaded |
| **License** | Apache 2.0 | Apache 2.0 | MIT | SSPL | BSL |

---

## 8. Actionable Recommendations

### 8.1 — From AGE: Study the Bison Grammar as Syntax Reference (NOW)

AGE's `cypher_gram.y` is a 2500-line Bison grammar file that serves as a
complete, auditable specification of the Cypher syntax AGE accepts. When
implementing new syntax in pg_eddy's hand-rolled parser, cross-reference AGE's
grammar to verify:
- Operator precedence and associativity
- Keyword classification (reserved vs safe)
- Edge case handling (e.g., chained comparisons, list comprehension vs IN ambiguity)

The grammar is Apache 2.0 — safe to reference.

### 8.2 — From AGE: Chained Comparison Implementation (LOW EFFORT)

AGE implements chained comparisons (`1 < x < 10`) by detecting consecutive
comparison A_Expr nodes in the grammar and flattening them into AND chains.
Verify pg_eddy handles this correctly — it's a common user expectation that
many Cypher parsers get wrong.

### 8.3 — From AGE: EXPLAIN for Cypher Queries (MEDIUM EFFORT)

AGE provides full `EXPLAIN` / `EXPLAIN ANALYZE` / `EXPLAIN VERBOSE` support for
Cypher queries. pg_eddy could expose its LogicalPlan via an `EXPLAIN` prefix on
Cypher queries, outputting the plan tree as JSON or text. This is invaluable for
debugging query performance.

Implementation: parse `EXPLAIN` keyword before the Cypher query; instead of
executing, serialize the `LogicalPlan` to a human-readable format.

### 8.4 — From AGE: VLE Cache Pattern (MEDIUM EFFORT)

AGE recently added a VLE cache (#2376) that stores traversal results for
repeated variable-length patterns. pg_eddy's BFS-based `exec_var_length_expand()`
currently recomputes from scratch every time. If VLE queries are repeated within
a transaction (or within a single query with multiple VLE patterns), caching
intermediate results could significantly reduce redundant work.

### 8.5 — From AGE: NULL-Guard Pattern for Predicate Functions (LOW EFFORT)

AGE wraps all predicate functions (`all/any/none/single`) with:
```
CASE WHEN list IS NULL THEN NULL ELSE <predicate_result> END
```
This ensures correct null propagation per the openCypher spec. Verify pg_eddy's
`eval_expr` for these functions applies the same null semantics.

### 8.6 — From KuzuDB: Separate Semantic Analysis Pass (MEDIUM EFFORT)

Kuzu has a dedicated "Binder" phase between parsing and planning that handles
variable scope resolution, type checking, and schema binding. pg_eddy currently
mixes these concerns into the planner (e.g., `VarKind` tracking in the planner).
Separating semantic analysis would:
- Catch more errors before execution (better error messages)
- Make the planner simpler (it only deals with validated, typed inputs)
- Enable schema-aware optimizations (e.g., rejecting queries against non-existent labels early)

### 8.7 — From KuzuDB: Statistics for Join Reordering (LONGER TERM)

The single most impactful planner improvement for pg_eddy would be collecting
basic statistics:
- Number of nodes per label
- Number of edges per type + direction
- Property value distributions (min/max/ndistinct for indexed properties)

These enable the planner to choose better join orders. For example, if
`MATCH (a:Person)-[:KNOWS]->(b:Person)-[:WORKS_AT]->(c:Company)` is issued and
there are 1M Person nodes but only 100 Company nodes, starting the scan from
Company and traversing backwards would be dramatically faster.

### 8.8 — From FalkorDB/Memgraph: Execution Plan Caching (HIGH VALUE)

Both FalkorDB and Memgraph cache compiled execution plans for parameterized
queries. pg_eddy currently re-parses and re-plans every query. For application
workloads that issue the same query structure repeatedly with different parameter
values (which is the majority of production graph workloads), plan caching would
eliminate parsing and planning overhead.

Implementation sketch:
1. Normalize the query string (strip parameter values, canonicalize whitespace)
2. Hash the normalized form
3. Store `LogicalPlan` in a session-local `HashMap<u64, LogicalPlan>`
4. On cache hit: clone the cached plan, bind new parameters, execute
5. Evict on DDL changes (new labels, indexes) that might invalidate plans

### 8.9 — From Memgraph: Graph Algorithm Procedures (LONGER TERM)

Memgraph's MAGE library provides 40+ graph algorithms as callable procedures
(`CALL pagerank.get()`). pg_eddy could expose graph algorithms as PostgreSQL
functions that operate on the custom AM directly:
```sql
SELECT * FROM pg_eddy.pagerank('my_graph', 'Person', 'KNOWS', 0.85, 20);
```
This would differentiate pg_eddy from AGE (which has no built-in algorithms)
and provide immediate value for analytics workloads.

---

## 9. What NOT to Adopt

| Idea | Source | Why not |
|------|--------|---------|
| **Cypher → SQL transformation** | AGE | pg_eddy's native executor is faster; going through PG's join engine would be a regression |
| **Sparse matrix execution** | FalkorDB | Fundamentally different execution model; adjacency lists are better for traversal workloads |
| **In-memory-only storage** | Memgraph | pg_eddy's durability via PG WAL is a feature; pure in-memory loses crash safety |
| **ANTLR4 parser** | KuzuDB | ANTLR4-rust ecosystem is immature (rs-polygraph evaluated and rejected it); hand-rolled parser is working well |
| **Columnar storage** | KuzuDB | Wrong trade-off for traversal-first workloads; would require rewriting the entire AM |
| **E-graph planner** | Memgraph | Cutting-edge research; premature for pg_eddy's current maturity level |

---

## 10. Priority-Ordered Action Items

| # | Recommendation | Source | Effort | Impact |
|---|---------------|--------|--------|--------|
| 1 | Cross-reference AGE grammar for new parser features | AGE | Ongoing | Prevents syntax bugs |
| 2 | Verify null-guard on predicate functions | AGE | Low | Spec compliance |
| 3 | Verify chained comparison handling | AGE | Low | Spec compliance |
| 4 | Add EXPLAIN support for Cypher queries | AGE | Medium | Developer experience |
| 5 | Execution plan caching for parameterized queries | FalkorDB/Memgraph | Medium | Performance (repeated queries) |
| 6 | Separate semantic analysis from planner | KuzuDB | Medium | Better errors, simpler planner |
| 7 | VLE result caching | AGE | Medium | Performance (VLE queries) |
| 8 | Label/edge cardinality statistics | KuzuDB | Large | Join order optimization |
| 9 | Graph algorithm procedures | Memgraph | Large | Analytics workloads |

---

## 11. Summary

Apache AGE is the safest and most directly relevant reference implementation.
It's Apache 2.0 licensed, targets PostgreSQL, and implements the same Cypher
clauses pg_eddy does. Its Bison grammar is a reliable syntax cross-reference,
and its recent work on VLE caching and index scans directly applies to pg_eddy's
roadmap.

KuzuDB (MIT, archived) provides the academic gold standard for query processing
— vectorized execution, worst-case optimal joins, and cost-based planning. These
are aspirational targets for pg_eddy's optimizer roadmap, not immediate priorities.

FalkorDB and Memgraph offer ideas around plan caching and graph algorithms, but
their licenses (SSPL and BSL respectively) mean we should study architecture
only and never copy code.

Neo4j should be avoided entirely due to GPL contamination risk.

The common thread across all implementations: **the parser is the easy part;
the hard differentiation is in storage layout, execution strategy, and query
optimization.** pg_eddy's custom Table AM with native adjacency lists is its core
advantage. The recommendations above focus on making the query engine around
that storage layer smarter, not on changing the storage itself.
