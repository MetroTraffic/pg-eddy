# openCypher TCK Harness

This directory contains the TCK (Technology Compatibility Kit) harness for
pg_eddy. It parses `.feature` files from `vendor/opencypher/tck/features/`
and runs each scenario against a fresh pg_eddy cluster.

## Running

```bash
just tck                        # run the full TCK
just tck-group match            # run only the 'match' clause group
just tck-group return           # run only the 'return' clause group
```

Or manually:

```bash
PG_REGRESS=/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress \
PERL5LIB=/usr/lib/postgresql/18/lib/pgxs/src/test/perl:$PERL5LIB \
PATH=/usr/lib/postgresql/18/bin:$PATH \
prove -v tests/tck/run_tck.pl
```

## Filtering

```bash
TCK_GROUPS='match'                prove tests/tck/run_tck.pl   # only match
TCK_SKIP_GROUPS='with,match-where' prove tests/tck/run_tck.pl  # skip groups
```

## Scenario Lifecycle

Each scenario runs inside a PostgreSQL transaction that is rolled back at the
end, so scenarios are isolated even when they share a cluster.

1. **Setup** (`And having executed:`) — Cypher CREATE statements are parsed
   by the harness and converted to `create_node()` / `create_edge()` SQL
   calls.
2. **Query** (`When executing query:`) — executed via
   `SELECT * FROM cypher('...')`.
3. **Comparison** (`Then the result should be...`) — result rows (JSONB) are
   compared with the expected Gherkin table, handling nodes, edges, scalars.

## What Is Supported

| Feature | Status |
|---------|--------|
| `Given an empty graph` | ✅ |
| `And having executed: CREATE (:Label {props})` | ✅ |
| `And having executed: CREATE (a)-[:TYPE]->(b)` | ✅ |
| `MATCH (n) RETURN n.prop` | ✅ |
| `MATCH (a)-[:TYPE]->(b) RETURN a, b` | ✅ |
| `Then the result should be, in any order:` | ✅ |
| `Then the result should be, in order:` | ✅ |
| `Then a SyntaxError should be raised` | ✅ (error check) |
| `Scenario Outline` with `Examples:` | ✅ |
| `Given any graph` | ⏭ skipped |
| Complex multi-clause queries (WITH, OPTIONAL MATCH) | ⏭ skipped (v0.7.0) |
| `ORDER BY`, `SKIP`, `LIMIT` | ⏭ skipped (v0.7.0) |

## Pass Rate Goal

- **v0.7.0 target**: ≥25% overall, all of `MatchAcceptance` group passing
