# Lessons from rs-polygraph's Spec-First Pivot

**Date**: 2026-05-11  
**Status**: reference  
**Context**: Analysis of [trickle-labs/rs-polygraph](https://github.com/trickle-labs/rs-polygraph)
and its [spec-first-pivot.md](https://github.com/trickle-labs/rs-polygraph/blob/main/plans/spec-first-pivot.md),
with recommendations for pg_eddy.

---

## 1. What rs-polygraph Is

rs-polygraph is a Rust library that **transpiles openCypher (and ISO GQL) queries
into SPARQL 1.1** algebra. It targets any SPARQL-compliant engine (Oxigraph,
Apache Jena, Stardog) as its execution backend. It does not execute queries
itself — it emits SPARQL strings.

Current state: TCK 3793/3828 (99.1%), 232 curated differential test queries.

---

## 2. What the Pivot Was

rs-polygraph was originally built by **reverse-engineering TCK scenarios**: read a
failing test, patch the translator until it passes, repeat. This reached ~97.5%
TCK compliance but left three structural risks:

1. **Grammar grew to fit the TCK**, not the spec. Constructs the TCK doesn't
   exercise (deeply nested `CALL { }`, label expressions with `&`/`|`/`!`, list
   comprehensions inside map projections) were silently rejected or misparsed.

2. **AST → SPARQL was a single hop** through visitors + ad-hoc rewrite rules.
   Many rules were scenario-specific patches rather than normalizations derivable
   from the spec.

3. **TCK was the only correctness oracle.** The TCK is thin in several
   user-visible areas (large WITH chains with aggregation+ordering, null
   propagation through CASE, bag semantics around DISTINCT + OPTIONAL MATCH).

The pivot introduced three remediation pillars:

| Pillar | What it is | Why it matters |
|--------|-----------|----------------|
| **Logical Query Algebra (LQA)** | A typed intermediate representation between AST and SPARQL | Encodes openCypher semantics in one place; everything below is mechanical lowering |
| **Grammar hardening** | Audit + extend parser for constructs TCK doesn't test | Prevents silent rejection of valid user queries |
| **Differential testing** | 232+ curated queries tested against an in-process Oxigraph oracle | Correctness measured beyond TCK; catches silent miscompilation |

### Key architectural pattern: Strangler-Fig Migration

The LQA was inserted between AST and SPARQL using a **strangler-fig** strategy:

```
Transpiler::cypher_to_sparql()
   ├─ 1. lower_to_lqa(ast) → Op tree        (new LQA path)
   ├─ 2. compile_lqa(op) → sparql            (new compiler)
   │       if Err(Unsupported) …
   └─ 3. fallback: legacy translate()        (existing translator)
```

The legacy translator remained as a safety net. The LQA path returned
`Err(Unsupported)` for constructs it couldn't yet handle, and execution silently
fell back to legacy. This meant **adding a new lowering arm could never make a
previously-passing query wrong**. The legacy translator was only deleted after
the LQA path handled ≥99% of the TCK corpus.

### Key process pattern: TCK Floor + Mechanical Loop

Every phase had a **TCK floor** — no phase could land if the pass count dropped
below the starting value. New LQA arms were added via a mechanical loop:

1. Add a difftest query first (establishes oracle before code changes)
2. Find the legacy implementation (read what SPARQL it emits)
3. Add the match arm in the new compiler (same semantics, not better)
4. Verify (difftest + TCK + trace legacy fallback count)
5. Commit with bucket counts

---

## 3. Where pg_eddy Differs Fundamentally

| Dimension | rs-polygraph | pg_eddy |
|-----------|-------------|---------|
| **What it does** | Transpiles Cypher → SPARQL strings | Executes Cypher natively inside PostgreSQL |
| **Execution model** | Zero execution — emits text for another engine | Full interpreter: lexer → parser → planner → executor → rows |
| **Backend** | Any SPARQL engine (Oxigraph, Jena, etc.) | Custom Table Access Method in PostgreSQL 18 |
| **Storage** | Relies on RDF triple store | Custom adjacency-list page format with WAL |
| **Language** | Cypher surface only; semantics must map to SPARQL | Full Cypher semantics; no translation target constraints |
| **Hard ceiling** | SPARQL's set-of-triples model blocks multigraph parallel edges, runtime list materialization | No inherent ceiling — can implement anything the spec requires |
| **TCK rate** | 3793/3828 (99.1%) — but many failures are fundamental SPARQL limits | 2391/3880 (61.6%) — failures are implementation gaps, not architectural limits |

**Critical difference**: rs-polygraph is constrained by what SPARQL can express.
pg_eddy has no such constraint — every TCK failure is fixable by writing more
Rust. This makes pg_eddy's ceiling 100%, while rs-polygraph's theoretical
maximum is 99.8% (one scenario requires multigraph support incompatible with
RDF's data model).

---

## 4. What We Can Learn

### 4.1 — Adopt Differential Testing Beyond TCK (HIGH VALUE)

**The problem rs-polygraph identified**: TCK pass rate is a necessary but not
sufficient correctness metric. The TCK is thin on:
- Large WITH chains with aggregation + ordering
- Null propagation through CASE
- Parameterized queries
- Bag semantics around DISTINCT + OPTIONAL MATCH

**pg_eddy's current state**: The TCK harness (`tests/tck/run_tck.pl`) is the
primary correctness oracle. Unit tests exist in the Rust code but focus on
component behavior, not end-to-end query semantics.

**Recommendation**: Build a curated query suite that exercises pg_eddy's Cypher
engine **beyond TCK coverage**. Concretely:

- Create `tests/difftest/` with TOML files (or similar) defining:
  - Input Cypher query
  - Setup graph (CREATE statements)
  - Expected result rows (bag or ordered)
- Focus on areas where TCK coverage is thin:
  - Multi-hop traversals with variable-length paths and property filters
  - Aggregation edge cases (GROUP BY with nulls, DISTINCT + ORDER BY)
  - WITH chains with aggregation and re-aliasing
  - CASE with null propagation
  - UNWIND + aggregation interactions
  - OPTIONAL MATCH null semantics
- Run these as part of `cargo pgrx test pg18`
- **Start at 50 curated queries**, grow to 200+

This is the single highest-value takeaway. rs-polygraph found previously-unknown
bugs during their Phase 1 (differential testing) that the TCK didn't cover.

### 4.2 — Classify Remaining TCK Failures Exhaustively (HIGH VALUE)

**What rs-polygraph did**: Before starting their Phase 3 (LQA), they audited all
71 remaining TCK failures and classified every one:

| Count | Bucket | Root cause |
|------:|--------|------------|
| 17 | Temporal8 — duration arithmetic | Runtime limitation |
| 10 | DST timezone | IANA DB required |
| 8 | Quantifier on list of nodes/rels | SPARQL encoding limit |
| ... | ... | ... |

This classification drove their entire phase ordering — they knew exactly what
to build, in what order, and which failures were permanently out of reach.

**pg_eddy's current state**: At 2391/3880 (61.6%), there are ~1489 failing
scenarios. The skip list in `run_tck.pl` categorizes some, but there is no
exhaustive bucket analysis of every failure.

**Recommendation**: Produce a `plans/tck-failure-analysis.md` that classifies
every failing scenario into buckets:

| Bucket | Example | Action |
|--------|---------|--------|
| Parser gap | Pattern expressions as predicates | Implement in parser |
| Missing function | `randomUUID()`, spatial | Implement in executor |
| Missing clause | `FOREACH`, `CALL {}` subquery | Implement clause-by-clause |
| Constraint/schema | `CREATE CONSTRAINT` | Design decision needed |
| Known bug | Specific edge case | Fix in targeted PR |
| Deferred by design | Spatial types, procedures | Document as out-of-scope |

This analysis determines what gets built next and prevents wasted effort on
constructs that are blocked by prerequisites.

### 4.3 — Establish a Regression Floor (HIGH VALUE)

**What rs-polygraph did**: Froze a baseline at `tests/tck/baseline/scenarios.jsonl`.
A diff tool (`tools/tck_diff.sh`) exits non-zero on any regression. **No phase
merges if TCK drops below the floor.**

**pg_eddy's current state**: The TCK badge in README is updated manually. There
is no automated regression floor.

**Recommendation**:
- Record the current TCK pass count (2391) as the floor
- Add a check to CI or to the release checklist: "TCK pass count must be ≥ FLOOR"
- Update FLOOR after each release that improves the pass rate
- Consider writing a small script that compares current vs baseline and fails
  if regressions are found

### 4.4 — Tag Scenario-Specific Patches in Code (MEDIUM VALUE)

**What rs-polygraph did**: Introduced comment markers in their codebase:
- `// NORMALIZATION(openCypher 9 §X.Y):` — rule derivable from the spec
- `// SCENARIO-PATCH(TCK-ids):` — fix that only targets specific test scenarios
- `// LOSSY-SEMANTICS(spec-ref):` — output deviates from spec intentionally

Goal: drive `SCENARIO-PATCH` count to zero. Every translation rule should be
derivable from the spec, not reverse-engineered from test output.

**Relevance to pg_eddy**: pg_eddy's executor directly implements openCypher
semantics in Rust, so "scenario patches" are less likely than in a transpiler.
However, the principle still applies — if any code exists solely to make a
specific TCK scenario pass (rather than implementing a general spec rule), tag it.

**Recommendation**: When fixing TCK failures, add a brief spec reference:
```rust
// openCypher 9 §6.3.3 — null propagation in comparison operators
```
This prevents future maintainers from removing "dead code" that actually
implements a spec requirement.

### 4.5 — Spec-Cite Unsupported Constructs (MEDIUM VALUE)

**What rs-polygraph did**: Introduced a structured error variant:
```rust
PolygraphError::Unsupported { construct, spec_ref, reason }
```
Every unsupported construct has a spec reference explaining *why* it's
unsupported and whether it's temporarily or permanently blocked.

**Recommendation**: pg_eddy's `PgEddyError` enum could benefit from the same
pattern for features that are parsed but not yet executed:
```rust
PgEddyError::PE020 { construct: "FOREACH", spec_ref: "openCypher 9 §3.3.14" }
```
This turns "query failed" into "this specific construct is not yet supported, per
this section of the spec."

### 4.6 — Consider a Logical Algebra IR — But Not Yet (LOW PRIORITY NOW)

**What rs-polygraph did**: Inserted an LQA (Logical Query Algebra) between AST
and SPARQL. This was their most significant architectural change — and took weeks.

**Why pg_eddy doesn't need this yet**: pg_eddy already has a `LogicalPlan` enum
that serves as an intermediate representation between AST and execution. The 18
plan node types (LabelScan, Expand, VarLengthExpand, Filter, Project, etc.) are
analogous to rs-polygraph's LQA operators. pg_eddy's `LogicalPlan` is:

- Simpler (no need to express SPARQL constraints)
- Directly executable (no second lowering step)
- Adequate for the current pipeline

**When this changes**: An algebra IR becomes valuable when:
1. **Multiple execution strategies** exist (e.g., columnar execution, vectorized
   processing, parallel query) and the planner must choose between them
2. **Optimization passes** need to compose (predicate pushdown, join reordering,
   common subexpression elimination)
3. **Cost-based optimization** requires a canonical form for cardinality
   estimation

None of these are immediate priorities. The current planner is heuristic-driven
and correct. Optimization can be layered in later.

**Recommendation**: Keep the current `LogicalPlan` structure. Revisit an algebra
IR when property indexes land (enabling index-driven scan selection) or when
parallel execution is implemented.

### 4.7 — The Grammar Question: Hand-Rolled vs Generated (INFORMATIONAL)

**What rs-polygraph found**: They evaluated ANTLR, tree-sitter-cypher, and
extending their existing pest grammar. Conclusion: **extend the existing grammar**
because (a) zero TCK failures were grammar-related, and (b) the existing parser
already covered 100% of the TCK surface.

**pg_eddy's situation**: The hand-rolled recursive-descent parser is comprehensive
(all clauses through v0.14.0+). Parser bugs account for some TCK failures
(pattern expressions as predicates), but these are specific gaps, not
architectural problems.

**Recommendation**: Keep the hand-rolled parser. It gives full control over error
messages, recovery, and span tracking. Augment it with a fuzz harness once the
current TCK failure backlog is under control — that's when grammar edge cases
become the binding constraint.

---

## 5. What Does NOT Apply

Several aspects of rs-polygraph's pivot are irrelevant or counterproductive for
pg_eddy:

### 5.1 — Strangler-Fig Migration

rs-polygraph needed this because they were replacing one translator path (AST →
SPARQL) with another (AST → LQA → SPARQL) while keeping the old one as a safety
net. pg_eddy has a single execution path (AST → LogicalPlan → Executor). There's
nothing to strangle — new features are added directly.

### 5.2 — Target Engine Abstraction

rs-polygraph's `TargetEngine` trait allows targeting different SPARQL backends
(Oxigraph, Jena, Stardog). pg_eddy has exactly one backend: its own custom
Table AM. No abstraction needed.

### 5.3 — Multi-Phase / Continuation Runtime

rs-polygraph introduced a `TranspileOutput::Continuation` type to handle
constructs that require multiple SPARQL round-trips (e.g., runtime list
materialization). pg_eddy executes everything in-process — there are no
round-trips. Any construct that requires iterating over intermediate results
just... does that in the executor.

### 5.4 — Lossy Semantics Tracking

rs-polygraph must document where SPARQL output deviates from openCypher semantics
(e.g., lists serialized to strings, collect() → GROUP_CONCAT). pg_eddy has no
such semantic gap — it implements openCypher directly. If something behaves
wrong, it's a bug, not a translation limitation.

---

## 6. Actionable Recommendations (Ordered by Priority)

| # | Recommendation | Effort | Value | When |
|---|---------------|--------|-------|------|
| 1 | **Classify all ~1489 TCK failures into buckets** | 1 session | Drives the entire development roadmap | Now |
| 2 | **Build a curated difftest suite (50+ queries)** | 2–3 sessions | Catches bugs TCK misses; regression oracle for areas with thin coverage | Next release |
| 3 | **Establish a TCK regression floor** (baseline count, fail CI on regression) | Small script | Prevents silent regressions | Now |
| 4 | **Add spec references to non-obvious Cypher semantics** in executor/planner | Incremental | Prevents accidental removal of correct behavior | Ongoing |
| 5 | **Structured `Unsupported` errors** with spec references | Small refactor | Better DX for users hitting unimplemented features | Next release |
| 6 | **Fuzz the parser** once TCK backlog is under control | Medium | Finds grammar edge cases before users do | After 75%+ TCK |
| 7 | **Algebra IR / cost-based optimizer** when property indexes land | Large | Enables index selection and join reordering | v0.15.0+ |

---

## 7. Summary

rs-polygraph's pivot was driven by the realization that **TCK-driven development
produces a system that passes tests but doesn't generalize to arbitrary user
queries**. Their remediation — a spec-grounded IR, grammar hardening, and
differential testing — is impressive engineering for a transpiler constrained by
SPARQL's semantic limits.

pg_eddy is in a fundamentally stronger position: it executes Cypher natively with
no translation target constraints. Every TCK failure is fixable. The lessons that
transfer are about **process and quality infrastructure** (exhaustive failure
classification, regression floors, differential testing, spec citations), not
about architecture. pg_eddy's current pipeline — hand-rolled parser, heuristic
planner, interpreter executor — is sound and can scale to full openCypher
compliance without the kind of architectural pivot rs-polygraph needed.
