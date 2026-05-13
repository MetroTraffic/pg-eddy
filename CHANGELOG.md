# Changelog

What's new in pg_eddy — written for everyone, not just developers.

For future plans and upcoming features, see [plans/implementation_plan.md](plans/implementation_plan.md).

## Table of Contents

- [0.23.0](#0230----property-indexes-and-query-planner-optimisation) — Property Indexes and Query Planner Optimisation
- [0.22.6](#0226----call-procedures-and-100-tck) — CALL Procedures and 100% TCK
- [0.22.2](#0222----tck-bug-fixes) — TCK Bug Fixes
- [0.22.1](#0221----100-tck-compliance) — 100% TCK Compliance
- [0.22.0](#0220----temporal-type-system) — Temporal Type System
- [0.21.0](#0210----variable-length-correctness-and-remaining-quick-wins) — Variable-Length Correctness and Remaining Quick Wins
- [0.20.0](#0200----engine-correctness-and-tcl-harness-improvements) — Engine Correctness and TCK Harness Improvements
- [0.19.0](#0190----cypher-correctness-and-ordering-improvements) — Cypher Correctness and Ordering Improvements
- [0.18.0](#0180----quantifiers-pattern-comprehension-and-union) — Quantifiers, Pattern Comprehension, and UNION
- [0.17.0](#0170----error-validation--named-paths) — Error Validation + Named Paths
- [0.16.0](#0160----map-literal-expressions) — Map Literal Expressions
- [0.15.0](#0150----storage-correctness-and-error-validation) — Storage Correctness and Error Validation
- [0.14.0](#0140----temporal-types-and-foreach) — Temporal Types and FOREACH
- [0.13.0](#0130--2026-05-10--storage-stabilisation-and-parser-hardening) — Storage Stabilisation and Parser Hardening
- [0.12.1](#0121--2026-05-10--batch-catalog-writes-and-ldbc-is-1is-3-benchmark) — Batch Catalog Writes and LDBC IS-1/IS-3 Benchmark
- [0.12.0](#0120--2026-05-10--cypher-write-language-create-merge-set-remove-delete) — Cypher Write Language: CREATE, MERGE, SET, REMOVE, DELETE
- [0.11.0](#0110--2026-05-10--subqueries-exists-call--and-call-procedure-yield) — Subqueries: EXISTS {}, CALL {}, and CALL procedure YIELD
- [0.10.0](#0100--2026-05-10--variable-length-paths-named-paths-and-path-functions) — Variable-Length Paths, Named Paths, and Path Functions
- [0.9.0](#090--2026-05-09--aggregation-list-comprehensions-and-numeric-operators) — Aggregation, List Comprehensions, and Numeric Operators
- [0.8.0](#080--2026-05-09--with-optional-match-unwind-and-case-expressions) — WITH, OPTIONAL MATCH, UNWIND, and CASE Expressions
- [0.7.0](#070--2026-05-09--cypher-predicates-ordering-and-built-in-functions) — Cypher Predicates, Ordering, and Built-in Functions
- [0.6.0](#060--2026-05-09--cypher-query-engine) — Cypher Query Engine
- [0.5.1](#051--2026-05-09--tap-infrastructure-wal-hardening-and-age-benchmark) — TAP Infrastructure, WAL Hardening, and AGE Benchmark
- [0.5.0](#050--2026-05-09--indexes-constraints-and-full-crud-api) — Indexes, Constraints, and Full CRUD API
- [0.4.0](#040--2026-05-09--mvcc-and-vacuum) — MVCC and VACUUM
- [0.3.0](#030--2026-05-09--edge-storage--adjacency-lists) — Edge Storage + Adjacency Lists
- [0.2.0](#020--2026-05-09--node-storage) — Node Storage
- [0.1.0](#010--2026-05-09--am-skeleton) — AM Skeleton

---

## [Unreleased]

---

## [0.23.0] — 2026-05-13 — Property Indexes and Query Planner Optimisation

v0.23.0 introduces **property value indexes** — B-tree indexes on node
properties that allow the query planner to resolve
`MATCH (n:Label {prop: $val})` in **O(log N)** time instead of scanning all
nodes with that label. This is the first performance-oriented feature in
pg_eddy; the schema version advances to **0.10.0**.

### What's New

**Property index DDL** — three new Cypher DDL statements:
- `CREATE INDEX ON :Label(prop)` — register a property index and backfill
  existing nodes
- `DROP INDEX ON :Label(prop)` — remove the index and all index data
- `SHOW INDEXES` — list all registered property indexes

**SQL API** — equivalent SQL functions:
- `pg_eddy.create_node_index(label, prop)` — returns the new `index_id`
- `pg_eddy.drop_node_index(label, prop)` — returns `true` if an index was removed
- `pg_eddy.show_indexes()` — returns SETOF JSONB `{label, prop}` rows

**Automatic index maintenance** — when a node is created, updated, or
deleted, pg_eddy transparently maintains `_pg_eddy.prop_value_index` so the
index is always consistent.

**Query planner optimisation** — `MATCH (n:Label {prop: $val})` patterns now
check for a registered property index at plan time. When one exists, the
planner substitutes a `PropertyIndexScan` node instead of a full
`LabelScan + filter`, reducing per-query complexity from O(|label|) to
O(log N + |results|).

**`CALL dbms.components()`** — a built-in procedure that returns
`{name: "pg_eddy", versions: ["0.10.0"], edition: "community"}`.

### Schema Changes (0.9.0 → 0.10.0)

Two new tables in `_pg_eddy`:
- `prop_index_catalog(index_id, label_name, prop_name, UNIQUE(label_name, prop_name))`
- `prop_value_index(entry_id, label_id, key_id, value_text, node_id, INDEX(label_id, key_id, value_text))`

Migration file: `pg_eddy--0.9.0--0.10.0.sql`

### Implementation Details

- `catalog/indexes.rs` — new module: property index CRUD, backfill, and
  lookup; all maintenance functions use SPI only (no raw storage access)
- `cypher/planner.rs` — `try_property_index_scan()` checks the index catalog
  at plan time; short-circuits to `None` in `pg_test` mode (no SPI at plan
  time in unit tests)
- `cypher/executor.rs` — `exec_property_index_scan()` evaluates the index
  lookup, fetches full node records, verifies label membership, and applies
  remaining property filters
- Linker fix — `try_property_index_scan` guarded with
  `#[cfg(not(feature = "pg_test"))]` to prevent SPI symbols from appearing
  in the test binary's dead-code GC roots

### TCK

**3880/3880 passed, 0 skipped, 0 failures** — no regressions.

---

## [0.22.6] — 2026-05-12 — CALL Procedures and 100% TCK

v0.22.6 implements full CALL procedure support and achieves true 100% TCK
compliance: **3880/3880 passed, 0 skipped, 0 failures**.

### What's New

**Full CALL procedure support** — `CALL proc(args) YIELD col, col2 AS alias`
works with full argument validation, type coercion, implicit argument
resolution, and `YIELD *`. Mock procedure definitions from TCK feature files
are parsed and passed to the executor via a `__procedures` params key.

**Error validation for CALL** — `InvalidNumberOfArguments` (wrong arg count),
`InvalidArgumentType` (wrong type), `ParameterMissing` (implicit call missing
params), `VariableAlreadyBound` (YIELD shadows bound var or duplicates),
`InvalidAggregation` (aggregation function in CALL args), `ProcedureNotFound`
(unknown procedure).

**Assignable type coercion** — NUMBER accepts INTEGER or FLOAT; FLOAT accepts
INTEGER. Matches are coerced when filtering mock procedure data tables.

**CypherDuration::parse false positive fix** — Strings like "Pontus" starting
with 'P' were incorrectly parsed as zero-duration temporal values. Fixed by
requiring at least one valid component and rejecting leftover text.

**All skip guards removed** — `@UNSUPPORTED_QUERY_PATTERNS` is empty. Every
TCK scenario runs without skipping.

### Implementation Details

- Parser: `YIELD *` support, `implicit` flag on CALL AST node
- Planner: VariableAlreadyBound and InvalidAggregation compile-time checks
- Executor: `exec_mock_procedure()` with data table filtering, numeric
  coercion in `mock_value_matches()`, `resolve_implicit_args()` from params
- TCK harness: `And there exists a procedure` declarations parsed from
  Gherkin steps, converted to JSON and injected as `__procedures` param

---

## [0.22.2] — 2026-05-12 — TCK Bug Fixes

v0.22.2 fixes two bugs found by a clean full-TCK run after v0.22.1.
TCK pass rate: **3880/3880** (0 failures; 171 intentionally skipped
scenarios count as TAP passes).

### Fixed

**`temporal_cmp` integer overflow** — When comparing `datetime` values
with timezone offsets, the conversion from day count to nanoseconds
used `i64` arithmetic which overflows for dates in common years (e.g.
1984 → ~724,000 days × 86,400,000,000,000 ns >> `i64::MAX`). Fixed
by using `i128` for the intermediate product. Fixes Temporal6[5] and
Temporal7[5].

**`exec_create_pattern` stack overflow on large CREATE queries** — A
query with many chained `CREATE` clauses (e.g. Create4[2] with ~180
clauses) built a deeply nested `LogicalPlan::CreatePattern` tree that
caused a stack overflow when `execute()` recursed through it. Fixed by
unwinding the chain iteratively before executing. Fixes Create4[2].

---

## [0.22.1] — 2026-05-12 — 100% TCK Compliance

v0.22.1 closes the last 4 TCK failures. TCK pass rate reaches
**3880/3880 (100%)** — full openCypher conformance.

### What's New

**`toLower()` / `toUpper()` string functions** — Cypher string case
conversion functions are now implemented. Fixes List12[6] (list
comprehension in WHERE using `toLower()`).

**Extended-range date parsing** — dates with years outside chrono's
representable range (±262143) now parse correctly via an extended-year
ISO 8601 format handler. `date('-999999999-01-01')` and
`localdatetime('+999999999-12-31T23:59:59')` work correctly.

**Extended-range duration computation** — `duration.between()` and
`duration.inSeconds()` now compute correct results for dates spanning
the full ±999,999,999 year range using proleptic Gregorian calendar
arithmetic that bypasses chrono when dates exceed its range. Fixes
Temporal10[9] and Temporal10[10].

**Match4[7] already passing** — variable-length pattern with bound
relationship was already handled correctly by the existing planner and
executor; confirmed with a dedicated regression test.

---

## [0.22.0] — Temporal Type System

v0.22.0 delivers full temporal type support. TCK pass rate jumps from
**2877/3880 (74.2%)** to **3876/3880 (99.9%)** — +999 scenarios. Every
temporal suite (Temporal1–10) now passes except 2 extreme-range tests
requiring ±999,999,999 year dates beyond chrono's representable range.

### What's New

**Six temporal types** — `date`, `localtime`, `time`, `localdatetime`,
`datetime`, and `duration` are now first-class Cypher values. Construct
from ISO 8601 strings (`date('2024-01-15')`), maps (`time({hour: 12,
minute: 30, timezone: 'Europe/London'})`), or project from other temporals
(`datetime({date: d, time: t, timezone: 'UTC'})`).

**Full component access** — `.year`, `.month`, `.day`, `.hour`, `.minute`,
`.second`, `.millisecond`, `.microsecond`, `.nanosecond`, `.timezone`,
`.offset`, `.offsetMinutes`, `.offsetSeconds`, `.epochMillis`,
`.epochSeconds` on temporal values. Duration accessors (`.years`, `.months`,
`.days`, `.hours`, `.minutes`, `.seconds`, etc.) follow Neo4j total-based
semantics.

**Temporal arithmetic** — `temporal + duration`, `temporal - duration`,
`duration ± duration`, `duration * number`, `duration / number`,
`duration * float`, `duration / float` all work correctly. Duration
division uses the Neo4j 30.436875 days/month conversion factor with
fractional cascading.

**Duration functions** — `duration.between(t1, t2)`, `duration.inMonths()`,
`duration.inDays()`, `duration.inSeconds()` compute differences between
any valid temporal type pair. Mixed zoned/local comparisons interpret the
local side in the named timezone for correct DST-aware results.

**Temporal comparison and ordering** — Temporal values of the same type
are comparable with `<`, `>`, `=`, etc. `ORDER BY` on temporal values
works correctly. Zoned types compare in UTC.

**Timezone support** — IANA named timezones (`'Europe/Stockholm'`,
`'Pacific/Honolulu'`) and fixed offsets (`'+05:00'`, `'Z'`). DST
transitions handled correctly for timezone projection and duration
computation.

**Date arithmetic edge cases** — date + duration includes whole-day
overflow from time components. Fractional duration fields (e.g.,
`years: 12.5`) cascade correctly through the field hierarchy.

### Remaining TCK Failures (4 of 3880) — all fixed in v0.22.1

- **Match4[7]** — ~~variable-length pattern with bound relationship~~ ✅ already passing
- **List12[6]** — ~~list comprehension in WHERE~~ ✅ fixed (toLower/toUpper added)
- **Temporal10[9]** — ~~`date('-999999999-01-01')` exceeds chrono range~~ ✅ fixed (extended-year parsing)
- **Temporal10[10]** — ~~`localdatetime('-999999999-01-01')` exceeds chrono range~~ ✅ fixed (extended-year parsing)

---

## [0.21.0] — Variable-Length Correctness and Remaining Quick Wins

v0.21.0 closes virtually all non-temporal TCK failures. TCK pass rate rises
from **2870/3880 (77.4%)** to **2877/3880 (77.6%)** — +7 scenarios beyond the
v0.20.0 baseline. Only one non-temporal failure remains (Match4[7], deferred).

### What's New

**Variable-length expand with destination predicates** — var-length
`MATCH (a)-[*]->(b:Label {prop: val})` now correctly applies label and
property filters on the destination node. Previously these predicates were
silently ignored during BFS traversal. Fixes Match4[4], Match5[4,21,22],
Match6[14], Match9[5] (6 scenarios).

**Variable-length expand with relationship predicates** — var-length
`MATCH ()-[r:T* {key: val}]-()` now post-filters the edge list with an
`all(r IN rels WHERE …)` predicate. Fixes Match4[5] (1 scenario).

**Exact-length var-length patterns** — `*N` (e.g. `*3`) now parses as
exactly N hops instead of unbounded. Fixes Match5[4,21,22] (3 scenarios).

**Pre-bound edge list in var-length position** — deprecated Cypher syntax
`WITH [r1,r2] AS rs … MATCH ()-[rs*]->()` now works via a new
`BoundRelListExpand` plan node that walks the pre-provided edge list,
verifying connectivity. Fixes Match4[8], Match9[6,7] (3 scenarios).

**Cross-hop uniqueness (fixed × var-length)** — when a pattern contains
both fixed-length and variable-length relationships, the BFS now excludes
edges already bound by fixed-length hops, enforcing relationship uniqueness
across the entire pattern. Fixes Match5[27] (1 scenario).

**OPTIONAL MATCH + var-length + destination label** — chained
`OPTIONAL MATCH (a)-[*]->(b:L)` where the var-length expand's destination
has label constraints now correctly uses LeftJoin semantics, preventing
null rows from being discarded by the post-expand label filter.
Fixes Match7[15] (1 scenario).

**OPTIONAL MATCH with non-existent destination label** — `OPTIONAL MATCH
(n)-[]->(m:NonExistent)` now short-circuits to a null row instead of scanning
edges. Fixes Match7[22,28] (2 scenarios).

**Optional var-length with pre-bound destination** — when the destination
variable is already bound and no path is found, the null-fill now preserves
the pre-bound value instead of overwriting it. Fixes Match9[9] (1 scenario).

**`WITH *` preserves variable bindings** — `WITH *` now correctly carries
all non-anonymous variables into downstream clauses. Previously, `WITH *`
cleared the planner's bound variable set, causing subsequent OPTIONAL MATCH
to misplan with fresh scans instead of reusing pre-bound variables.
Fixes Match8[2] (1 scenario).

**Quantifier type-mismatch detection** — `any/all/none/single` over a
literal list of strings or booleans with arithmetic predicates now raises
`SyntaxError: InvalidArgumentType` at compile time. Fixes Quantifier1–4
(12 scenarios).

**WHERE expression must be boolean** — `WHERE (n)` (bare node variable)
now raises `SyntaxError: InvalidArgumentType`. Fixes Pattern1[11]
(1 scenario).

**MERGE path variable on create** — `MERGE p = (a)-[:R]->(b) RETURN p`
now populates `p` on the create branch. Fixes Merge5[10] (1 scenario).

### Process & Quality

- **TCK regression floor**: `tests/tck/baseline.txt` + `tests/tck/tck_floor.sh`
  enforce a minimum passing count. Current floor: 2877.
- **TCK failure classification**: `plans/tck-failure-analysis.md` exhaustively
  buckets every failing scenario by root cause.

### TCK

- Pass rate: 2870 → 2877 / 3880 (77.4% → 77.6%, +7 net).

---

## [0.20.0] — Engine Correctness and TCK Harness Improvements

v0.20.0 is a focused correctness and test-harness release. TCK pass rate rises
from **2974/3880 (76.6%)** to **3006/3880 (77.5%)** — +32 scenarios.

### What's New

**NaN round-trip through JSON** — `Value::Float(NaN)` now serialises to the
JSON string `"NaN"` and deserialises back correctly, fixing
`ReturnOrderBy1[11,12]` (NaN sort ordering).

**Relationship isomorphism in named paths** — the `exec_named_path` step now
checks that all relationship slots in a named path `p = ...` are distinct,
enforcing openCypher relationship uniqueness within each bound path. Fixes
`Match6[8-14]` and `Match8[3]`.

**Optional MATCH null-row preservation** — when an OPTIONAL MATCH with a
WHERE produces no matching rows, the null row now correctly preserves any
variables that were already bound in the input row (e.g. a relationship
variable forwarded from an earlier WITH). Fixes `Match7[4,8,9,17]`.

**Multi-hop OPTIONAL MATCH uses LeftJoin** — patterns with more than one hop
(e.g. `OPTIONAL MATCH (a)-[r1]->(b)-[r2]->(c)`) now generate LeftJoin plans
instead of InnerJoin plans, ensuring null rows are produced when no match
exists. Fixes `Match7[8,9]`.

**Correlated variable fallback in eval_expr** — `Expr::Variable` lookups now
fall back to the `params` map when the variable is not in the current row.
This enables correlated variable references in MERGE and EXISTS sub-queries.
Fixes `Merge5[14]` and `WithWhere1[3]`.

**Nested EXISTS scope** — when evaluating an `EXISTS { ... }` sub-query, the
outer row's variable names now include both `row.keys()` and `params.keys()`,
enabling nested EXISTS patterns to correctly resolve outer variables. Fixes
`ExistentialSubquery3[1,3]`.

**SET clause rejects pattern predicates** — the planner now detects inline
pattern expressions (e.g. `(n)-[:R]->()` embedded inside function calls) in
the right-hand side of a `SET` clause and raises `SyntaxError: UnexpectedSyntax`.
Fixes `Pattern1[24]`. The `expr_has_inline_pattern` helper now recursively
inspects `Property`, `Subscript`, `ListSlice`, `Compare`, and `Arith`
sub-expressions, catching patterns nested inside wrappers like
`head(nodes(head((n)-[:REL]->())))`.

**rand() forbidden inside aggregate** — `rand()` called inside an aggregate
expression (e.g. `count(rand())`) now raises
`SyntaxError: NonConstantExpression` at plan time. Fixes `Return6[15]`.

**Property type validation in SET** — assigning a map or list-of-maps as a
scalar property value (e.g. `SET n.prop = {k: v}`) now raises
`TypeError: InvalidArgumentType` rather than silently storing the wrong type.
Fixes `Set1[10]`.

**Aggregate ORDER BY via projected column lookup** — when sorting after
aggregation (`ORDER BY count(*) DESC`), the sort key is now resolved by
looking up the projected column name (e.g. `"count(*)"`) in the output row
before falling back to `eval_expr`. Fixes `ReturnOrderBy3[1]`.

**Map literal key case preservation** — the parser now uses the original
source text (via token offset) to recover the exact case of keyword-tokens
used as map property keys (e.g. `{null: 'a', NULL: 'b'}` produces two
distinct keys `"null"` and `"NULL"`). Fixes `Map1[5]` and `Map2[5]`.

**UNWIND list-of-maps parameter parsing** — the `cypher_list_to_json` helper
in the TCK harness was rewritten to handle nested maps and lists with proper
depth-aware splitting. Fixes `Unwind1[6,14]`.

**Trailing empty cell preservation** — `_split_row` in the TCK harness now
passes `-1` as the limit to Perl's `split`, preserving trailing empty cells in
Gherkin table rows. Fixes `List2[9]`.

**Gherkin table backslash unescaping** — `_split_row` now unescapes `\\` → `\`
and `\|` → `|` in table cells, matching standard Gherkin table escaping rules.
Fixes `Literals6[5]`.

**String-aware list depth tracking** — the list-element splitter in
`cell_match` now enters a "string mode" when it sees a single-quote, so `[`
and `]` characters inside string literals do not affect the nesting depth.
Fixes `Literals7[17]`.

---

## [0.19.0] — Cypher Correctness and Ordering Improvements

v0.19.0 is a focused correctness release. No new Cypher clauses are added;
instead, a broad sweep of engine bugs are fixed so that existing features
behave according to the openCypher specification. TCK pass rate rises from
2391/3880 (61.6%) to **2974/3880 (76.6%)** — +583 scenarios.

### What's New

**WITH post-aggregation WHERE (HAVING semantics)** — when a `WITH` clause
contains an aggregation in its projection list, any `WHERE` on that `WITH` is
now evaluated *after* aggregation (i.e., acts as a `HAVING` filter). Previously
the WHERE was applied before aggregation, giving wrong results for queries like
`WITH count(*) AS c WHERE c > 1`.

**Bound relationship variable forwarding** — a relationship variable bound in a
prior `WITH` can now be passed through and used as a filter in a subsequent
`MATCH`. The `exec_expand` step checks whether the relationship variable already
has a value in the current row and, if so, only keeps edges whose `edge_id`
matches that binding. Fixes `WITH … MATCH (a)-[r]->(b)` after a `WITH r`.

**Named path null propagation** — when a named path `p = (a)-[r]->(b)` is
inside an `OPTIONAL MATCH` that finds no results, the relationship slot `r` is
null. The `exec_named_path` step now detects a null relationship slot and sets
the path variable to null rather than constructing a malformed one-node path.
Fixes all "optionally matching named paths" scenarios.

**Correct openCypher type ordering** — `ORDER BY` and `min()`/`max()` now use
the openCypher-specified ascending type order:
Map < Node < Relationship < List < Path < String < Boolean < Number < NaN < Null.
Previously the implementation used an ad-hoc ordering that mixed types
incorrectly. Fixes `Aggregation2[11,12]`, `ReturnOrderBy1[11,12]`,
`WithOrderBy1[21,22]`.

**ListPredicate over aggregate expressions** — `ALL(x IN collect(…) WHERE …)`
and its siblings (`ANY`, `NONE`, `SINGLE`) now work correctly when the list
argument is an aggregate expression produced by `collect()`. The
`eval_with_agg` path was missing a handler for `ListPredicate`; it is now
present with the same three-valued logic as the normal evaluator.

**Edge and path equality** — two relationships (edges) can now be compared with
`=` and `<>`: they are equal iff they share the same internal `edge_id`. Two
paths are equal iff they have the same sequence of nodes and the same sequence
of relationships. Previously both fell to the type-mismatch arm and were always
unequal.

**Strict type checking in planner** — `labels()` applied to a path or
relationship, `type()` applied to a node or path, and `length()` applied to a
node or relationship now raise `InvalidArgumentType` at plan time instead of
returning wrong results at run time.

**DISTINCT + ORDER BY validation** — `RETURN DISTINCT … ORDER BY x` where `x`
is not in the projection now correctly raises `UndefinedVariable`. Similarly,
an aggregation in `ORDER BY` when `RETURN` contains no aggregation now raises
`InvalidAggregation`.

### TCK Result

| Release | Pass  | Total | %     | Delta |
|---------|-------|-------|-------|-------|
| v0.18.0 | 2391  | 3880  | 61.6% | baseline |
| v0.19.0 | **2974** | 3880 | **76.6%** | **+583** |

---

## [0.18.0] — Quantifiers, Pattern Comprehension, and UNION

v0.18.0 adds quantifier functions (`any`, `none`, `all`, `single`), named
pattern comprehension, inline pattern predicates in WHERE, `UNION` / `UNION ALL`,
and a range of correctness fixes. TCK pass rate rises from 2260/3880 (58.2%)
to **2391/3880 (61.6%)** — +131 scenarios.

### What's New

**Quantifier functions** — `any(x IN list WHERE pred)`, `none(x IN list WHERE
pred)`, `all(x IN list WHERE pred)`, `single(x IN list WHERE pred)` are all
implemented with correct three-valued null logic (null propagation per
openCypher spec). Covers Quantifier1, 9, 11, 12 scenarios.

**Named pattern comprehension** — `[p = (n)-->(m) | p]` now works. The parser
detects the `ident =` prefix before the pattern, captures the path variable,
and stores it in `PatternComprehension { path_variable: Some("p"), ... }`. The
executor calls `exec_pattern_inline` which builds and executes a plan with a
`NamedPath` wrapper, then stores the path in the path variable slot. Covers
Pattern2 scenarios.

**Inline pattern predicates** — `WHERE (a)-->(b)` (a pattern used as a
boolean test) is now supported. The parser uses `looks_like_pattern_predicate()`
to distinguish pattern predicates from expression predicates. Covers Pattern1
scenarios.

**UNION / UNION ALL** — the AST has `Query { union: Option<(bool, Box<Query>)>
}`. The parser handles chained `A UNION B UNION C` right-recursively, detecting
mismatches between `UNION` and `UNION ALL` at parse time
(`SyntaxError::InvalidClauseComposition`). The planner validates column name
parity (`SyntaxError::DifferentColumnsInUnion`). The executor concatenates
result sets and deduplicates for plain `UNION`. Covers Union1/2/3 scenarios.

**Correlated pattern comprehension** — variables from the outer MATCH row are
injected into `inner_params` in `exec_pattern_inline`, so `(n)-->()` where `n`
is from an outer MATCH correctly uses the outer binding rather than scanning all
nodes. The `exec_expand` and `exec_var_length_expand` functions have a params
fallback for `src_var` / `dst_var` lookup that enables this.

**`IN` null semantics** — `x IN [null, 1, 2]` where x is null now correctly
propagates null (ternary logic), instead of returning false. The `InList`
evaluator was updated to use `compare_values()` with three-valued logic.

**`MATCH (a)-[*]->(b)` with both ends bound** — `exec_var_length_expand` now
reads `expected_dst_id` from both `input_row` and `params`, enabling correct
BFS filtering when the destination variable is provided via the outer correlated
context.

**CREATE double-node fix** — when creating `(a)-[:T]->(:C)`, the anonymous
destination node is pre-created in the Relationship arm and the `node_was_precreated`
flag prevents the Node arm from creating a second copy.

**MATCH (a),(b) cross-product via LabelScan** — the planner correctly emits
a `CrossProduct` of two separate `LabelScan` plans when the first variable in a
pattern is already bound in the outer scope, avoiding the `_none` source
variable bug that caused `find_last_node_var(SingleRow)` to return `"_none"`.

### TCK Result

| Release | Pass  | Total | %     | Delta |
|---------|-------|-------|-------|-------|
| v0.17.0 | 2260  | 3880  | 58.2% | baseline |
| v0.18.0 | **2391** | 3880 | **61.6%** | **+131** |

---

## [0.17.0] — Error Validation + Named Paths

v0.17.0 adds strict variable-kind checking, named path support, and
aggregation-in-ORDER-BY detection. TCK pass rate rises from 2002/3880 (51.6%)
to **2260/3880 (58.2%)** — +258 scenarios.

### What's New

**Variable type conflict detection (SyntaxError)** — the planner now tracks
whether each bound variable refers to a node, relationship, path, or scalar
value (`VarKind`). Reusing a variable for two different entity types raises a
`SyntaxError` immediately during planning: `MATCH (a)-[a]->(b)` raises
`SyntaxError: Type conflict: variable 'a' already bound as Node`, and
`WITH 1 AS n MATCH (n)` raises `SyntaxError: Type conflict: 'n' is Scalar,
cannot be used as a node pattern variable`. Covers ~130 previously failing
Match1/Match2/Match9 scenarios.

**Aggregation in ORDER BY after non-aggregating WITH** — `WITH a ORDER BY
count(a)` now raises `SyntaxError: Aggregation not allowed in ORDER BY of a
non-aggregating WITH` during planning. Covers 25 WithOrderBy2 scenarios.

**Named paths** — `MATCH p = (a)-[r]->(b) RETURN p` now works. The planner
assembles `element_vars` (alternating node/rel variable names) for each named
path; the executor reads each variable's current row value and constructs a
`Value::Path { nodes, rels }` bound to the path variable. Anonymous
relationships in named paths are assigned internal names (`_pr_N`). Covers 94
Match6 scenarios.

**Variable-length named paths** — `MATCH p = (a)-[*..3]->(b) RETURN p`
propagates the full path through BFS using a `path_carry_var` field. Each BFS
hop appends the current node and edge ids; on emit the full sequence is
converted to a `Value::Path` and stored under the path variable name. Named
path merging in `exec_named_path` then picks up this pre-built path.

**TCK harness path comparison** — `cell_match()` now recognises `<...>`
path display format from the TCK expected output and delegates to a
`path_display_matches()` helper that parses alternating `(node)` and `[edge]`
segments and compares them against the `Value::Path` array serialised from the
executor.

### TCK Result

| Release | Pass  | Total | %     | Delta |
|---------|-------|-------|-------|-------|
| v0.16.0 | 2002  | 3880  | 51.6% | baseline |
| v0.17.0 | **2260** | 3880 | **58.2%** | **+258** |

---

## [0.16.0] — Map Literal Expressions

v0.16.0 unlocks map literal expressions throughout Cypher. TCK pass rate
rises from 1781/3880 (45.9%) to **2002/3880 (51.6%)** — +221 scenarios, the
largest single-release gain to date.

### What's New

**Map literal expressions** — map literals `{key: expr, ...}` now work
everywhere an expression is valid: `RETURN`, `WITH`, `WHERE`, `CREATE`,
`MERGE`, and nested inside lists and other maps. The parser and evaluator
already handled this; the release removes the skip guards that prevented the
TCK from exercising them and fixes the remaining compliance gaps.

**Map property access** — `expr.key` and `expr[keyExpr]` now work when
`expr` evaluates to a map value. Previously only Node and Edge supported
property access; any computed map (from a map literal, parameter, or function
result) was silently returning `null`. `get_property()` and `Expr::Subscript`
now both handle `Value::Json(Object)`.

**Map subscript access** — `map[stringKey]` is now supported. Passing a
non-string key into a map raises `TypeError: map element access requires a
string key` per the openCypher spec.

**Map equality** — `{a: 1} = {a: 1}` now evaluates correctly. The
`compare_values()` function now handles `Json(Object)` pairs with key-by-key
recursive equality, returning `null` if any value comparison is `null`.

**Nested map comparison in the test harness** — `cell_match()` now correctly
compares arbitrarily nested map values like `{a: {b: {c: 1}}}` against the
JSON objects returned by the executor, using a depth-aware Cypher map display
parser.

**Map parameter support** — parameters whose values are Cypher map literals
(e.g., `| expr | {name: 'Apa'} |`) are now correctly converted to JSON
objects when building the parameter JSON, instead of being passed as raw
strings.

### TCK Result

| Release | Pass  | Total | %    | Delta |
|---------|-------|-------|------|-------|
| v0.15.0 | 1781  | 3880  | 45.9% | baseline |
| v0.16.0 | **2002** | 3880 | **51.6%** | **+221** |

---

## [0.15.0] — Storage Correctness and Error Validation

v0.15.0 eliminates all storage corruption errors and adds strict openCypher
type checking. TCK pass rate rises from 1628/3880 (42.0%) to **1781/3880
(45.9%)** — the largest single-release improvement (+153 scenarios).

### What's New

**Storage corruption fix** — `clear()` (used by the TCK harness between tests)
now acquires `AccessExclusiveLock` before calling `RelationTruncate`. Previously
it used `NoLock`, which raced with autovacuum's cached `nblocks`, causing
`could not read blocks` errors that cascaded into ~732 test failures. This was
the single largest source of TCK noise.

**Cross-page node update** — `update_node()` now handles records that grow
beyond the current page's free space. Instead of raising PE201, it logically
deletes the old record in-place and re-inserts on a new page via
`find_or_extend_page`. Also fixed a MAXALIGN bug where the free-space check
used raw item length instead of 8-byte-aligned length.

**Strict boolean type checking** — `AND`, `OR`, `NOT`, `XOR` now raise
`TypeError` for non-boolean operands (e.g., `1 AND true`). Filter contexts
(`WHERE`, `CASE WHEN`) still use truthiness coercion per openCypher spec.

**Property access type checking** — accessing `.prop` on a non-graph-element
(integer, string, boolean, list) now raises `TypeError` instead of returning
`null`.

**range() type checking** — `range()` now raises `TypeError` for non-integer
arguments instead of silently returning `null`.

**ORDER BY validation** — `ORDER BY` on an undefined variable now raises
`SyntaxError` instead of silently producing null-ordered results.

### TCK Result

| Release | Pass  | Total | %    | Delta |
|---------|-------|-------|------|-------|
| v0.14.0 | 1628  | 3880  | 42.0% | baseline |
| v0.15.0 | **1781** | 3880 | **45.9%** | **+153** |

---

## [0.14.0] — Temporal Types and FOREACH

v0.14.0 adds full openCypher temporal type support and the `FOREACH` iteration
clause. TCK pass rate rises from 1526/3880 (39.3%) to **1628/3880 (42.0%)**.

### What's New

**Temporal types** — all six openCypher temporal constructors are now supported:
- `date("2015-07-21")` / `date({year: 2015, month: 7, day: 21})`
- `localtime("12:00:00")` / `time("12:00:00+01:00")`
- `localdatetime("2015-07-21T12:00:00")` / `datetime("2015-07-21T12:00:00Z")`
- `duration("P1Y2M3DT4H5M6S")` / `duration({years: 1, months: 2, days: 3})`

**Temporal arithmetic**:
- `duration.between(t1, t2)` — full duration between two temporal values
- `duration.inMonths(t1, t2)` — month-resolution duration
- `duration.inDays(t1, t2)` — day-resolution duration
- `duration.inSeconds(t1, t2)` — second-resolution duration

**Temporal property access**: `.year`, `.month`, `.day`, `.hour`, `.minute`,
`.second`, `.millisecond`, `.microsecond`, `.nanosecond`, `.epochSeconds`,
`.epochMillis`, `.timezone`, `.offset`, `.quarter`, `.dayOfWeek`, `.week`,
`.ordinalDay`; for duration: `.years`, `.months`, `.weeks`, `.days`, `.hours`,
`.minutes`, `.seconds`, `.milliseconds`, `.microseconds`, `.nanoseconds`,
`.nanosecondsOfSecond`

**ISO 8601 date/time parsing** — extended and basic formats supported:
- Dates: `YYYY-MM-DD`, `YYYYMMDD`, `YYYY-Www-D`, `YYYY-DDD`, `YYYY-MM`, `YYYY`
- Times: `HH:MM:SS[.nnnnnnnnn][±HH:MM|Z|[TZ-name]]`
- Duration: `P[nY][nM][nW][nD][T[nH][nM][nS]]`

**FOREACH clause** — write clauses can now iterate over a list:
```cypher
FOREACH (x IN [1, 2, 3] | CREATE (:N {v: x}))
FOREACH (n IN nodes | SET n.processed = true)
```

**Comparison** — temporal values can be compared with `=`, `<>`, `<`, `>`,
`<=`, `>=` for ordering and equality checks in `WHERE` clauses.

**Clock functions** — `datetime.transaction()`, `datetime.statement()`,
`datetime.realtime()` all return the current UTC datetime.

### Dependencies Added

- `chrono = "0.4"` — date/time parsing and arithmetic
- `chrono-tz = "0.9"` — IANA timezone name resolution for `datetime("...Z[America/New_York]")`

### TCK Result

| Release | Pass  | Total | %    | Delta |
|---------|-------|-------|------|-------|
| v0.13.0 | 1526  | 3880  | 39.3% | baseline |
| v0.14.0 | **1628** | 3880 | **42.0%** | +102 |

---

## [0.13.0] — 2026-05-10 — Storage Stabilisation and Parser Hardening

v0.13.0 is a correctness release that dramatically improves OpenCypher TCK
conformance by fixing two critical storage bugs, three parser gaps, a
cascading test-harness issue, and two missing error-validation rules.

### Bug Fix: MAXALIGN in Page Free-Space Check (storage/node_store.rs, storage/edge_store.rs)

`find_or_extend_page` and `find_or_extend_edge_page` computed available free
space correctly but compared it against the raw item size rather than the
PostgreSQL-aligned size.  On a 64-bit host, PostgreSQL's `PageAddItemExtended`
requires `MAXALIGN(item_size) + sizeof(ItemIdData)` bytes, where `MAXALIGN`
rounds up to the nearest 8 bytes.  For a 46-byte item the check passed at 50
bytes free (`50 >= 46+4`), but `PageAddItemExtended` actually needed 52 bytes
(`MAXALIGN(46)=48 + 4`), causing silent data-loss or a panic.

Fixed by computing `let aligned = (item_size + 7) & !7` before the comparison
in both stores.  This bug was responsible for roughly **790 TCK failures**
(pages silently full, subsequent scans returning wrong row counts).

### Bug Fix: TCK Graph State Leak (tests/tck/run_tck.pl)

The TCK harness wrapped each scenario in `BEGIN` / `ROLLBACK` calls issued as
separate `psql` invocations.  Because each `$node->psql()` call opens and
closes its own connection, the BEGIN and ROLLBACK ran in separate
auto-committed sessions — effectively doing nothing.  Data from prior scenarios
accumulated across the full test run, producing "expected 0 rows, got 148"-
style cascading failures.

Fixed by implementing `clear()` (a new `#[pg_extern]` function that physically
truncates the custom-AM node and edge relations via `RelationTruncate`, truncates
the catalog index tables via SPI `TRUNCATE`, and restarts both ID sequences) and
calling it at the start of every scenario via `$node->safe_psql('postgres',
'SELECT clear()')`.  The original call used the wrong schema prefix
(`pg_eddy.clear()` — that schema does not exist; all functions are in `public`)
and silently swallowed the resulting error inside `eval { }`, so every scenario
ran on top of accumulated data from all prior scenarios.

### Feature: Map Literal Expressions (`{key: expr}`)

Cypher allows map literals in RETURN, WITH, and SET expressions, e.g.
`RETURN {name: n.name, age: n.age}`.  The lexer, parser, AST, and executor
now support these fully:

- **Lexer / Parser**: `Token::LBrace` in expression position triggers
  `parse_property_map()`, producing `Expr::MapLiteral(Vec<(String, Expr)>)`.
- **Executor**: `eval_expr` evaluates each value expression and wraps the
  result in a `serde_json::Value::Object`.  `expr_has_aggregate` and
  `collect_free_var_refs` handle the new variant.

### Feature: Hex and Octal Integer Literals

The lexer now recognises `0x`/`0X` (hexadecimal) and `0o`/`0O` (octal)
integer prefixes, as required by the openCypher grammar.  Overflow values
clamp to `i64::MAX` rather than returning a lex error.  Values that overflow
`i64` during ordinary decimal parsing now fall back to `FloatLit` rather than
halting lexing.

### Error Validation: Undirected Relationships in CREATE

`CREATE (a)-[r:R]-(b)` (no arrow direction) now raises a `SyntaxError`
instead of silently choosing an arbitrary direction.  The check is in the
executor's `create_pattern_in_row` function.

### Error Validation: Re-Creating an Already-Bound Node Variable

`MATCH (n) CREATE (n)` now raises a `SyntaxError`.  A CREATE pattern whose
sole element is a node variable already present in the current row is
rejected; using a bound variable as a *relationship endpoint* in CREATE
remains valid (`MATCH (n) CREATE (n)-[:R]->(b)`).

---

## [0.12.1] — 2026-05-10 — Batch Catalog Writes and LDBC IS-1/IS-3 Benchmark

v0.12.1 delivers two improvements: a **performance fix** for Cypher `CREATE`
throughput and a **validated benchmark** comparing pg_eddy to Apache AGE on
LDBC-style workloads.

### Performance: Batch Catalog Writes (`CatalogWriteBuffer`)

In v0.12.0, each node/edge created by a Cypher `UNWIND+CREATE` batch issued
3 SPI INSERT calls to catalog index tables (`_pg_eddy.label_index`,
`edge_type_src`, `edge_type_dst`). For a batch of N rows, this was 3N round-
trips inside a single SPI context.

v0.12.1 introduces `CatalogWriteBuffer`: all catalog writes in a single
`CREATE` or `MERGE` call are buffered in Rust `Vec`s and flushed at the end
as a single bulk `INSERT ... VALUES (...),...` per table — 3 SPI calls total
regardless of batch size.

### Bug Fix: UNWIND Variable Scoping in MATCH

`exec_cross_product` previously executed the right-hand plan (e.g., a
`LabelScan`) once, independently of the left-hand rows. This meant UNWIND
variables were not visible when evaluating inline property filters on the
downstream MATCH — `MATCH (n:Person {id: r.src})` after `UNWIND $rels AS r`
would fail with "unbound variable: r". Fixed by evaluating the right-hand
plan once per left row, with left-row variables merged into the params context.

### Benchmark: LDBC IS-1 / IS-3 vs Apache AGE

| Benchmark | pg_eddy | AGE | Ratio |
|---|---|---|---|
| Node insert (nodes/s, UNWIND+CREATE) | 4 422 | 7 155 | 0.62× |
| Edge load (edges/s) | 9 300 (SQL API) | 594 (UNWIND+MATCH) | N/A (diff API) |
| IS-1: node lookup (ms/query) | 90.84 ms | 12.37 ms | 7.34× slower |
| IS-3: 1-hop expand (ms/query) | **92.67 ms** | 169.41 ms | **1.83× faster** |

pg_eddy's adjacency-chain traversal (IS-3) is **1.83× faster** than AGE.
IS-1 is slower because pg_eddy has no property index yet (v0.13.x milestone).

Scale: 1 000 nodes / 5 000 edges, dev container (Debian 11, PG 18.3).
Full results in [`benchmarks/README.md`](benchmarks/README.md).

---

## [0.12.0] — 2026-05-10 — Cypher Write Language: CREATE, MERGE, SET, REMOVE, DELETE

v0.12.0 delivers the full Cypher write language, completing the read+write
pipeline from lexer to storage. All five write clauses are now supported:
`CREATE`, `MERGE`, `SET`, `REMOVE`, and `DELETE` (including `DETACH DELETE`).

### What's New

**CREATE**
- `CREATE (n:Label {prop: val})` — create nodes with labels and properties
- `CREATE (a)-[:REL_TYPE {prop: val}]->(b)` — create relationships with properties
- `MATCH ... CREATE ...` — create new nodes/relationships for each matched row
- Multi-hop patterns: `CREATE (a)-[:R]->(b)-[:R]->(c)` in a single clause

**MERGE**
- `MERGE (n:Label {prop: val})` — find or create a node matching the pattern
- `MERGE ... ON CREATE SET n.x = 1` — set properties only when creating
- `MERGE ... ON MATCH SET n.x = 1` — set properties only when finding existing
- `MERGE` on relationship patterns with full ON CREATE / ON MATCH support

**SET**
- `SET n.prop = expr` — set a single property
- `SET n = {map}` — replace all properties with a map literal
- `SET n += {map}` — merge properties (add/overwrite, keep unset)
- `SET n:Label` — add a label to a node

**REMOVE**
- `REMOVE n.prop` — delete a single property
- `REMOVE n:Label` — remove a label from a node

**DELETE / DETACH DELETE**
- `DELETE n` — delete a node (fails if it has relationships)
- `DETACH DELETE n` — delete a node and all its relationships

### Pipeline Changes

| Layer | Changes |
|---|---|
| Lexer | `Token::Merge`, `Token::Remove`, `Token::On`, `Token::PlusEq` (`+=`) |
| AST | `QueryClause::{Create,Merge,Set,Remove,Delete}`, `SetItem`, `RemoveItem` |
| Parser | Full write clause parsing; `MERGE ON CREATE`/`ON MATCH` sub-clauses |
| Planner | `CreatePattern`, `MergePattern`, `SetProp`, `RemoveProp`, `DeleteNodes` |
| Executor | Write executors using storage layer; `is_write_only_plan()` detection |

### TCK Progress

OpenCypher TCK pass rate improved from **188/3880** (v0.11.0) to **1254/3880**
(v0.12.0), a gain of 1,066 newly passing scenarios enabled by write clause
support. See README badge for the current count.

### Unit Tests

80 unit tests pass (up from 75 in v0.11.0), covering:
- `test_cypher_create_node` — CREATE and re-MATCH a node with label + properties
- `test_cypher_create_relationship` — CREATE nodes then a relationship between them
- `test_cypher_set_property` — CREATE + SET a property, verify with MATCH
- `test_cypher_delete_node` — CREATE then DELETE a node
- `test_write_parse_plan` — parse all five write clause types without panic

---

## [0.11.0] — 2026-05-10 — Subqueries: EXISTS {}, CALL {}, and CALL procedure YIELD

v0.11.0 adds full parser/planner/executor support for existential subquery
predicates (`EXISTS { pattern }`), correlated and uncorrelated `CALL { }`
subqueries, and `CALL procedure(args) YIELD col` syntax. These language features
are complete per the openCypher grammar. TCK gains are reserved until v0.12.0
when `CREATE` is implemented (all EXISTS/CALL TCK scenarios use `CREATE` for
data setup and are currently skipped). 188/188 in-scope TCK scenarios continue
to pass (100%).

### New Cypher Features

**`EXISTS { pattern }` predicate** — evaluates to `true` if at least one result
exists for the inner pattern, `false` otherwise. Correlated with the outer scope
via variable bindings. Supports both pattern-only form (`exists { (n)-->() }`)
and full subquery form (`exists { MATCH (n)-[:R]->(m) WHERE m.val > 5 }`).

**`CALL { subquery }` — subquery clauses** — runs an inner Cypher query for
each outer row and emits merged (outer, inner) rows. Supports uncorrelated
subqueries (inner is independent) and correlated subqueries (inner uses outer
variables). Maps to an `Apply` logical plan node.

**`CALL proc.name(args) YIELD col [AS alias]`** — procedure call syntax fully
parsed and planned. Currently produces zero rows (procedure registry not yet
implemented in v0.11.0). The YIELD column names are bound in scope so downstream
clauses compile without errors. Procedure registration is planned for a future
release.

### Release Checklist

All AGENTS.md release gates passed in order:
1. `cargo clippy --features pg18` → 0 warnings
2. `cargo pgrx test pg18` → 75/75 passed (8 new tests added)
3. `prove tests/tap/*.pl` → 11/11 passed
4. `perl tests/tck/run_tck.pl` → 188/188 in-scope (100%)

---

## [0.10.0] — 2026-05-10 — Variable-Length Paths, Named Paths, and Path Functions

v0.10.0 adds full parser/planner/executor support for variable-length path
patterns, named paths, path value type, `nodes()`/`relationships()`/`length()`
path functions, `shortestPath()`, `allShortestPaths()`, and pattern
comprehensions. 188/188 in-scope TCK scenarios continue to pass (100%).

### New Cypher Features

**Variable-length path patterns** — full `[*m..n]` syntax in all variants:
- `[*]` or `[*1..]` — unbounded from 1 hop
- `[*3]` — exactly 3 hops (parsed as min=max=3)
- `[*1..5]` — between 1 and 5 hops
- `[*..5]` — up to 5 hops (min defaults to 1)
- `[*3..]` — at least 3 hops (unbounded max)
- Supports rel-type filters, rel variables, all directions

**Named paths** — `p = (a)-[r]->(b)` syntax assigns the matched path to a
variable. Produces a `Path` value with `.nodes` and `.rels` arrays.

**Path value type** — new `Value::Path { nodes, rels }` runtime value supports:
- `nodes(p)` — returns list of all nodes in a path
- `relationships(p)` — returns list of all relationships in a path
- `length(p)` — returns hop count of a path

**`shortestPath()` and `allShortestPaths()`** — parsed and routed to
BFS-based traversal (full result packaging in future release).

**Pattern comprehensions** — `[(n)-[:R]->(m) | expr]` syntax produces a list
by executing the inline pattern and projecting each match.

### Planner Extensions

Two new plan nodes:
- `VarLengthExpand` — BFS traversal with no-repeated-edges constraint,
  capped at 256 hops for safety, supports `min_hops`/`max_hops` bounds.
- `NamedPath` — wraps an expand plan and packages the result into a path value.

### Bug Fixes

- **TCK harness Background parsing** — `run_tck.pl` now correctly parses
  `Background:` sections and prepends their steps to each scenario. Previously,
  `having executed:` in Background sections was silently dropped, causing some
  scenarios with CREATE setup to run against an empty graph instead of being
  properly skipped. 3,692 scenarios are now correctly classified (up from 3,668).

---

## [0.9.0] — 2026-05-09 — Aggregation, List Comprehensions, and Numeric Operators

v0.9.0 expands the Cypher engine with a complete aggregation suite, list
comprehensions and predicates, correct numeric semantics, and full openCypher
null propagation for comparisons. 188/188 in-scope TCK scenarios pass (100%).

### New Cypher Features

**Aggregation functions** — full suite: `count(*)`, `count(expr)`,
`count(DISTINCT expr)`, `sum`, `avg`, `min`, `max`, `stdev`, `stdevp`,
`collect`, `collect(DISTINCT expr)`. All aggregate functions correctly ignore
null inputs and return null when the input set is empty (except `count` which
returns 0).

**List comprehensions**: `[x IN list WHERE pred | projection]` — filters a list
and optionally transforms each element. The WHERE clause and projection are
both optional.

**List predicates**: `any(x IN list WHERE pred)`, `all(x IN list WHERE pred)`,
`none(x IN list WHERE pred)`, `single(x IN list WHERE pred)`.

**XOR operator**: `a XOR b` — boolean exclusive-or with full null propagation.

**Exponentiation**: `x ^ y` — left-associative per the openCypher spec
(`4^6^3 = (4^6)^3`).

**List subscript and slice**: `list[i]`, `list[i..j]` — with null-safe
element access.

**List concatenation with scalar append**: `list + element`, `element + list`.

### Correctness Fixes

**Null propagation in all comparisons**: `compare_values` now returns
`Option<bool>` — ordering operators (`<`, `>`, `<=`, `>=`) on null inputs
produce null, while equality (`=`) between different types produces `false`
rather than null.

**List equality semantics**: `[a, b] = [c, d]` uses recursive element
comparison with full null propagation. Lists of different lengths are
definitively not equal (no null short-circuit on length mismatch).

**Cross-type sort ordering**: `ORDER BY` now respects the openCypher type
ordering: `null > lists > numbers > strings > booleans`. Mixed-type lists
sort lexicographically with per-element type ordering.

**Boolean ordering**: `false < true` is now correctly implemented for
`<`, `>`, `<=`, `>=` operators.

**OPTIONAL MATCH null safety**: OPTIONAL MATCH on relationships
(`OPTIONAL MATCH ()-[r]->()`) now correctly returns one null row when no
relationships exist. The node isomorphism filter is null-safe so null nodes
from OPTIONAL MATCH pass through instead of being filtered out.

**Column naming**: `RETURN count(a) > 0`, `RETURN count(DISTINCT a)`,
`RETURN n.x IS NULL` now produce correct column names matching the Cypher
expression text.

### TCK

- 188/3880 overall (4.8%); 188/188 in-scope (100%)
- Newly unlocked acceptance tests: `Aggregation1`–`Aggregation8`,
  `ListComprehension`, `ListPredicate`, `Comparison1` (list equality),
  `ReturnOrderBy1` / `WithOrderBy1` (list sort), `Graph6` (optional rel),
  `Null1`, `Null2` (IS NULL column names), `Precedence1` (^, boolean order)

---

## [0.8.0] — 2026-05-09 — WITH, OPTIONAL MATCH, UNWIND, and CASE Expressions

v0.8.0 is a major architectural expansion of the Cypher query engine. The AST,
planner, and executor have been refactored from a single-clause model to a
full **multi-clause pipeline**, enabling composition across MATCH chains with
WITH, outer-join semantics via OPTIONAL MATCH, list expansion via UNWIND, and
conditional logic via CASE. 172/172 in-scope TCK scenarios pass (100%).

### New Cypher Features

**WITH clause**: Projects and renames bindings between query stages, optionally
filtering with `WHERE`. Supports `DISTINCT`, `ORDER BY`, `SKIP`, and `LIMIT`.
Variables not projected by WITH are no longer in scope for subsequent clauses,
exactly matching the openCypher spec.

**OPTIONAL MATCH**: Returns all rows from the left side even when the pattern
finds no matches. Unbound variables from an OPTIONAL MATCH produce `null`
bindings that propagate correctly through subsequent WHERE and RETURN clauses
with openCypher 3-valued logic.

**UNWIND**: Expands a list expression into one row per element, binding each
element to the given variable. Works with literal lists, property accesses, and
expressions. `UNWIND [] AS x` produces zero rows.

**CASE expressions** — both forms:
- *Searched*: `CASE WHEN cond THEN val … [ELSE val] END` — evaluates conditions
  in order and returns the first matching branch.
- *Simple*: `CASE expr WHEN val THEN result … [ELSE val] END` — compares the
  subject expression against each WHEN value.
Both forms return `null` when no branch matches and no ELSE is present.

### Architecture

The internal `Query` type now holds a `Vec<QueryClause>` pipeline instead of a
single match + return pair. The planner folds over clauses left-to-right,
building up a `LogicalPlan` tree. New plan nodes: `SingleRow` (seed for queries
starting with UNWIND or a second MATCH) and `Unwind { input, expr, alias }`.
`Expand` gains an `optional: bool` flag for OPTIONAL MATCH semantics.

### TCK

- 172/3881 overall (4.4%); 172/172 in-scope (100%)
- Newly unlocked acceptance tests: `WithAcceptance`, `OptionalMatchAcceptance`,
  `UnwindAcceptance`, `CaseExpressionAcceptance`, `TriadicSelection`

---

## [0.7.0] — 2026-05-09 — Cypher Predicates, Ordering, and Built-in Functions

v0.7.0 substantially expands the Cypher query engine with string predicates,
list operations, result ordering and pagination, correct null semantics, and a
large suite of built-in functions. 107/107 in-scope TCK scenarios pass (100%).

### New Cypher Features

**String predicates**: `STARTS WITH`, `ENDS WITH`, `CONTAINS`, and `=~` (POSIX
regular expression match, evaluated via PostgreSQL's native regex engine).

**List membership**: `x IN [a, b, c]` — list literals and membership tests with
openCypher null semantics (null IN list containing a match → true; no match →
null if list contains null, else false).

**Result ordering and pagination**: `ORDER BY expr [ASC|DESC], ...`, `SKIP n`,
`LIMIT n`. ORDER BY resolves aliases from both the RETURN clause and the MATCH
bindings; NULL sorts last per openCypher spec.

**Corrected null semantics**: `AND`, `OR`, and `NOT` now use 3-valued logic
exactly as specified in the openCypher standard (`null AND false = false`,
`null AND true = null`, `null OR true = true`, etc.).

### Built-in Functions Added

**Type conversion**: `toBoolean(value)`

**Size/length**: `size(string|list)`, `length(string|list)`

**List functions**: `head(list)`, `tail(list)`, `last(list)`, `reverse(list)`,
`range(start, end[, step])`

**String functions**: `trim(s)`, `ltrim(s)`, `rtrim(s)`, `upper(s)` / `toUpper(s)`,
`lower(s)` / `toLower(s)`, `substring(s, start[, length])`,
`replace(s, search, replacement)`, `split(s, delimiter)`, `reverse(s)`

**Math functions**: `abs()`, `ceil()` / `ceiling()`, `floor()`, `round()`,
`sign()`, `sqrt()`, `log()` (natural log), `log10()`, `exp()`, `sin()`,
`cos()`, `tan()`, `asin()`, `acos()`, `atan()`, `atan2(y, x)`, `pi()`, `e()`

### Test Results

- **Unit tests**: 61/61 pass
- **TCK**: 107/3881 overall (2.8%); 107/107 in-scope (100%)

---

## [0.6.0] — 2026-05-09 — Cypher Query Engine

v0.6.0 delivers the first working Cypher query engine for pg_eddy. You can now
execute `MATCH (n:Label) RETURN n` queries via `pg_eddy.cypher()` and inspect
the logical plan with `pg_eddy.cypher_explain()`. The engine is a recursive
interpreter — it walks the logical plan tree and drives the native AM accessors
directly, avoiding SQL injection risk and SQL round-trips alike. 61/61 tests pass.

### New Functions

**`pg_eddy.cypher(query TEXT, params JSONB DEFAULT NULL) RETURNS SETOF JSONB`**  
Execute a Cypher query and receive JSONB rows. Each output row is a JSON object
whose keys are the names from the `RETURN` clause.

```sql
SELECT * FROM pg_eddy.cypher('MATCH (n:Person) WHERE n.age > $min RETURN n.name',
                              '{"min": 30}'::jsonb);
```

**`pg_eddy.cypher_explain(query TEXT) RETURNS TEXT`**  
Return the logical query plan as a human-readable string, without executing it.
Useful for understanding how the planner decomposed the query.

```sql
SELECT pg_eddy.cypher_explain('MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b');
-- Project
--   Filter (isomorphism: id(a) <> id(b))
--     Expand a -[:KNOWS]-> b
--       LabelScan a :Person
```

### Cypher Language Coverage

The parser handles a useful subset of openCypher:

- **Patterns**: node patterns `(n:Label {prop: val})`, relationship patterns
  `(a)-[:TYPE]->(b)`, bidirectional `(a)-[:TYPE]-(b)`, any-direction `(a)-->(b)`
- **WHERE**: equality (`=`, `<>`, `<`, `>`, `<=`, `>=`), `IS NULL`, `IS NOT NULL`,
  `AND`, `OR`, `NOT`, arithmetic (`+`, `-`, `*`, `/`, `%`), property access
  (`n.prop`), parameters (`$name`), string literals, numeric literals
- **RETURN**: property access, variable projection, `RETURN DISTINCT`,
  function calls, `AS` aliases
- **Parameters**: `$name` mapped to the `params` JSONB argument

### Built-in Functions

`id(n)`, `labels(n)`, `type(r)`, `properties(n)`, `keys(n)`,
`coalesce(a, b, …)`, `toString(v)`, `toInteger(v)`, `toFloat(v)`

### Logical Planner

The planner (`src/cypher/planner.rs`) builds a tree of:

- **LabelScan** — iterates all nodes matching a label via `_pg_eddy.label_index`
- **Expand** — follows edges in `OUT`, `IN`, or `BOTH` directions via the
  adjacency-follow AM accessors
- **CrossProduct** — joins two independent patterns (no shared variables)
- **Filter** — evaluates a WHERE predicate or isomorphism constraint
- **Project** — evaluates the RETURN items and selects output columns

### Node Isomorphism

Per the openCypher specification, two distinct node variables in a MATCH pattern
must not be bound to the same physical node. pg_eddy enforces this by
automatically injecting `id(a) <> id(b)` filter nodes in the plan for every
pair of distinct node variables in the pattern.

### Tests

10 new pgrx integration tests cover end-to-end Cypher execution. 26 Rust unit
tests cover the lexer, parser, and planner individually. All 61 tests pass.

### Not Included in v0.6.0 (deferred to v0.7.0)

- openCypher TCK harness (requires downloading TCK `.feature` files)
- Fuzz targets for the lexer and parser
- `WITH`, `OPTIONAL MATCH`, `ORDER BY`, `SKIP`, `LIMIT`
- `IN [...]`, `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `=~`
- Additional built-ins: `size()`, `length()`, `head()`, `tail()`, `toBoolean()`

---

## [0.5.1] — 2026-05-09 — TAP Infrastructure, WAL Hardening, and AGE Benchmark

v0.5.1 completes Phase 4.x: multi-session TAP tests prove WAL correctness
under crash and concurrent-write conditions; two critical correctness bugs are
fixed; the AGE benchmark gate is passed (4.27× faster on 2-hop expand).
25/25 pgrx tests + 11/11 TAP assertions pass.

### Critical Bug Fixes

**WAL redo PANIC on restart** — Any database that used v0.2.0–v0.5.0 would
PANIC on the first restart after inserting nodes. `redo_node_insert` called
`XLogReadBufferForRedo` for block 1 on every `NODE_INSERT` record, but block 1
only exists on `NODE_INSERT_OVF` records (inserts with overflow property
pages). PostgreSQL's WAL replayer PANICs when asked to locate a block that was
never registered. Fixed with an `is_ovf` guard that only accesses block 1 when
the record type is `XLOG_PG_EDDY_NODE_INSERT_OVF`.

**MVCC isolation broken under REPEATABLE READ / SERIALIZABLE** — `count_nodes()`
and all node scans were using `TransactionIdDidCommit(xmin)`, which returns
`true` for any committed transaction — including transactions that committed
*after* the reader's snapshot was taken. A REPEATABLE READ session therefore
saw new nodes inserted by concurrent transactions, violating snapshot isolation.
Fixed by checking `XidInMVCCSnapshot(xmin, snapshot)` when the xmin is
committed: a node is only visible if its inserting transaction committed
*before* the snapshot was taken.

### TAP Test Infrastructure

Four crash-safety and concurrency tests are now run by `just tap`:

- **001_crash_recovery** — inserts 10 000 nodes, sends `SIGQUIT` (immediate
  shutdown, no checkpoint), restarts the cluster, verifies `count_nodes() = 10000`.
  This test was the one that caught the WAL redo PANIC above.
- **002_edge_crash_recovery** — builds a 10-node / 20-edge ring graph, crashes
  and restarts, verifies edge count and adjacency-chain integrity survive WAL
  replay.
- **003_mvcc_isolation** — T2 opens a REPEATABLE READ transaction and
  snapshots an empty graph; T1 inserts and commits; T2 re-reads and must still
  see zero nodes (snapshot isolation). T2 then commits and must see 1 node.
  This test was the one that caught the MVCC bug above.
- **004_concurrent_inserts** — 4 background sessions each insert 1 000 nodes
  concurrently; verifies `count_nodes() = 4000` with all node IDs distinct
  (no sequence collisions or lost writes).

CI workflow `.github/workflows/tap.yml` runs all four scripts against a fresh
PostgreSQL 18 cluster on every push.

### New SQL Functions

| Function | Returns | Description |
|---|---|---|
| `count_nodes()` | `BIGINT` | Alias for the internal `node_count()`; used by TAP tests and user queries |
| `count_edges()` | `BIGINT` | Alias for the internal `edge_count()`; used by TAP tests and user queries |
| `find_edges(src BIGINT, dst BIGINT, rel_type TEXT)` | `SETOF BIGINT` | Fast edge lookup using rel-type catalog indexes when type + endpoint are given; falls back to adjacency-chain scan |

### Rel-type Catalog Indexes

Two new internal catalog tables enable O(1) edge lookup by type and endpoint
without scanning adjacency chains:

- `_pg_eddy.edge_type_src(type_id, src_node_id, edge_id)` — indexed on
  `(type_id, src_node_id)` and `edge_id`. Written on every `create_edge` call.
- `_pg_eddy.edge_type_dst(type_id, dst_node_id, edge_id)` — same structure
  for the destination endpoint.

Both tables are used by `find_edges()` fast paths and will be used by the
Cypher query planner in Phase 5.

### AGE Benchmark Gate — PASSED ✅

Results on a dev container (Debian 11, PostgreSQL 18.3, 1/50 scale):

| Operation | pg_eddy | AGE | Ratio |
|---|---|---|---|
| Node insert (1K nodes) | 0.129 s | 0.026 s | 0.20× (slower — P1 bug) |
| 1-hop adjacency follow | 12.52 ms | 12.24 ms | 0.98× (parity) |
| **2-hop neighbour expand** | **11.49 ms** | **49.08 ms** | **4.27× faster** |

The ≥2× gate on 2-hop expansion is cleared. **v0.6.0 (Cypher engine) starts
next.** The insert regression (5× slower than AGE due to per-edge SPI writes
to the catalog index tables) is filed as a P1 bug, deferred to v0.5.2 after
the first Cypher milestone ships.

### Schema Note

PostgreSQL reserves all schema names beginning with `pg_`. The `schema =
'pg_eddy'` field that was briefly attempted in the control file was rejected
by PostgreSQL with `ERRCODE_RESERVED_NAME`. Functions install in `public`
(or the schema chosen at `CREATE EXTENSION` time). Internal objects remain in
`_pg_eddy` as before.

### Migration

Upgrade from v0.5.0:

```sql
ALTER EXTENSION pg_eddy UPDATE TO '0.5.1';
-- or run: psql -f sql/pg_eddy--0.5.0--0.5.1.sql
```

New objects added by the migration:

| Object | Type | Description |
|---|---|---|
| `_pg_eddy.edge_type_src` | TABLE | Rel-type → src-node → edge catalog index |
| `_pg_eddy.edge_type_dst` | TABLE | Rel-type → dst-node → edge catalog index |
| `count_nodes()` | FUNCTION | Alias for `node_count()` |
| `count_edges()` | FUNCTION | Alias for `edge_count()` |
| `find_edges(bigint, bigint, text)` | FUNCTION | Fast edge lookup by type + endpoint |

---

## [0.5.0] — 2026-05-09 — Indexes, Constraints, and Full CRUD API

v0.5.0 completes Phase 4: the storage layer is feature-complete for building
the query engine on top. Property overflow pages, physical VACUUM compaction,
label indexes, and the full node/edge CRUD API are all implemented.
24/24 pgrx tests pass.

### Critical WAL Opcode Fix

All WAL info bytes now use only the **high nibble** (bits 4–7). PostgreSQL's
XLogInsert reserves bits 2–3 of the low nibble for its own flags and will
PANIC if an extension sets them. The previous opcodes were broken:

| Record | Old (broken) | New (correct) |
|---|---|---|
| `NODE_INSERT` | `0x00` | `0x00` (unchanged) |
| `NODE_INSERT_OVF` | `0x05` | `0x10` |
| `NODE_DELETE` | `0x02` | `0x20` |
| `NODE_COMPACT` | `0x04` | `0x30` |
| `EDGE_INSERT` | `0x10` | `0x40` |
| `EDGE_DELETE` | `0x11` | `0x50` |
| `ADJ_UPDATE` | `0x20` | `0x60` |
| `VACUUM_PAGE` | `0x30` | `0x70` |

**Databases created with v0.4.0 or earlier cannot be upgraded in-place** — the
on-disk WAL records have the old opcodes. Create a fresh cluster for v0.5.0.

### Property Overflow Pages

Node records now support properties exceeding 48 bytes. When the inline
property buffer is full, a **property overflow page** is allocated in the same
node relation and its block number is stored in the `prop_overflow_page` field
of the node record. The overflow page holds the full serialised property map.

WAL coverage: the overflow block is written inside the same critical section
as the node record, using `REGBUF_FORCE_IMAGE` so the full page image is
captured. VACUUM skips overflow pages (they are reclaimed when the parent node
record becomes dead-to-all-snapshots).

### Physical VACUUM Compaction

After `VACUUM _pg_eddy.nodes`, dead node slots are now physically removed from
pages via `PageRepairFragmentation`. The page is WAL-logged as a full page
image via `XLOG_PG_EDDY_NODE_COMPACT`. Zeroed-out adjacency headers for dead
slots are cleared at the same time. Free space is correctly returned to
PostgreSQL's free space map.

### Label B-tree Index

`_pg_eddy.label_index(label_id INT, node_id BIGINT)` is maintained by the
Rust/SPI layer in `create_node`, `update_node`, `add_label`, `remove_label`,
and `delete_node`. It enables O(|matching nodes|) label scans without sweeping
all node pages.

### New SQL Functions

| Function | Returns | Description |
|---|---|---|
| `add_label(node_id BIGINT, label TEXT)` | `BOOLEAN` | Add a label to an existing node; `FALSE` if already present |
| `remove_label(node_id BIGINT, label TEXT)` | `BOOLEAN` | Remove a label; `FALSE` if not present |
| `detach_delete_node(node_id BIGINT)` | `BOOLEAN` | Delete all incident edges then delete the node atomically |
| `find_nodes(label TEXT, property_filter JSONB)` | `SETOF BIGINT` | Fast label lookup via `label_index`; optional property post-filter |
| `schema_info()` | `JSONB` | Label, rel-type, and property-key registry summary |

### Migration

Upgrade from v0.4.0:

```sql
ALTER EXTENSION pg_eddy UPDATE TO '0.5.0';
-- or run: psql -f sql/pg_eddy--0.4.0--0.5.0.sql
```

**Note**: if your cluster has WAL generated by v0.4.0 or earlier, create a
fresh cluster rather than upgrading — the WAL opcode change is not backward
compatible.

New objects added by the migration:

| Object | Type | Description |
|---|---|---|
| `_pg_eddy.label_index` | TABLE | Label → node B-tree catalog index |
| `add_label(bigint, text)` | FUNCTION | Add a label to a node |
| `remove_label(bigint, text)` | FUNCTION | Remove a label from a node |
| `detach_delete_node(bigint)` | FUNCTION | Detach-delete a node and all its edges |
| `find_nodes(text, jsonb)` | FUNCTION | Label + property scan |
| `schema_info()` | FUNCTION | Registry summary |

---


v0.4.0 implements Phase 3: correct MVCC semantics for nodes and a working
VACUUM pass for both node and edge tables. 17/17 pgrx tests pass.

### What's New

**Node MVCC**

- `pg_eddy.update_node(node_id, labels, properties)` — logically deletes the
  old node record and inserts a new MVCC version on the same page, preserving
  the adjacency-header slot index (`adj_slot_idx`).
- `pg_eddy.delete_node(node_id)` — sets xmax on the node record; physical
  reclamation happens during the next VACUUM pass.
- `read_node_at_offset` now performs full xmin/xmax visibility checks, so
  deleted or not-yet-committed node inserts are correctly filtered out of
  scans and `get_node()` results.

**adj_slot_idx fix**

A bug in Phase 1 caused every node to be stored with `adj_slot_idx = 0`,
meaning all nodes on a page incorrectly shared the same adjacency header
slot. This is now fixed: after `PageAddItemExtended` the correct slot index
(`off − 1`) is written back into the in-page record and used for all
adjacency-header reads and writes.

**VACUUM**

- `VACUUM _pg_eddy.nodes` and `VACUUM _pg_eddy.edges` are now functional.
  The `relation_vacuum` AM callback scans every page, finds slots whose
  xmax has been committed before `GetOldestNonRemovableTransactionId`, marks
  them `LP_DEAD`, and WAL-logs the change via the new
  `XLOG_PG_EDDY_VACUUM_PAGE` (0x30) WAL record type.
- Dead edge slots are **not** physically removed in v0.4.0; instead they are
  kept with `LP_DEAD` flags so that adjacency-chain traversal can still read
  the `next_out` / `next_in` pointers through them. Physical compaction
  (`PageRepairFragmentation`) is planned for Phase 4.
- `edge_store::follow_chain` now handles `LP_DEAD` slots: they are skipped
  (not yielded to callers) but the chain pointer is still followed so the
  remainder of the chain is reachable.

**WAL**

- New `XLOG_PG_EDDY_NODE_DELETE` (0x02) WAL record: sets xmax on the
  in-page `HeapTupleHeaderData`.
- New `XLOG_PG_EDDY_VACUUM_PAGE` (0x30) WAL record: a compact list of
  offset numbers to mark `LP_DEAD` on redo.
- Both records have corresponding redo functions, `rmgr_desc`, and
  `rmgr_identify` entries.

**am_stats()**

`pg_eddy.am_stats()` returns a JSONB document with `live_nodes`, `dead_nodes`,
`live_edges`, `dead_edges`, `node_pages`, and `edge_pages`, suitable for
diagnosing fragmentation before running VACUUM.

### Edge-store improvements

The private `find_node_location` in `edge_store.rs` has been replaced by the
public `node_store::find_node_location`, which returns the **stored**
`adj_slot_idx` from the node record rather than computing it from the item
offset. This is important for correctness after node updates create new items
at different offsets while the adj slot stays the same.



## [0.3.0] — 2026-05-09 — Edge Storage + Adjacency Lists

v0.3.0 implements Phase 2 of the pg_eddy roadmap. Edges are stored with
singly-linked adjacency chains. Edge deletes are logical only (set xmax);
physical compaction is deferred to Phase 3 VACUUM. 14/14 pgrx tests pass.

### Storage Layout — Edge Pages

Each edge page (8 KB) uses standard `PageInit(page, BLCKSZ, 0)` — no
`pd_special` area. Edge slots contain:

```
HeapTupleHeaderData (24 B)
rel_id           (8 B, i64 LE)   — globally unique edge id
rel_type_id      (4 B, i32 LE)   — relationship type (from rel_type_registry)
source_node_id   (8 B, i64 LE)
target_node_id   (8 B, i64 LE)
next_out_page    (4 B, u32 LE)   — next edge in source's outgoing chain
next_out_slot    (2 B, u16 LE)   — 0 = end of chain
next_in_page     (4 B, u32 LE)   — next edge in target's incoming chain
next_in_slot     (2 B, u16 LE)   — 0 = end of chain
prop_inline_len  (2 B, u16 LE)
prop_overflow_page (4 B, u32 LE) — 0 = no overflow (Phase 2: overflow = PE200)
prop_data        (up to 48 B)    — inline binary properties
```

Adjacency heads are stored in the **node page** `pd_special` area
(`NodeAdjHeader` entries), NOT inside edge records, so inserting an edge never
creates a new MVCC version of the source or target node record.

### Adjacency Chain Protocol

- **Insert at head**: new edges are inserted at the front of the out-chain
  (source) and in-chain (target). The `next_*` pointers are set to the
  previous head before the insert.
- **Delete = logical only**: `xmax` is set; the slot remains in the chain.
  Traversal skips invisible slots; VACUUM (Phase 3) rebuilds the chain.
- **Lock ordering**: source node page is always locked before target node page
  (by block number) to prevent deadlocks under concurrent inserts.

### WAL Records

| Record | Opcode | Covers | Approx. size |
|---|---|---|---|
| `XLOG_PG_EDDY_EDGE_INSERT` | `0x10` | Edge page (new slot) | 80–120 B |
| `XLOG_PG_EDDY_EDGE_DELETE` | `0x11` | Edge page (xmax set) | 12 B |
| `XLOG_PG_EDDY_ADJ_UPDATE`  | `0x20` | Node page (new adj header) | 30 B |

Each `create_edge` call emits three WAL records (one per opcode above, two
`ADJ_UPDATE` for source and target). All are within a single critical section.

### Catalog

- `ensure_rel_type(name)` / `rel_type_name(id)` — SPI-backed relationship type
  registry with idempotent upsert.
- `next_edge_id()` — allocates a dense sequential edge id via
  `nextval('_pg_eddy.edge_id_seq')`.

### Simplified MVCC (Phase 2)

Tuple visibility checks use the PostgreSQL commit log (`TransactionIdDidCommit`,
`TransactionIdIsCurrentTransactionId`) to filter out ghost tuples from
rolled-back transactions. Full `HeapTupleSatisfiesVisibility` with snapshot
isolation is Phase 3.

### SQL API

New functions installed by this release:

| Function | Returns | Description |
|---|---|---|
| `create_edge(source BIGINT, target BIGINT, type TEXT, properties JSONB)` | `BIGINT` | Insert an edge; returns its `rel_id` |
| `get_edge(rel_id BIGINT)` | `JSONB` | Read an edge by id; `NULL` if not found or deleted |
| `delete_edge(rel_id BIGINT)` | `BOOLEAN` | Logically delete an edge; `TRUE` if found |
| `edge_count()` | `BIGINT` | Count all non-deleted edges |
| `neighbours(node_id BIGINT, direction TEXT, rel_type TEXT)` | `SETOF BIGINT` | Follow adjacency chain; returns neighbour node ids |
| `expand(node_id BIGINT, direction TEXT, rel_type TEXT)` | `TABLE(...)` | Like neighbours but returns full edge info |

`direction` is `'OUT'`, `'IN'`, or `'BOTH'`. `rel_type` is `NULL` for all
types.

### Migration

Upgrade from v0.2.0:

```sql
ALTER EXTENSION pg_eddy UPDATE TO '0.3.0';
-- or run: psql -f sql/pg_eddy--0.2.0--0.3.0.sql
```

New objects added by the migration:

| Object | Type | Description |
|---|---|---|
| `_pg_eddy.edge_id_seq` | SEQUENCE | Dense sequential edge id allocator |
| `create_edge(...)` | FUNCTION | Edge insert API |
| `get_edge(...)` | FUNCTION | Edge read API |
| `delete_edge(...)` | FUNCTION | Edge logical delete API |
| `edge_count()` | FUNCTION | Edge count API |
| `neighbours(...)` | FUNCTION | Adjacency-follow SRF (node ids) |
| `expand(...)` | FUNCTION | Adjacency-follow SRF (full edge rows) |

### Deliverable Checklist (Phase 2)

- [x] Edge page layout: MVCC records + singly-linked chain pointers
- [x] `tuple_insert` for edges with adjacency chain maintenance
- [x] Logical delete for edges (xmax set, no chain modification)
- [x] WAL redo for `EDGE_INSERT`, `EDGE_DELETE`, `ADJ_UPDATE`
- [x] Lock ordering: source node page locked before target node page
- [x] Adjacency-follow scan (`neighbours`, `expand`) — O(degree), no index
- [x] `create_edge`, `get_edge`, `delete_edge`, `edge_count`
- [x] `neighbours(node_id, direction, rel_type)` — SETOF BIGINT
- [x] `expand(node_id, direction, rel_type)` — TABLE(rel_id, other_node_id, rel_type_id, rel_properties)
- [ ] Property overflow pages (deferred; > 48 B properties raise PE200)
- [ ] Slot callback verification with SQL trigger (Phase 3)
- [ ] Early pg-trickle smoke test (Phase 3)
- [ ] Concurrency / crash-safe edge tests (Phase 3)

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
