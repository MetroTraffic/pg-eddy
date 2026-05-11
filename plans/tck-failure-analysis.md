# TCK Failure Classification — post v0.20.0 fixes (3029/3880 passing, 78.1%)

**Date**: 2025-05-11 (updated)
**Status**: living reference — update after each release
**Purpose**: Exhaustive classification of every failing TCK scenario, grouped
by root cause. See [plans/rs-polygraph-analysis.md](rs-polygraph-analysis.md)
§4.2 for rationale.

## Snapshot

- **Pass**: 3029 / 3880 (78.1%)
- **Fail**: 851
- **Unique failing feature groups**: 22 (down from 28)
- **Regression floor**: 3029 (enforced by `tests/tck/baseline.txt`)

## Bucket Summary

| Count | Bucket | Action |
|------:|--------|--------|
| 826 | Temporal types (Date, Time, LocalTime, LocalDateTime, DateTime, Duration) | Implement temporal type system → v0.22.0 |
| ~~12~~ 5 | Variable-length / shortest-path (Match4[7,8], Match5[27], Match6[14], Match9[*]) | Remaining hard cases after v0.21.0-dev fixes |
| ~~12~~ 0 | ~~Quantifier1–4 type mismatch [15/16]~~ ✅ **FIXED** | Compile-time type check in planner |
| 5 | WithOrderBy2 date sort [11,12] | Subset of temporal bucket — ORDER BY on date expression |
| ~~4~~ 2 | ~~Optional MATCH (Match7[22,28])~~ partially ✅ | Match7[22,28] **FIXED**; Match8[2,3] remain |
| 2 | Create2 [11,12] — adjacency edge to existing node | Storage-level: edge from pre-existing node not visible to follow-up MATCH |
| 2 | CountingSubgraphMatches1 [10,11] — self-relationship counting | Executor: count self-relationships in mixed directed/undirected pattern |
| 2 | WithOrderBy4 [13,14] — fail on non-projected aggregation in ORDER BY | Planner: add InvalidAggregation check for ORDER BY agg refs not in RETURN |
| 2 | With2[1] / With4[2] — Forward property across WITH for join | Planner: forwarded scalar binding not used as join key |
| 1 | WithSkipLimit2[2] | Limit dependencies |
| 1 | MatchWhere4[2] | Disjunctive multi-part predicates |
| 1 | WithWhere4[2] | Same as MatchWhere4[2] but in WITH |
| 1 | Delete5[7] | Delete paths from nested map/list |
| 0 | ~~Pattern1[11]~~ ✅ **FIXED** | WHERE boolean check |
| 0 | ~~Merge5[10]~~ ✅ **FIXED** | MERGE path bind on create branch |
| 0 | ~~Match5[4,21,22]~~ ✅ **FIXED** | *N exact length parsing |
| 0 | ~~Match4[4], Match6[14], Match9[5]~~ ✅ **FIXED** | Var-length dst predicates |
| 0 | ~~Match4[5]~~ ✅ **FIXED** | Var-length rel property predicates |

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

### 2. Variable-length and named paths — 5 remaining failures

| Test | Issue | Status |
|------|-------|--------|
| Match4[4] | Var-length with dst label predicate | ✅ Fixed |
| Match4[5] | Var-length with rel property predicate | ✅ Fixed |
| Match4[7] | Complex var-length with multiple predicates | Open |
| Match4[8] | Complex var-length with backtracking | Open |
| Match5[4,21,22] | *N exact length | ✅ Fixed |
| Match5[27] | allShortestPaths with complex predicates | Open |
| Match6[14] | Named path with undirected fixed var-length | ✅ Fixed |
| Match9[5] | Deprecated var-length with dst predicate | ✅ Fixed |
| Match9[*] | Remaining deprecated syntax | Open (~2) |

**Action**: Remaining cases (Match4[7,8], Match5[27]) require deeper analysis
of the BFS executor. Low priority — only 5 scenarios total.

### 3. Quantifier type-mismatch detection — ✅ RESOLVED (0 remaining)

All 12 scenarios fixed via `check_quantifier_type_mismatch()` in planner.
Detects when the list element type and predicate operand type are statically
incompatible and raises SyntaxError at compile time.

### 4. Optional MATCH edge cases — 2 remaining failures

| Test | Issue | Status |
|------|-------|--------|
| Match7[22] | OPTIONAL MATCH with non-existent dst label | ✅ Fixed |
| Match7[28] | OPTIONAL MATCH with inline label predicate | ✅ Fixed |
| Match8[2] | Counting rows after MATCH, MERGE, OPTIONAL MATCH | Open |
| Match8[3] | Matching and disregarding output, then matching again | Open |

**Action**: Match8[2,3] need individual analysis — involve interaction between
MATCH-after-MERGE and OPTIONAL MATCH row pipeline.

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

### 9. Singletons (1 failure each)

| Test | Bucket | Status |
|------|--------|--------|
| ~~Pattern1[11]~~ | ~~WHERE boolean check~~ | ✅ Fixed |
| ~~Merge5[10]~~ | ~~MERGE path bind~~ | ✅ Fixed |
| WithSkipLimit2[2] | Dependencies across WITH with LIMIT | Open |
| MatchWhere4[2] | Non-equi join with disjunctive multi-part predicates | Open |
| WithWhere4[2] | Same as MatchWhere4[2] but in WITH | Open |
| ~~Match6[14]~~ | ~~Named path with undirected var-length~~ | ✅ Fixed |
| Delete5[7] | Delete paths from nested map/list | Open |
| ~~Literals6[5]~~ | ~~Gherkin backslash unescaping~~ | ✅ Fixed (v0.20.0) |
| ~~Literals7[17]~~ | ~~String-aware list depth~~ | ✅ Fixed (v0.20.0) |

## Resolved (v0.21.0-dev, post v0.20.0 tag)

| Fix | Scenarios Won | Commit |
|-----|:---:|--------|
| Quantifier type-mismatch detection | +12 | planner `check_quantifier_type_mismatch()` |
| WHERE boolean check (Pattern1[11]) | +1 | planner `check_where_is_boolean()` |
| MERGE path bind (Merge5[10]) | +1 | executor MERGE create branch |
| OPTIONAL MATCH label short-circuit (Match7[22,28]) | +2 | executor `exec_expand` |
| *N exact length parsing (Match5[4,21,22]) | +3 | parser `min_explicit` tracking |
| Var-length dst predicates (Match4[4], Match6[14], Match9[5]) | +3 | planner Filter step |
| Var-length rel predicates (Match4[5]) | +1 | planner rel property Filter |
| **Total** | **+23** | 3006 → 3029 |

## Remaining v0.21.0 Targets

In rough effort-vs-value order:

1. **CountingSubgraphMatches1[10,11]** (2 wins): self-loop counting in
   exec_expand.
2. **WithOrderBy4[13,14]** (2 wins): re-enable aggregation-in-ORDER-BY check
   with a tighter predicate that avoids the v0.20.0 regression.
3. **Create2[11,12]** (2 wins): adjacency flush timing — TAP test reproducer first.
4. **With2[1] / With4[2]** (2 wins): scalar-to-pattern join via WITH.
5. **Match8[2,3]** (2 wins): MATCH after MERGE + OPTIONAL MATCH row counting.
6. **WithSkipLimit2[2]** (1 win): dependencies across WITH with LIMIT.
7. **MatchWhere4[2] / WithWhere4[2]** (2 wins): disjunctive multi-part predicates.
8. **Delete5[7]** (1 win): DELETE paths from nested map/list.

Total realistic v0.21.0 target: **+14 to +20 scenarios** (3043–3049 / 3880, ~78.6%).

After v0.21.0, only **826 temporal** + ~5 hard variable-length cases remain.
The temporal type system (v0.22.0) is the final major feature push.

## Out of Scope (Future Releases)

- Temporal types (v0.22.0) — 826 scenarios; dedicated release planned
- Spatial types — currently unimplemented, no TCK failures attributed
- Variable-length hard cases (Match4[7,8], Match5[27]) — deferred; 
  require recursive pattern analysis beyond current BFS executor

## How To Update This File

After each TCK run:
1. Update `Snapshot` numbers.
2. Move any bucket that fully clears to a "Resolved" section.
3. Add new buckets for any newly-failing categories.
4. Re-rank the v0.NN.0 target list based on current effort estimates.
