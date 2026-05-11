# TCK Failure Classification — post-Quantifier-fix (3018/3880 passing, 77.8%)

**Date**: 2026-05-11
**Status**: living reference — update after each release
**Purpose**: Exhaustive classification of every failing TCK scenario, grouped
by root cause. See [plans/rs-polygraph-analysis.md](rs-polygraph-analysis.md)
§4.2 for rationale.

## Snapshot

- **Pass**: 3018 / 3880 (77.8%)
- **Fail**: 862
- **Unique failing feature groups**: 28

## Bucket Summary

| Count | Bucket | Action |
|------:|--------|--------|
| 826 | Temporal types (Date, Time, LocalTime, LocalDateTime, DateTime, Duration) | Implement temporal type system (large feature; deferred) |
| 12 | Variable-length / shortest-path patterns (Match4/5/9) | Implement variable-length expand correctness improvements |
| ~~12~~ 0 | ~~Quantifier1–4 type mismatch [15/16]~~ ✅ **FIXED in v0.21.0-dev** | Compile-time type check in planner |
| 5 | WithOrderBy2 date sort [11,12] | Subset of temporal bucket — ORDER BY on date expression |
| 4 | Optional MATCH edge cases (Match7[22,28], Match8[2,3]) | Targeted fixes to exec_expand / optional-match planner |
| 2 | Create2 [11,12] — adjacency edge to existing node | Storage-level: edge from pre-existing node not visible to follow-up MATCH |
| 2 | CountingSubgraphMatches1 [10,11] — self-relationship counting | Executor: count self-relationships in mixed directed/undirected pattern |
| 2 | WithOrderBy4 [13,14] — fail on non-projected aggregation in ORDER BY | Planner: add InvalidAggregation check for ORDER BY agg refs not in RETURN |
| 2 | With2[1] / With4[2] — Forward property across WITH for join | Planner: forwarded scalar binding not used as join key |
| 1 each | Pattern1[11] (self-pattern type check), Merge5[10] (path bind), WithSkipLimit2[2], MatchWhere4[2], WithWhere4[2], Match6[14], Delete5[7], Temporal7[*] | Targeted fixes |

## Detailed Buckets

### 1. Temporal types — 826 failures (deferred by design)

**Spec ref**: openCypher 9 §3.5 (Temporal types)

| Suite | Count | Status |
|-------|------:|--------|
| Temporal9 | 322 | DateTime arithmetic and comparison |
| Temporal3 | 183 | Time + LocalTime |
| Temporal1 | 162 | Date construction and properties |
| Temporal10 | 66 | Duration arithmetic |
| Temporal8 | 27 | Duration construction |
| Temporal4 | 27 | LocalDateTime |
| Temporal6 | 17 | Time zones |
| Temporal2 | 14 | Date arithmetic |
| Temporal5 | 7 | DateTime |
| Temporal7 | 1 | Edge cases |

**Action**: Deferred. Temporal types are a large feature requiring:
- 5 distinct types (Date, Time, LocalTime, LocalDateTime, DateTime) + Duration
- Timezone resolution (IANA tz database integration)
- Calendar arithmetic (months, quarters, etc.)
- Comparison and ordering across types
- Serialization to/from JSON for storage

Recommend a dedicated v0.22.0+ release.

### 2. Variable-length and named paths — 12 failures

| Test | Issue |
|------|-------|
| Match4[*] | Variable-length pattern with various predicates |
| Match5[*] | shortestPath / allShortestPaths |
| Match9[*] | Deprecated variable-length scenarios |
| Match6[14] | Named path with undirected fixed variable length |

**Action**: Each Match4/5/9 group needs targeted analysis. Most are likely
correctness issues in `exec_var_length_expand` rather than missing features.

### 3. Quantifier type-mismatch detection — 12 failures (4 scenarios × ~3 examples)

**Spec ref**: openCypher 9 §6.5 (Predicate functions)

| Test | Scenario |
|------|----------|
| Quantifier1[15] | none(x IN list WHERE pred) with type mismatch |
| Quantifier2[16] | single(x IN list WHERE pred) with type mismatch |
| Quantifier3[15] | any(x IN list WHERE pred) with type mismatch |
| Quantifier4[15] | all(x IN list WHERE pred) with type mismatch |

Expected behaviour: when the list element type and the predicate's operand
type are statically incompatible (e.g., `all(x IN [1,2,3] WHERE x.prop)` where
integers don't have properties), raise SyntaxError at compile time.

**Action**: Add compile-time type check in planner for list predicates. Requires
basic type inference over the list expression.

### 4. Optional MATCH edge cases — 4 failures

| Test | Issue |
|------|-------|
| Match7[22] | MATCH after OPTIONAL MATCH — subsequent MATCH should run on null rows? |
| Match7[28] | OPTIONAL MATCH with inline label predicate |
| Match8[2] | Counting rows after MATCH, MERGE, OPTIONAL MATCH |
| Match8[3] | Matching and disregarding output, then matching again |

**Action**: Each needs individual analysis — these are pre-existing failures
that survived v0.20.0 improvements. Suspect: how MATCH-after-OPTIONAL-MATCH
interacts with the row pipeline when the optional side is null.

### 5. Storage / adjacency — 2 failures

| Test | Issue |
|------|-------|
| Create2[11] | Create relationship and end node on existing start node |
| Create2[12] | Create relationship and start node on existing end node |

**Hypothesis**: After `MATCH (a) CREATE (a)-[:R]->(b)`, the new edge is
created on the heap but the adjacency-page index for `a` might not be flushed
before a subsequent traversal sees `a`. Storage-level bug.

**Action**: Trace via TAP test reproducer; verify adjacency-page CatalogWriteBuffer
flushes before the next MATCH executes.

### 6. Self-relationship counting — 2 failures

| Test | Issue |
|------|-------|
| CountingSubgraphMatches1[10] | (a)-[r]-(a) directed/undirected count |
| CountingSubgraphMatches1[11] | (a)-[r]-(a) undirected count |

**Action**: Verify exec_expand emits both directions exactly once for
undirected self-loops; or twice when it should.

### 7. Aggregation-in-ORDER-BY validation — 2 failures

| Test | Issue |
|------|-------|
| WithOrderBy4[13] | Sort by non-projected aggregation on a variable |
| WithOrderBy4[14] | Sort by non-projected aggregation on an expression |

Expected: raise InvalidAggregation when an aggregate appears in ORDER BY that
is not also in the RETURN/WITH projection.

**Action**: Planner check exists for RETURN, needs to extend to WITH. Already
attempted in v0.20.0 but reverted due to regressions; needs more careful
predicate.

### 8. WITH variable forwarding — 2 failures

| Test | Issue |
|------|-------|
| With2[1] | Forwarding a property to express a join |
| With4[2] | Aliasing expression to new variable name then re-binding |

**Action**: Both involve a WITH-clause value used downstream as a join key.
Planner currently expects pattern-pattern joins, not scalar-to-pattern joins.

### 9. Singletons (1 failure each, 9 tests)

| Test | Bucket |
|------|--------|
| Pattern1[11] | Compile-time type check: WHERE (n) must reject non-boolean pattern |
| Merge5[10] | MERGE should bind a path variable |
| WithSkipLimit2[2] | Dependencies across WITH with LIMIT |
| MatchWhere4[2] | Non-equi join with disjunctive multi-part predicates |
| WithWhere4[2] | Same as MatchWhere4[2] but in WITH |
| Match6[14] | Named path with undirected fixed variable length |
| Delete5[7] | Delete paths from nested map/list |
| Literals6[5] | (already fixed in v0.20.0) |
| Literals7[17] | (already fixed in v0.20.0) |

## Recommended v0.21.0 Targets

In rough effort-vs-value order:

1. **Quantifier type-mismatch** (12 wins): compile-time type check in planner —
   well-scoped, clear spec semantics.
2. **WithOrderBy4[13,14]** (2 wins): re-enable aggregation-in-ORDER-BY check
   with a tighter predicate that avoids the v0.20.0 regression.
3. **CountingSubgraphMatches1[10,11]** (2 wins): self-loop counting in
   exec_expand.
4. **Create2[11,12]** (2 wins): adjacency flush timing — TAP test reproducer first.
5. **Pattern1[11]** (1 win): reject non-pattern non-boolean expressions in WHERE.
6. **Match7[28]** (1 win): inline label predicate on optional match.

Total realistic v0.21.0 target: **+20 to +30 scenarios** (3026–3036 / 3880).

## Out of Scope (Future Releases)

- Temporal types (v0.22.0+) — 826 scenarios
- Spatial types — currently unimplemented, no TCK failures attributed
- Variable-length path completeness (Match4/5/9) — partial in v0.21.0+

## How To Update This File

After each TCK run:
1. Update `Snapshot` numbers.
2. Move any bucket that fully clears to a "Resolved" section.
3. Add new buckets for any newly-failing categories.
4. Re-rank the v0.NN.0 target list based on current effort estimates.
