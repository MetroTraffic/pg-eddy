# pg_eddy

[![CI Status](https://github.com/trickle-labs/pg-eddy/actions/workflows/ci.yml/badge.svg)](https://github.com/trickle-labs/pg-eddy/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![PostgreSQL 18+](https://img.shields.io/badge/PostgreSQL-18%2B-336791.svg)](https://www.postgresql.org/)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-CE4624.svg)](https://www.rust-lang.org/)
[![OpenCypher TCK](https://img.shields.io/badge/OpenCypher%20TCK-2391%2F3880%20passed-e67e22.svg)](tests/tck/)

A PostgreSQL 18 extension implementing a high-performance native **Labelled Property Graph (LPG)** store via a custom Table Access Method, enabling graph queries directly inside PostgreSQL without a separate database.

## Overview

pg_eddy delivers **adjacency-aware graph storage** inside PostgreSQL. Instead of storing graph data as heap tables and scanning B-tree indexes on every hop, pg_eddy uses a custom AM that places node adjacency information adjacent to node data on disk — enabling O(degree) per-hop traversal without index lookups.

**Why pg_eddy over Neo4j or Apache AGE?**

- **One system to operate**: single backup, single monitoring stack, single connection pool
- **Full ACID transactions**: spanning both graph and relational data in the same transaction
- **Faster multi-hop queries**: adjacency-follow design is 2–5× faster than AGE on MATCH patterns
- **Incremental view maintenance**: optional integration with [pg-trickle](https://github.com/trickle-labs/pg-trickle) for incrementally-maintained graph views

## Status

For a detailed feature breakdown and release history, see [CHANGELOG.md](CHANGELOG.md). For the complete roadmap with phases, exit criteria, and design rationale, see [plans/implementation_plan.md](plans/implementation_plan.md).

## Installation

### Prerequisites

- PostgreSQL 18.x
- Rust 1.85+ (with `cargo`)
- `cargo-pgrx` 0.18

### Build from Source

```bash
# Clone the repository
git clone https://github.com/trickle-labs/pg-eddy.git
cd pg-eddy

# Install cargo-pgrx (if not already installed)
OPENSSL_NO_VENDOR=1 cargo install cargo-pgrx --version 0.18.0 --locked

# Initialize pgrx with your PostgreSQL 18 installation
cargo pgrx init --pg18 /usr/lib/postgresql/18/bin/pg_config

# Build the extension
cd pg_eddy
cargo build --features pg18

# Run tests
cargo pgrx test pg18

# Install into your PostgreSQL instance
cargo pgrx install --release --features pg18
```

### Configuration

Add to `postgresql.conf`:

```ini
shared_preload_libraries = 'pg_eddy'
```

Then restart PostgreSQL:

```bash
pg_ctl restart
```

### Create the Extension

```sql
CREATE EXTENSION pg_eddy;
SELECT pg_eddy.health_check();  -- Should return 'pg_eddy OK'
```

## Quick Start

### Create Nodes

```sql
SELECT pg_eddy.create_node(
    ARRAY['Person'],
    '{"name": "Alice", "age": 30}'::jsonb
);  -- Returns node_id

SELECT pg_eddy.create_node(
    ARRAY['Person'],
    '{"name": "Bob", "age": 28}'::jsonb
);
```

### Read Nodes

```sql
SELECT pg_eddy.get_node(1);
-- Returns: {"name": "Alice", "age": 30}
```

### Count Nodes

```sql
SELECT pg_eddy.node_count();
-- Returns: 2
```

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                     Client Layer                        │
│  pgEddy.cypher(query, params)  │  SQL / SPI interface   │
└───────────────────┬─────────────────────────────────────┘
                    │
┌───────────────────▼─────────────────────────────────────┐
│              OpenCypher Query Engine                     │
│  Lexer → Parser → AST → Logical Plan → Physical Plan    │
│  Pattern rewriting · Filter pushdown · Index selection  │
└───────────────────┬─────────────────────────────────────┘
                    │
┌───────────────────▼─────────────────────────────────────┐
│               Native Graph Storage AM                   │
│  Node Store (custom pages) │ Edge Store (CSR pages)     │
│  Property Store (inline + overflow)                     │
│  Label Index (B-tree)  │  Rel-type Index (B-tree)       │
│  Property Index (B-tree per indexed property)           │
└───────────────────┬─────────────────────────────────────┘
                    │
┌───────────────────▼─────────────────────────────────────┐
│              Reactivity Layer (optional — pg_trickle)   │
│  Graph stream tables: MATCH views, path aggregates      │
│  IVM engine · DAG scheduler · CDC change capture        │
└─────────────────────────────────────────────────────────┘
```

## Development

### Run Tests

```bash
cd pg_eddy
cargo pgrx test pg18
```

### Run a Development PostgreSQL Instance

```bash
cd pg_eddy
cargo pgrx run pg18
```

Then in `psql`:

```sql
CREATE EXTENSION pg_eddy;
SELECT pg_eddy.health_check();
```

### Lint

```bash
cd pg_eddy
cargo clippy --features pg18
```

### Tasks

See [justfile](justfile) for common development tasks:

```bash
just build      # Build the extension
just test       # Run tests
just lint       # Run clippy
just run        # Start a development PostgreSQL instance
```

## Performance Expectations

pg_eddy targets **2–5× faster** multi-hop MATCH patterns than Apache AGE on graphs that fit in `shared_buffers`. Per-hop traversal is O(degree) via adjacency-follow (no index lookups).

**Honest trade-offs**:
- Neo4j on in-memory graphs: ~5–10× faster (native memory access vs PostgreSQL buffer manager)
- For I/O-bound graphs: both systems are I/O-dominated; the gap narrows to 2–3×
- See [plans/implementation_plan.md](plans/implementation_plan.md) for benchmarking strategy

## Documentation

- [CHANGELOG.md](CHANGELOG.md) — release notes and feature highlights
- [plans/implementation_plan.md](plans/implementation_plan.md) — complete design document, storage layout, query engine architecture, and phased roadmap
- [CONTRIBUTING.md](CONTRIBUTING.md) — contribution guidelines
- Source code: [pg_eddy/src/](pg_eddy/src/) — organized by layer (storage, catalog, cypher, etc.)

## Contributing

We welcome contributions! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines, and [plans/implementation_plan.md](plans/implementation_plan.md) for the current phase and next priorities.

## Compatibility

| Component | Version | Status |
|---|---|---|
| PostgreSQL | 18.x | Required |
| pgrx | 0.18 | Required |
| Rust Edition | 2024 | Required |
| Linux | Debian 11+, Ubuntu 20.04+ | Tested |
| macOS | 12+ (Intel/ARM) | Expected to work |
| Windows | WSL2 | Expected to work |

## License

pg_eddy is licensed under the **Apache License 2.0**. See [LICENSE](LICENSE) for details.

## Support

- **Issues**: [GitHub Issues](https://github.com/trickle-labs/pg-eddy/issues)
- **Discussions**: [GitHub Discussions](https://github.com/trickle-labs/pg-eddy/discussions)
- **Security**: See [SECURITY.md](SECURITY.md) for responsible disclosure

## Acknowledgments

pg_eddy's design draws inspiration from:
- **Neo4j's native store**: adjacency-aware layout and singly-linked relationship chains
- **PostgreSQL's AM API**: MVCC, WAL, buffer management, index integration
- **OpenCypher spec**: query language and TCK conformance target
- **pg-trickle**: incremental view maintenance and CDC integration patterns

## Roadmap

See [plans/implementation_plan.md](plans/implementation_plan.md) for the detailed technical roadmap with phase gates, exit criteria, and strategic rationale.
