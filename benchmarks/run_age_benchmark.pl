#!/usr/bin/env perl
# benchmarks/run_age_benchmark.pl
#
# AGE vs pg_eddy comparison benchmark (v0.5.1 gate).
#
# Uses PostgreSQL::Test::Cluster (ships with postgresql-server-dev-18) to
# spin up an ephemeral cluster so it works inside a dev container without a
# running system PostgreSQL.
#
# Run with:
#   PG_REGRESS=/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress \
#   PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:$PERL5LIB"    \
#   PATH="/usr/lib/postgresql/18/bin:$PATH"                                 \
#   perl benchmarks/run_age_benchmark.pl

use strict;
use warnings;
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Time::HiRes qw(gettimeofday tv_interval);
use POSIX       qw(strftime);

# ---------------------------------------------------------------------------
# Scale parameters — keep small so the benchmark finishes in a dev container.
# The README target scale is 100 000 nodes / 1 000 000 edges; results here
# are recorded at 1/50 scale and marked as such.
# ---------------------------------------------------------------------------
my $N_NODES   = 1_000;  # nodes to insert in the insert benchmark
my $N_EDGES   = 5_000;  # edges to insert for the traversal benchmarks
my $N_QUERIES = 10;     # random nodes to sample per traversal benchmark

# ---------------------------------------------------------------------------
# Cluster setup
# ---------------------------------------------------------------------------
$ENV{PGHOST} = '/tmp';

my $node = PostgreSQL::Test::Cluster->new('bench_node');
$node->init(extra => ['--no-sync']);
$node->append_conf(
    'postgresql.conf',
    "shared_preload_libraries = 'pg_eddy, age'\n"
    . "synchronous_commit = off\n"
    . "checkpoint_completion_target = 0.9\n"
);
$node->start;

print "\n";
print "=" x 66, "\n";
print "  pg_eddy v0.5.1 vs Apache AGE 1.7.0-rc0  —  benchmark\n";
print "  Scale: $N_NODES nodes / $N_EDGES edges / $N_QUERIES queries\n";
print "  Date:  ", strftime("%Y-%m-%d %H:%M:%S", localtime), "\n";
print "=" x 66, "\n\n";

# ---------------------------------------------------------------------------
# Install extensions
# ---------------------------------------------------------------------------
$node->safe_psql('postgres', "CREATE EXTENSION pg_eddy;");
$node->safe_psql('postgres', "CREATE EXTENSION age;");
$node->safe_psql('postgres', q{
    SET search_path = ag_catalog, "$user", public;
    SELECT create_graph('bench');
});

# ---------------------------------------------------------------------------
# Benchmark 1 — Node insert throughput
# ---------------------------------------------------------------------------
print "--- Benchmark 1: Insert $N_NODES nodes ---\n";

# pg_eddy: loop in a DO block (same pattern as TAP tests)
my $t0 = [gettimeofday];
$node->safe_psql('postgres', qq{
    DO \$\$
    DECLARE i INT;
    BEGIN
        FOR i IN 1..$N_NODES LOOP
            PERFORM create_node(
                ARRAY['Node']::text[],
                ('{"seq":' || i || '}')::jsonb
            );
        END LOOP;
    END \$\$;
});
my $eddy_ins_time = tv_interval($t0);
my $eddy_ins_rate = $N_NODES / $eddy_ins_time;

# AGE: bulk insert via Cypher UNWIND range()
$t0 = [gettimeofday];
$node->safe_psql('postgres', qq{
    SET search_path = ag_catalog, "\$user", public;
    SELECT count(*) FROM cypher('bench', \$\$
        UNWIND range(1, $N_NODES) AS i
        CREATE (n:Node {seq: i})
    \$\$) AS (v agtype);
});
my $age_ins_time = tv_interval($t0);
my $age_ins_rate = $N_NODES / $age_ins_time;

my $ins_ratio = $eddy_ins_rate / $age_ins_rate;
printf "  pg_eddy : %7.3f s  (%7.0f nodes/s)\n", $eddy_ins_time, $eddy_ins_rate;
printf "  AGE     : %7.3f s  (%7.0f nodes/s)\n", $age_ins_time,  $age_ins_rate;
printf "  Ratio   : %.2fx  %s\n\n",
    $ins_ratio,
    ($ins_ratio >= 1.0 ? "(pg_eddy faster)" : "(AGE faster)");

# ---------------------------------------------------------------------------
# Setup — edge insertion (not part of primary benchmark table, but reported)
# ---------------------------------------------------------------------------
print "--- Setup: Inserting $N_EDGES edges ---\n";

# Precompute random (src, dst) pairs shared by both engines so we compare
# the same graph topology.
srand(42);
my @src_seqs = map { 1 + int(rand($N_NODES)) } 1..$N_EDGES;
my @dst_seqs = map { 1 + int(rand($N_NODES)) } 1..$N_EDGES;

# pg_eddy: create_edge(src_node_id, dst_node_id, rel_type, props)
# Node IDs in pg_eddy are sequential starting at 1, matching the seq values.
# Materialize the same pairs into a temp table, then drive the loop from it.
my $pairs_values = join(",\n", map {
    "($src_seqs[$_], $dst_seqs[$_])"
} 0..$#src_seqs);

$t0 = [gettimeofday];
$node->safe_psql('postgres', qq{
    CREATE TEMP TABLE IF NOT EXISTS bench_pairs (src BIGINT, dst BIGINT);
    INSERT INTO bench_pairs VALUES $pairs_values;
    DO \$\$
    DECLARE r RECORD;
    BEGIN
        FOR r IN SELECT src, dst FROM bench_pairs LOOP
            PERFORM create_edge(r.src, r.dst, 'KNOWS', '{}'::jsonb);
        END LOOP;
    END \$\$;
    DROP TABLE bench_pairs;
});
my $eddy_edge_time = tv_interval($t0);
printf "  pg_eddy edges : %.3f s  (%.0f edges/s)\n", $eddy_edge_time, $N_EDGES / $eddy_edge_time;

# AGE: bulk edge insertion via a PL/pgSQL DO block.
# AGE's cypher() third argument MUST be a PL/pgSQL variable (not a string
# literal), so we use: DECLARE params agtype := $JSON$...JSON$::agtype; THEN
# pass `params` as the variable.  We match nodes by seq property (full scan
# — AGE has no property index by default at this scale).

my @pairs_json = map { qq({"src":$src_seqs[$_],"dst":$dst_seqs[$_]}) }
                 0..$#src_seqs;
my $pairs_json_str = '{"pairs":[' . join(',', @pairs_json) . ']}';

$t0 = [gettimeofday];
$node->safe_psql('postgres', qq{
    SET search_path = ag_catalog, "\$user", public;
    DO \$BENCH\$
    DECLARE params agtype;
    BEGIN
        params := \$JSON\$$pairs_json_str\$JSON\$::agtype;
        PERFORM count(*) FROM cypher('bench', \$CYP\$
            UNWIND \$pairs AS p
            MATCH (a:Node), (b:Node)
            WHERE a.seq = p.src AND b.seq = p.dst
            CREATE (a)-[:KNOWS]->(b)
        \$CYP\$, params) AS (v agtype);
    END \$BENCH\$;
});
my $age_edge_time = tv_interval($t0);
printf "  AGE edges     : %.3f s  (%.0f edges/s)\n\n",
    $age_edge_time, $N_EDGES / $age_edge_time;

# ---------------------------------------------------------------------------
# Pick N_QUERIES random node IDs (seq values = pg_eddy node_ids for this graph)
# ---------------------------------------------------------------------------
my @query_nodes = map { 1 + int(rand($N_NODES)) } 1..$N_QUERIES;

# ---------------------------------------------------------------------------
# Benchmark 2 — 1-hop adjacency follow
# ---------------------------------------------------------------------------
print "--- Benchmark 2: 1-hop adjacency ($N_QUERIES queries) ---\n";

$t0 = [gettimeofday];
for my $nid (@query_nodes) {
    $node->safe_psql('postgres',
        "SELECT count(*) FROM neighbours($nid, 'OUT', NULL)");
}
my $eddy_1hop_ms = tv_interval($t0) / $N_QUERIES * 1000;

$t0 = [gettimeofday];
for my $seq (@query_nodes) {
    $node->safe_psql('postgres', qq{
        SET search_path = ag_catalog, "\$user", public;
        SELECT count(*) FROM cypher('bench', \$\$
            MATCH (a:Node {seq: $seq})-[:KNOWS]->(b)
            RETURN b
        \$\$) AS (b agtype);
    });
}
my $age_1hop_ms = tv_interval($t0) / $N_QUERIES * 1000;

my $hop1_ratio = $age_1hop_ms / $eddy_1hop_ms;
printf "  pg_eddy : %7.2f ms/query\n", $eddy_1hop_ms;
printf "  AGE     : %7.2f ms/query\n", $age_1hop_ms;
printf "  Ratio   : %.2fx  %s\n\n",
    $hop1_ratio,
    ($hop1_ratio >= 1.0 ? "(pg_eddy faster)" : "(AGE faster)");

# ---------------------------------------------------------------------------
# Benchmark 3 — 2-hop neighbour expansion
# ---------------------------------------------------------------------------
print "--- Benchmark 3: 2-hop expansion ($N_QUERIES queries) ---\n";

$t0 = [gettimeofday];
for my $nid (@query_nodes) {
    $node->safe_psql('postgres', qq{
        SELECT count(*) FROM (
            SELECT neighbours(n, 'OUT', NULL)
            FROM   neighbours($nid, 'OUT', NULL) AS n
        ) t
    });
}
my $eddy_2hop_ms = tv_interval($t0) / $N_QUERIES * 1000;

$t0 = [gettimeofday];
for my $seq (@query_nodes) {
    $node->safe_psql('postgres', qq{
        SET search_path = ag_catalog, "\$user", public;
        SELECT count(*) FROM cypher('bench', \$\$
            MATCH (a:Node {seq: $seq})-[:KNOWS*2]->(b)
            RETURN DISTINCT b
        \$\$) AS (b agtype);
    });
}
my $age_2hop_ms = tv_interval($t0) / $N_QUERIES * 1000;

my $hop2_ratio = $age_2hop_ms / $eddy_2hop_ms;
printf "  pg_eddy : %7.2f ms/query\n", $eddy_2hop_ms;
printf "  AGE     : %7.2f ms/query\n", $age_2hop_ms;
printf "  Ratio   : %.2fx  %s\n\n",
    $hop2_ratio,
    ($hop2_ratio >= 1.0 ? "(pg_eddy faster)" : "(AGE faster)");

# ---------------------------------------------------------------------------
# Summary & gate decision
# ---------------------------------------------------------------------------
print "=" x 66, "\n";
print "  SUMMARY  (scale: $N_NODES nodes / $N_EDGES edges)\n";
print "=" x 66, "\n";
printf "  %-28s  pg_eddy=%7.0f/s   AGE=%7.0f/s   %.2fx\n",
    "Node insert throughput:", $eddy_ins_rate, $age_ins_rate, $ins_ratio;
printf "  %-28s  pg_eddy=%7.2f ms  AGE=%7.2f ms  %.2fx\n",
    "1-hop avg latency:", $eddy_1hop_ms, $age_1hop_ms, $hop1_ratio;
printf "  %-28s  pg_eddy=%7.2f ms  AGE=%7.2f ms  %.2fx\n",
    "2-hop avg latency:", $eddy_2hop_ms, $age_2hop_ms, $hop2_ratio;

print "\n";
my $gate;
if ($hop2_ratio >= 2.0) {
    $gate = "PROCEED to v0.6.0 — pg_eddy >=2x faster on 2-hop expand";
} elsif ($ins_ratio >= 1.0 && $hop1_ratio >= 1.0 && $hop2_ratio >= 1.0) {
    $gate = "PROCEED (file P1 storage bugs) — pg_eddy 1-2x faster on all";
} else {
    $gate = "STOP — pg_eddy slower on at least one metric; fix in v0.5.2";
}
print "  GATE DECISION: $gate\n\n";

$node->stop;

# ---------------------------------------------------------------------------
# Emit markdown snippet for README
# ---------------------------------------------------------------------------
my $date = strftime("%Y-%m-%d", localtime);
print "=" x 66, "\n";
print "  README snippet (copy-paste into benchmarks/README.md)\n";
print "=" x 66, "\n";
print <<MD;

| Field | Value |
|---|---|
| Date | $date |
| Hardware | dev container (Debian 11, codespaces) |
| PostgreSQL version | 18 (pgdg) |
| pg_eddy version | 0.5.1 |
| Apache AGE version | 1.7.0-rc0 |
| Dataset (this run) | $N_NODES nodes / $N_EDGES edges (1/50 scale) |

### 1. Node insert throughput ($N_NODES nodes)

| Engine | Time (s) | Throughput (nodes/s) |
|---|---|---|
| pg_eddy | @{[sprintf "%.3f", $eddy_ins_time]} | @{[sprintf "%.0f", $eddy_ins_rate]} |
| AGE | @{[sprintf "%.3f", $age_ins_time]} | @{[sprintf "%.0f", $age_ins_rate]} |
| **Ratio (pg_eddy / AGE)** | — | @{[sprintf "%.2f", $ins_ratio]}x |

### 2. 1-hop adjacency ($N_QUERIES queries, avg)

| Engine | Time (ms/query) |
|---|---|
| pg_eddy `neighbours()` | @{[sprintf "%.2f", $eddy_1hop_ms]} |
| AGE Cypher MATCH 1-hop | @{[sprintf "%.2f", $age_1hop_ms]} |
| **Ratio (pg_eddy / AGE)** | @{[sprintf "%.2f", $hop1_ratio]}x |

### 3. 2-hop expansion ($N_QUERIES queries, avg)

| Engine | Time (ms/query) |
|---|---|
| pg_eddy nested `neighbours()` | @{[sprintf "%.2f", $eddy_2hop_ms]} |
| AGE Cypher `[:KNOWS*2]` | @{[sprintf "%.2f", $age_2hop_ms]} |
| **Ratio (pg_eddy / AGE)** | @{[sprintf "%.2f", $hop2_ratio]}x |

**Gate decision**: $gate
MD
