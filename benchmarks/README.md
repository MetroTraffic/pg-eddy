# pg_eddy Benchmark Results

This file records the AGE benchmark baseline from v0.5.1.
It is the **gate** for proceeding to the Cypher query engine (Phase 5 / v0.6.0).

## Decision table

| Outcome | Action |
|---|---|
| pg_eddy ≥ 2× faster than AGE on 2-hop expand | Proceed to v0.6.0 (Cypher engine) |
| pg_eddy 1–2× faster than AGE on all operations | Proceed but file storage-engine issues as P1 bugs |
| pg_eddy slower than AGE on any operation | **Stop** — diagnose and fix in v0.5.2 before query-engine work |

---

## Environment

<!-- Fill in after running benchmarks -->

| Field | Value |
|---|---|
| Date | _(fill in)_ |
| Hardware | _(fill in, e.g. "16-core Intel i9-13900K, 64 GB DDR5, NVMe SSD")_ |
| PostgreSQL version | 18.x |
| pg_eddy version | 0.5.1 |
| Apache AGE version | _(fill in)_ |
| `shared_buffers` | _(fill in, e.g. "4GB")_ |
| `wal_level` | replica |
| `full_page_writes` | on |
| Dataset | 1 000 000 nodes / 10 000 000 edges (synthetic, random graph) |

---

## How to run

### Prerequisites

```bash
# pg_eddy (already installed as extension)
# Apache AGE — install from https://github.com/apache/age
sudo apt-get install -y postgresql-18-age   # or build from source
```

### Generate the dataset

```bash
cd benchmarks/
psql -c "CREATE EXTENSION IF NOT EXISTS pg_eddy;"
psql -c "CREATE EXTENSION IF NOT EXISTS age;"
psql -f setup_dataset.sql
```

### Run benchmarks

```bash
psql -f bench_insert.sql       | tee results/insert.txt
psql -f bench_1hop.sql         | tee results/1hop.txt
psql -f bench_2hop.sql         | tee results/2hop.txt
```

---

## Results

### 1. Node insert throughput (100 000 nodes)

| Engine | Time (s) | Throughput (nodes/s) |
|---|---|---|
| pg_eddy | _(fill in)_ | _(fill in)_ |
| AGE | _(fill in)_ | _(fill in)_ |
| **Ratio (pg_eddy / AGE)** | — | _(fill in)_ |

### 2. 1-hop adjacency follow (10 random starting nodes, avg over 1 000 000 edges)

| Engine | Time (ms/query) | p50 | p99 |
|---|---|---|---|
| pg_eddy `neighbours()` | _(fill in)_ | _(fill in)_ | _(fill in)_ |
| AGE `MATCH (a)-[]->(b) WHERE id(a)=N RETURN b` | _(fill in)_ | _(fill in)_ | _(fill in)_ |
| **Ratio (pg_eddy / AGE)** | _(fill in)_ | — | — |

### 3. 2-hop neighbour expansion (10 random starting nodes, avg over 1 000 000 edges)

| Engine | Time (ms/query) | p50 | p99 |
|---|---|---|---|
| pg_eddy `neighbours()` × 2 | _(fill in)_ | _(fill in)_ | _(fill in)_ |
| AGE `MATCH (a)-[*2]->(b) WHERE id(a)=N RETURN DISTINCT b` | _(fill in)_ | _(fill in)_ | _(fill in)_ |
| **Ratio (pg_eddy / AGE)** | _(fill in)_ | — | — |

---

## Gate decision

<!-- Fill in after running benchmarks -->

**Decision**: _(proceed / P1 bugs filed / stop)_

**Rationale**: _(brief note on bottlenecks found, if any)_

---

## Appendix: Raw query plans

<!-- Paste `EXPLAIN (ANALYZE, BUFFERS)` output here -->
