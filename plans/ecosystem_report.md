# pg_eddy Ecosystem Report

A survey of the broader graph database ecosystem — tools, libraries, protocols,
and standards — assessed for relevance to pg_eddy. Items are grouped by
category and rated on two axes:

- **Migration value**: how much does supporting this help teams move from
  Neo4j/other graph systems to pg-eddy?
- **Net-new value**: how much does this make pg-eddy more useful independent of
  migration?

Rating scale: ★☆☆ low · ★★☆ medium · ★★★ high

---

## 1. Query Language Standards

### 1.1 GQL (ISO/IEC 39075 — Graph Query Language)

| | |
|---|---|
| What it is | The ISO standard graph query language, ratified in 2024. Syntactically similar to Cypher but not identical; defines property graphs, path patterns, and graph updates as an international standard. |
| Migration value | ★★★ |
| Net-new value | ★★★ |
| Notes | GQL is to graph databases what SQL is to relational. Neo4j has announced GQL support; it will become the lingua franca. pg-eddy's Cypher parser will need to evolve toward GQL compliance over time. The openCypher grammar is a reasonable stepping stone — the two languages are close enough that most constructs map 1:1, but there are differences (e.g. GQL's `MATCH` is a separate statement, not a clause; `INSERT`/`SET` syntax differs). Not an immediate priority but the right long-term direction. |
| Recommendation | Track the GQL spec. Document divergences. Begin alignment after openCypher TCK reaches ~95%. |

### 1.2 openCypher (already in scope)

Already the primary conformance target. No further assessment needed here.

### 1.3 SPARQL / RDF

| | |
|---|---|
| What it is | W3C standard query language for RDF triple stores. Fundamentally different data model (triples, not LPG). |
| Migration value | ★☆☆ |
| Net-new value | ★☆☆ |
| Notes | RDF and LPG are different enough that bridging them is a substantial research project, not an extension. PostgreSQL already has `rdf_fdw` for RDF access. Out of scope. |
| Recommendation | Out of scope. |

---

## 2. Wire Protocols & Drivers

### 2.1 Bolt Protocol (Neo4j)

| | |
|---|---|
| What it is | Neo4j's binary application protocol (analogous to PostgreSQL's wire protocol). All official Neo4j drivers (Python, Java, JavaScript, Go, .NET, Rust) speak Bolt, not SQL. |
| Migration value | ★★★ |
| Net-new value | ★★☆ |
| Notes | This is the single largest migration friction point that openCypher conformance alone does not solve. A team with a Python application using `neo4j` (the official driver) connecting over Bolt cannot simply point it at pg-eddy — they must rewrite the connection layer to use `psycopg` + a Cypher-over-SQL adapter. A Bolt-to-PostgreSQL proxy (similar in concept to `pgbouncer` but speaking Bolt on one side and libpq on the other) would allow zero-code-change driver reuse. This is a substantial project — Bolt 5.x is a complex binary protocol with streaming, routing, bookmarks, and reactive messaging. However, it may be the highest-leverage single investment for Neo4j migration. |
| Recommendation | Scope a standalone `bolt-pg-proxy` project (separate repo, not a PostgreSQL extension) after v1.0. A Rust implementation using `tokio` is feasible. Treat as a separate product. |

### 2.2 HTTP/Cypher Transactional API (Neo4j)

| | |
|---|---|
| What it is | Neo4j's legacy REST API (`POST /db/neo4j/tx/commit` with JSON body `{"statements": [{"statement": "MATCH ..."}]}`). Used by some older drivers and tooling. |
| Migration value | ★★☆ |
| Net-new value | ★☆☆ |
| Notes | The `pg_eddy_http` crate is already a placeholder for this. The transactional HTTP API is simpler than Bolt — a JSON-over-HTTP wrapper around Cypher. Useful for scripting, curl-based testing, and tools that speak the HTTP API (some BI connectors, older Python libraries). |
| Recommendation | Implement in `pg_eddy_http` after the Cypher engine is stable. Lower priority than Bolt proxy. |

### 2.3 Apache TinkerPop / Gremlin

| | |
|---|---|
| What it is | Apache's graph computing framework. Gremlin is its traversal language; TinkerPop's Gremlin Server speaks a WebSocket protocol. Used by Amazon Neptune, JanusGraph, Cosmos DB. |
| Migration value | ★★☆ |
| Net-new value | ★☆☆ |
| Notes | Different user base from Neo4j/Cypher. A Gremlin-to-Cypher translation layer is theoretically possible (several exist: Gremlin-to-Cypher transpilers have been written by AWS and others) but the semantics differ in subtle ways (lazy vs. eager evaluation, path tracking). Targeting Gremlin migrations is a separate effort from Neo4j migrations. |
| Recommendation | Deferred. Revisit if there is user demand from Gremlin-ecosystem users. A Gremlin-to-Cypher transpiler could be a thin shim in front of the existing engine. |

---

## 3. Graph Data Science / Algorithm Libraries

### 3.1 Neo4j GDS (Graph Data Science Library)

| | |
|---|---|
| What it is | Neo4j's first-party graph algorithms library. Provides ~50 production-quality graph algorithms: centrality (PageRank, Betweenness, HITS), community detection (Louvain, Label Propagation, WCC), path finding (Dijkstra, A*, Yen's k-shortest), similarity (Node2Vec, Cosine, Jaccard), and link prediction. Also includes a graph projection/catalog system for in-memory algorithm execution. |
| Migration value | ★★★ |
| Net-new value | ★★★ |
| Notes | GDS is heavily used in production — fraud detection, recommendation systems, knowledge graphs all rely on it. Unlike APOC, there is no PostgreSQL equivalent to fall back on. `pg_routing` covers some path-finding cases but not centrality or community detection. This is the highest-value algorithm gap in the current PostgreSQL ecosystem. A `pg_eddy_gds` extension implementing the top 15–20 GDS algorithms using the AM's traversal primitives would be genuinely differentiated. Underlying Rust crates (`petgraph`, or a custom streaming implementation) can handle most of the algorithm logic. See also §3.2 on the in-memory projection problem. |
| Recommendation | High priority after v1.0. Begin with path-finding and centrality (most commonly used). Design an in-memory graph projection API that snapshots a subgraph for algorithm execution. |

### 3.2 In-Memory Graph Projection

| | |
|---|---|
| What it is | GDS works on "projected graphs" — an in-memory snapshot of (a subset of) the stored graph optimized for algorithm traversal. The projection is separate from the storage layer. |
| Migration value | ★★☆ |
| Net-new value | ★★★ |
| Notes | Most graph algorithms are not expressible as single-pass streaming operations over the AM. PageRank, for instance, requires multiple iterations over all nodes. Running this directly against the AM (with buffer manager overhead per node visit per iteration) would be prohibitively slow for large graphs. A projection layer — materializing the relevant subgraph into a CSR (compressed sparse row) or adjacency list in PostgreSQL shared memory or a palloc'd context — is necessary for algorithm performance. This is a core infrastructure piece that `pg_eddy_gds` would build on. It also has applicability beyond GDS (e.g., EXPLAIN-visible graph statistics). |
| Recommendation | Design and implement as part of `pg_eddy_gds` Phase A. |

### 3.3 Memgraph MAGE (Memgraph Advanced Graph Extensions)

| | |
|---|---|
| What it is | Memgraph's open-source graph algorithm and ML library, similar in scope to Neo4j GDS. Implements algorithms in C++ and Python as Memgraph query modules. Covers centrality, community detection, link prediction, node classification, and graph neural networks (via PyTorch Geometric integration). |
| Migration value | ★★☆ |
| Net-new value | ★★☆ |
| Notes | MAGE is MIT-licensed. Its algorithm implementations are a useful reference. The GNN/ML integration (graph embeddings, node classification) is interesting but requires Python runtime integration which is non-trivial in a PostgreSQL extension. Memgraph is a smaller user base than Neo4j; migration path is less common. |
| Recommendation | Use MAGE as an algorithm reference implementation. The GNN integration is deferred until there is a clean story for Python interop (possibly via `plpython3u` or a separate service). |

### 3.4 NetworkX (Python)

| | |
|---|---|
| What it is | The standard Python library for graph analysis. Not a database extension — users load graphs into Python, run algorithms, and write results back. |
| Migration value | ★☆☆ |
| Net-new value | ★★☆ |
| Notes | Many data science workflows use NetworkX on data exported from Neo4j. Supporting this workflow from pg-eddy means: (a) efficient `COPY`-based export of subgraphs to the NetworkX format, and (b) potentially a Python connector (`pg_eddy_python`) that lets NetworkX operate directly on the pg-eddy graph without a full export. The latter would require the projection layer from §3.2. |
| Recommendation | Document the export-to-NetworkX pattern in the migration guide. A native connector is post-v1.0. |

---

## 4. Visualization & BI

### 4.1 Neo4j Bloom

| | |
|---|---|
| What it is | Neo4j's first-party graph visualization product. Speaks Bolt. |
| Migration value | ★★☆ |
| Net-new value | ★☆☆ |
| Notes | Bloom is closed-source and will not be ported. However, if the Bolt proxy (§2.1) is implemented, Bloom could potentially connect to pg-eddy transparently — it only needs Bolt + Cypher. |
| Recommendation | No direct work. Depends entirely on the Bolt proxy. |

### 4.2 Gephi / Sigma.js / Cytoscape

| | |
|---|---|
| What it is | Open-source graph visualization tools. Gephi speaks GraphML/GEXF; Cytoscape has its own format; Sigma.js is JavaScript. |
| Migration value | ★★☆ |
| Net-new value | ★★☆ |
| Notes | Export functions in `pg_eddy_http` or as SQL functions (`cypher_to_graphml(query text)`, etc.) would allow pg-eddy graphs to be visualized in any of these tools. This is lower-effort than a full Bolt proxy and useful for data exploration. GraphML and Cytoscape JSON are simple enough formats to generate from a SELECT. |
| Recommendation | Add `pg_eddy_export` functions (GraphML, GeoJSON, JSON Graph Format) after the HTTP API stabilizes. |

### 4.3 BI Connectors (Tableau, Power BI, Looker)

| | |
|---|---|
| What it is | Neo4j has official BI connectors that present the graph as a tabular view for BI tools. |
| Migration value | ★★☆ |
| Net-new value | ★★★ |
| Notes | pg-eddy's data is ultimately in PostgreSQL, so any PostgreSQL JDBC/ODBC connector already works. BI tools can query pg-eddy graphs using SQL (joining against the node/edge tables) or, once the HTTP API exists, by embedding Cypher in SQL via a function (`SELECT * FROM cypher_table('MATCH ...')`). This is already partially available. The main gap is a friendly "virtual table" layer that makes the graph look like dimension/fact tables for BI. |
| Recommendation | Document the SQL interface to pg-eddy for BI users. A `CREATE FOREIGN TABLE ... USING pg_eddy_fdw` wrapper could present graph data as virtual relational tables. |

---

## 5. ETL & Data Integration

### 5.1 Apache Kafka / Confluent

| | |
|---|---|
| What it is | Neo4j Streams plugin and Confluent connector allow bidirectional sync between Kafka topics and the graph. |
| Migration value | ★★☆ |
| Net-new value | ★★★ |
| Notes | PostgreSQL already has logical replication and `pgoutput`. A Kafka connector for pg-eddy would be: (a) a logical replication consumer that writes graph events to Kafka, and (b) a Kafka consumer that applies events to the graph. The pg-eddy WAL resource manager already emits structured WAL records — a logical replication output plugin that translates these to Kafka-consumable events is feasible. |
| Recommendation | Design a logical replication output plugin (`pg_eddy_output`) that emits node/edge change events after the WAL RMGR is stable. |

### 5.2 Apache Spark (GraphX / GraphFrames)

| | |
|---|---|
| What it is | Spark's graph processing APIs. GraphFrames is a higher-level DataFrame-based graph library built on Spark. Neo4j has an official Spark connector. |
| Migration value | ★★☆ |
| Net-new value | ★★☆ |
| Notes | The Neo4j Spark connector reads/writes graphs over Bolt. A pg-eddy Spark connector would use the standard PostgreSQL JDBC driver (already exists) plus a schema convention for nodes/edges. This is primarily a connector configuration problem, not an extension problem — document the JDBC schema layout. |
| Recommendation | Write a Spark connector configuration guide. No new extension code needed. |

### 5.3 dbt (data build tool)

| | |
|---|---|
| What it is | SQL-first transformation tool. No native graph support, but dbt models can query pg-eddy's SQL-accessible graph data. |
| Migration value | ★☆☆ |
| Net-new value | ★★☆ |
| Notes | Teams that use dbt with PostgreSQL can already use dbt to build models on top of pg-eddy's relational tables. A dbt adapter or macro library that exposes Cypher-returning functions as dbt sources would improve ergonomics. |
| Recommendation | Write a dbt macro library (`dbt-pg-eddy`) post-v1.0 if there is user demand. |

---

## 6. ORM / ODM / Application Libraries

### 6.1 Official Neo4j Drivers (Python, Java, JS, Go, .NET, Rust)

| | |
|---|---|
| What it is | Neo4j's official client libraries. All speak Bolt. |
| Migration value | ★★★ |
| Net-new value | ★☆☆ |
| Notes | Zero pg-eddy code will make these work without the Bolt proxy (§2.1). The Rust driver (`neo4rs`) is particularly relevant given pg-eddy's implementation language. |
| Recommendation | Blocked on Bolt proxy. Document replacement libraries (psycopg, asyncpg, etc.) in the migration guide. |

### 6.2 Spring Data Neo4j

| | |
|---|---|
| What it is | Java/Spring ORM for Neo4j. Maps Java objects to nodes/relationships via annotations. Very widely used in enterprise Neo4j deployments. |
| Migration value | ★★★ |
| Net-new value | ★☆☆ |
| Notes | Spring Data Neo4j speaks Bolt and is tightly coupled to Neo4j semantics. It cannot be redirected to pg-eddy without the Bolt proxy. Even with the proxy, SDN uses Neo4j-specific features (internal node IDs, `elementId()`, etc.) that would need compatibility stubs. |
| Recommendation | Blocked on Bolt proxy. Track SDN-specific features that need stubs. |

### 6.3 py2neo / neomodel / neo4j-ogm

| | |
|---|---|
| What it is | Python-level Neo4j ORM/ODM libraries. |
| Migration value | ★★☆ |
| Net-new value | ★☆☆ |
| Notes | Same Bolt dependency as §6.1. `neomodel` is the most widely used Python OGM. |
| Recommendation | Blocked on Bolt proxy. |

### 6.4 Cypher query builders (non-driver)

| | |
|---|---|
| What it is | Libraries like `cypher-builder` (TypeScript), `neomodel` query DSL, `pycypher` that generate Cypher strings without speaking Bolt directly. |
| Migration value | ★★☆ |
| Net-new value | ★★☆ |
| Notes | These generate Cypher strings that can be passed to any Cypher executor. Once pg-eddy has a clean `cypher(query, params)` SQL function interface, these libraries work with zero modification by passing the generated query string to psycopg/libpq. This is already the case with the current SQL interface. |
| Recommendation | Document Cypher query builder compatibility in the migration guide. No extension changes needed. |

---

## 7. Graph Neural Networks & ML

### 7.1 PyTorch Geometric (PyG)

| | |
|---|---|
| What it is | The dominant Python library for graph neural networks. Expects data as edge lists + feature matrices. |
| Migration value | ★☆☆ |
| Net-new value | ★★★ |
| Notes | The primary workflow is: export graph + features from database → train GNN → store embeddings back. pg-eddy is well-positioned here because: (a) it can store node embeddings as `vector` properties (with `pgvector`), and (b) the projection layer (§3.2) could expose graph snapshots as PyG-compatible tensors via a Python extension. This is a post-v2.0 topic. |
| Recommendation | Deferred. Design a `pg_eddy_ml` extension that bridges the projection layer and PyG/PyTorch via `plpython3u` after v1.0. |

### 7.2 pgvector integration

| | |
|---|---|
| What it is | `pgvector` is a PostgreSQL extension for vector similarity search. Storing node embeddings alongside graph properties enables hybrid graph+vector queries. |
| Migration value | ★☆☆ |
| Net-new value | ★★★ |
| Notes | This is one of the most compelling differentiated features pg-eddy could offer that Neo4j cannot match without an external vector index. A query like "find nodes similar to X within 3 hops of Y" — combining vector ANN search with graph traversal — is currently not possible in any single system. pg-eddy + pgvector in the same PostgreSQL instance enables this naturally. The main work is ensuring `vector` columns are supported as node/edge properties and that the executor can apply `pgvector` operators inside a Cypher `WHERE` clause. |
| Recommendation | **High priority**, independent of v1.0. File this as a tracked feature. The integration is mostly about property type support and executor clause evaluation — not a separate extension. |

---

## 8. Observability & Operations

### 8.1 pg_stat_statements / auto_explain

| | |
|---|---|
| What it is | Standard PostgreSQL query profiling extensions. |
| Migration value | ★★☆ |
| Net-new value | ★★★ |
| Notes | Cypher queries compiled to PostgreSQL plans should appear in `pg_stat_statements` automatically if they go through the standard executor. Ensuring pg-eddy's plans are EXPLAIN-able and appear in `pg_stat_statements` with meaningful query text (not just the internal SQL wrapper) is important for production observability. |
| Recommendation | Ensure EXPLAIN compatibility and `pg_stat_statements` registration as part of the executor work. Not a separate extension. |

### 8.2 Prometheus / OpenTelemetry

| | |
|---|---|
| What it is | Standard observability stacks. |
| Migration value | ★★☆ |
| Net-new value | ★★☆ |
| Notes | PostgreSQL already exposes metrics via `pg_stat_*` views. These are scraped by `postgres_exporter` for Prometheus. pg-eddy should expose graph-specific metrics (node count, edge count, AM buffer hits/misses, WAL RMGR record counts) via custom `pg_stat_*` views rather than a separate metrics endpoint. |
| Recommendation | Add `pg_stat_eddy` view (node/edge counts, traversal stats, WAL stats) as part of the catalog work. |

### 8.3 CNPG (CloudNativePG) / Kubernetes operators

| | |
|---|---|
| What it is | PostgreSQL Kubernetes operators that manage HA clusters, backups, and failover. |
| Migration value | ★★★ |
| Net-new value | ★★★ |
| Notes | One of pg-eddy's explicit design goals is "operate with standard PostgreSQL tooling". CNPG and similar operators work if pg-eddy's WAL resource manager correctly handles crash recovery and the extension loads via `shared_preload_libraries`. This is already required by the core design — the TAP tests cover crash recovery. Validate specifically that pg-eddy survives a CNPG failover (primary → standby promotion) without data loss. |
| Recommendation | Add a TAP test for standby promotion and CNPG-style backup/restore. |

---

## 9. Other Graph Databases' Ecosystems (migration sources)

### 9.1 Amazon Neptune

| | |
|---|---|
| What it is | AWS managed graph database. Supports both Gremlin (TinkerPop) and SPARQL (RDF), and recently added openCypher support. |
| Migration value | ★★☆ |
| Net-new value | ★☆☆ |
| Notes | Neptune's openCypher support is a subset of the spec. Migration from Neptune openCypher to pg-eddy Cypher is mostly a matter of Cypher conformance (already in scope). Neptune's Gremlin users need a Gremlin-to-Cypher path (see §2.3). Neptune users running on AWS who want to consolidate onto Aurora PostgreSQL are a realistic migration audience. |
| Recommendation | Document Neptune openCypher compatibility gaps in the migration guide after TCK reaches ~95%. |

### 9.2 ArangoDB

| | |
|---|---|
| What it is | Multi-model database (document + graph + key-value). Uses AQL (ArangoDB Query Language), not Cypher. |
| Migration value | ★☆☆ |
| Net-new value | ★☆☆ |
| Notes | AQL and Cypher are not compatible. Migration requires query rewrite regardless. No shim is feasible. |
| Recommendation | Out of scope. |

### 9.3 TigerGraph

| | |
|---|---|
| What it is | Enterprise graph database using GSQL (a SQL-like procedural language). |
| Migration value | ★☆☆ |
| Net-new value | ★☆☆ |
| Notes | GSQL is proprietary and procedural. No migration path without full query rewrite. |
| Recommendation | Out of scope. |

### 9.4 Apache AGE (current PostgreSQL alternative)

| | |
|---|---|
| What it is | The existing PostgreSQL graph extension. Stores properties as JSONB in heap tables. |
| Migration value | ★★★ |
| Net-new value | ★★☆ |
| Notes | Teams already on AGE are the easiest migration target — they are already on PostgreSQL, already writing Cypher, and can stay on the same infrastructure. An AGE-to-pg-eddy migration tool (schema converter + data exporter that reads AGE's internal tables and writes into the pg-eddy AM) would make this a one-command migration. |
| Recommendation | Build an `age_to_pg_eddy` migration utility after v1.0. High value, relatively low effort (both are PostgreSQL extensions with accessible internal tables). |

---

## 10. Summary Prioritisation

| Item | Migration value | Net-new value | Recommended timing |
|---|---|---|---|
| pgvector integration | ★☆☆ | ★★★ | During v1.x, independent of other items |
| GDS algorithms + projection layer | ★★★ | ★★★ | Post v1.0, first extension project |
| APOC compat shim (top ~25) | ★★★ | ★☆☆ | Post v1.0, see apoc_compat_plan.md |
| GQL alignment | ★★★ | ★★★ | Ongoing, begin after TCK ~95% |
| Bolt proxy (standalone) | ★★★ | ★★☆ | Post v1.0, separate repo |
| AGE migration utility | ★★★ | ★★☆ | Post v1.0 |
| HTTP/Cypher API (pg_eddy_http) | ★★☆ | ★★☆ | When Cypher engine is stable |
| pg_stat_eddy view | ★★☆ | ★★★ | With catalog work |
| EXPLAIN / pg_stat_statements compat | ★★☆ | ★★★ | With executor work |
| CNPG failover TAP test | ★★★ | ★★★ | Add to TAP suite soon |
| Graph export functions (GraphML etc.) | ★★☆ | ★★☆ | Post v1.0, low effort |
| Kafka logical replication output | ★★☆ | ★★★ | Post v1.0 |
| Gremlin-to-Cypher transpiler | ★★☆ | ★☆☆ | Deferred, needs user demand |
| dbt macro library | ★☆☆ | ★★☆ | Deferred, needs user demand |
| pg_eddy_ml / PyTorch Geometric | ★☆☆ | ★★★ | Post v2.0 |
| Spring Data Neo4j compat | ★★★ | ★☆☆ | Blocked on Bolt proxy |
| NetworkX connector | ★☆☆ | ★★☆ | Deferred |
| Spark connector guide | ★★☆ | ★★☆ | Documentation only, post v1.0 |
| Amazon Neptune compat notes | ★★☆ | ★☆☆ | Documentation only |
| ArangoDB / TigerGraph | — | — | Out of scope |
| SPARQL / RDF | — | — | Out of scope |
