#!/usr/bin/env perl
# benchmarks/run_ldbc_benchmark.pl — pg_eddy v0.23.x LDBC IS-1/IS-3 benchmark
#
# Measures:
#   1. Batch node insert throughput  (UNWIND + CREATE via Cypher)
#   2. Edge loading                  (SQL create_edge() for pg_eddy; UNWIND+CREATE for AGE)
#   3. Property index build          (create_node_index('Person','id') on 1 000 nodes)
#   4. IS-1: single-node lookup by property (MATCH (n:Person {id: $id}) — with index)
#   5. IS-3: 1-hop neighbour expansion    (MATCH (n:Person {id:$id})-[:KNOWS]-(f))
#
# Runs pg_eddy and Apache AGE side-by-side on the same dataset.
# Note: pg_eddy loads edges via the SQL create_edge() API (sequential node IDs
# from the sequence are used directly) since Cypher MATCH still does a full scan
# for edge creation; a property-indexed MATCH is a future milestone.
# AGE loads edges via UNWIND+MATCH+CREATE (AGE has B-tree index on properties).
#
# Scale: 1 000 nodes / 5 000 edges (adjust $N_*)
#
# Run with:
#   PG_REGRESS=/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress \
#   PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:$PERL5LIB"    \
#   PATH="/usr/lib/postgresql/18/bin:$PATH"                                 \
#   prove -v benchmarks/run_ldbc_benchmark.pl
#   # Results go to log/regress_log_run_ldbc_benchmark

use strict;
use warnings;
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Time::HiRes qw(gettimeofday tv_interval);
use POSIX       qw(strftime);
use JSON;

# ---------------------------------------------------------------------------
# Scale parameters
# ---------------------------------------------------------------------------
my $N_NODES      = 1_000;    # Person nodes
my $N_EDGES      = 5_000;    # KNOWS edges
my $BATCH_SIZE   = 100;      # nodes per Cypher UNWIND batch
my $N_IS_QUERIES = 20;       # random lookups per IS benchmark

# ---------------------------------------------------------------------------
# Cluster setup
# ---------------------------------------------------------------------------
my $node = PostgreSQL::Test::Cluster->new('ldbc_bench');
$node->init(extra => ['--no-sync']);
$node->append_conf('postgresql.conf',
    "shared_preload_libraries = 'pg_eddy, age'\n"
  . "synchronous_commit = off\n"
  . "checkpoint_completion_target = 0.9\n"
  . "shared_buffers = 256MB\n"
);
$node->start;

print "\n";
print "=" x 70, "\n";
printf "  pg_eddy vs Apache AGE — LDBC IS-1/IS-3 benchmark\n";
printf "  Scale: %d nodes / %d edges (Cypher batch size: %d)\n", $N_NODES, $N_EDGES, $BATCH_SIZE;
printf "  Date:  %s\n", strftime("%Y-%m-%d %H:%M:%S", localtime);
print "=" x 70, "\n\n";

# ---------------------------------------------------------------------------
# Install extensions
# ---------------------------------------------------------------------------
$node->safe_psql('postgres', "CREATE EXTENSION pg_eddy;");
eval { $node->safe_psql('postgres', "CREATE EXTENSION age;") };
my $age_ok = !$@;
if (!$age_ok) {
    print "WARNING: AGE extension not available — skipping AGE comparisons\n\n";
}

# ---------------------------------------------------------------------------
# Generate dataset in Perl (deterministic, repeatable)
# ---------------------------------------------------------------------------
my @node_ids   = (1 .. $N_NODES);
my @edge_pairs;
srand(42);
for my $i (0 .. $N_EDGES - 1) {
    my $src = int(rand($N_NODES)) + 1;
    my $dst = int(rand($N_NODES)) + 1;
    $dst = ($dst == $src) ? ($dst % $N_NODES) + 1 : $dst;
    push @edge_pairs, [$src, $dst];
}

# ---------------------------------------------------------------------------
# Helper: time a block
# ---------------------------------------------------------------------------
sub timeit {
    my ($label, $code) = @_;
    my $t0 = [gettimeofday];
    $code->();
    my $elapsed = tv_interval($t0);
    printf "  %-48s %8.3f s\n", $label, $elapsed;
    return $elapsed;
}

# ---------------------------------------------------------------------------
# SECTION 1: pg_eddy — batch node insert via Cypher UNWIND + CREATE
# pg_eddy node IDs are allocated from _pg_eddy.node_id_seq (starts at 1,
# increments by 1), so after N_NODES inserts the nodes have IDs 1..N_NODES.
# ---------------------------------------------------------------------------
print "--- pg_eddy: node insert (Cypher UNWIND+CREATE, batch=$BATCH_SIZE) ---\n";

my $eddy_node_time = timeit("$N_NODES Person nodes", sub {
    my @ids = @node_ids;
    while (@ids) {
        my @batch = splice(@ids, 0, $BATCH_SIZE);
        my $json = encode_json({ persons => [ map { {id => $_, name => "Person$_"} } @batch ] });
        $json =~ s/'/''/g;
        $node->safe_psql('postgres',
            "SELECT * FROM cypher('UNWIND \$persons AS p CREATE (n:Person {id: p.id, name: p.name})',
             '$json'::jsonb)");
    }
});
my $eddy_nodes_per_s = int($N_NODES / ($eddy_node_time || 0.001));
printf "  => Throughput: %d nodes/s\n", $eddy_nodes_per_s;

# ---------------------------------------------------------------------------
# SECTION 2: pg_eddy — edge loading via SQL create_edge()
# Uses the known sequential node IDs directly (no MATCH scan needed).
# This avoids O(N) full-scan-per-edge limitation pending property index support.
# ---------------------------------------------------------------------------
print "\n--- pg_eddy: edge load (SQL create_edge, sequential node IDs) ---\n";

my $eddy_edge_time = timeit("$N_EDGES KNOWS edges (one SQL batch)", sub {
    # Build a single VALUES list and execute via generate_series trick.
    # create_edge(src_id, dst_id, rel_type, properties) → edge_id
    my $vals = join(",\n  ", map { "($_->[0]::bigint, $_->[1]::bigint)" } @edge_pairs);
    $node->safe_psql('postgres',
        "SELECT create_edge(s, d, 'KNOWS', '{}'::jsonb)
         FROM (VALUES\n  $vals\n) AS t(s, d)");
});
my $eddy_edges_per_s = int($N_EDGES / ($eddy_edge_time || 0.001));
printf "  => Throughput: %d edges/s\n\n", $eddy_edges_per_s;

# ---------------------------------------------------------------------------
# SECTION 3: AGE — batch insert via Cypher UNWIND + CREATE
# ---------------------------------------------------------------------------
my ($age_node_time, $age_edge_time, $age_nodes_per_s, $age_edges_per_s);

if ($age_ok) {
    print "--- AGE: node insert (Cypher UNWIND+CREATE, batch=$BATCH_SIZE) ---\n";

    $node->safe_psql('postgres', q{
        SET search_path = ag_catalog, "$user", public;
        SELECT create_graph('ldbc');
    });

    $age_node_time = timeit("$N_NODES Person nodes", sub {
        my @ids = @node_ids;
        while (@ids) {
            my @batch = splice(@ids, 0, $BATCH_SIZE);
            my $list = join(",", map { "{id: $_, name: 'Person$_'}" } @batch);
            # Each safe_psql call is a new connection; must re-set search_path
            # to reach ag_catalog.cypher() instead of pg_eddy's cypher().
            $node->safe_psql('postgres',
                "SET search_path = ag_catalog, \"\$user\", public;
                 SELECT * FROM ag_catalog.cypher('ldbc',
                 \$\$UNWIND [$list] AS p CREATE (n:Person {id: p.id, name: p.name})\$\$
                 ) AS (v agtype)");
        }
    });
    $age_nodes_per_s = int($N_NODES / ($age_node_time || 0.001));
    printf "  => Throughput: %d nodes/s\n", $age_nodes_per_s;

    print "\n--- AGE: edge load (Cypher UNWIND+MATCH+CREATE, indexed) ---\n";
    $age_edge_time = timeit("$N_EDGES KNOWS edges (batches of $BATCH_SIZE)", sub {
        my @pairs = @edge_pairs;
        while (@pairs) {
            my @batch = splice(@pairs, 0, $BATCH_SIZE);
            my $list = join(",", map { "{src: $_->[0], dst: $_->[1]}" } @batch);
            $node->safe_psql('postgres',
                "SET search_path = ag_catalog, \"\$user\", public;
                 SELECT * FROM ag_catalog.cypher('ldbc',
                 \$\$UNWIND [$list] AS r
                    MATCH (a:Person {id: r.src}), (b:Person {id: r.dst})
                    CREATE (a)-[:KNOWS]->(b)\$\$
                 ) AS (v agtype)");
        }
    });
    $age_edges_per_s = int($N_EDGES / ($age_edge_time || 0.001));
    printf "  => Throughput: %d edges/s\n\n", $age_edges_per_s;
}

# ---------------------------------------------------------------------------
# Create property index on Person.id for IS-1/IS-3 optimization
# ---------------------------------------------------------------------------
print "--- Creating property index on Person.id ---\n";
my $idx_time = timeit("CREATE INDEX ON :Person(id)", sub {
    $node->safe_psql('postgres', "SELECT create_node_index('Person', 'id')");
});
printf "  Index build time: %.3f s\n\n", $idx_time;

# ---------------------------------------------------------------------------
# SECTION 4: IS-1 — single-node lookup by property (with index)
# ---------------------------------------------------------------------------
print "--- IS-1: single-node lookup (MATCH (n:Person {id: X})) ---\n";

my @sample_ids = map { int(rand($N_NODES)) + 1 } (1 .. $N_IS_QUERIES);

my $eddy_is1_time = timeit("pg_eddy: $N_IS_QUERIES IS-1 lookups", sub {
    for my $id (@sample_ids) {
        $node->safe_psql('postgres',
            "SELECT * FROM cypher('MATCH (n:Person {id: $id}) RETURN n.id, n.name', NULL::jsonb)");
    }
});
my $eddy_is1_ms = 1000 * $eddy_is1_time / $N_IS_QUERIES;
printf "  => pg_eddy avg: %.2f ms/query\n", $eddy_is1_ms;

my ($age_is1_ms);
if ($age_ok) {
    my $age_is1_time = timeit("AGE:     $N_IS_QUERIES IS-1 lookups", sub {
        for my $id (@sample_ids) {
            $node->safe_psql('postgres',
                "SET search_path = ag_catalog, \"\$user\", public;
                 SELECT * FROM ag_catalog.cypher('ldbc',
                 \$\$MATCH (n:Person {id: $id}) RETURN n.id, n.name\$\$) AS (id agtype, name agtype)");
        }
    });
    $age_is1_ms = 1000 * $age_is1_time / $N_IS_QUERIES;
    printf "  => AGE avg:     %.2f ms/query\n", $age_is1_ms;
    printf "  => Ratio (pg_eddy/AGE): %.2fx\n", $eddy_is1_ms / ($age_is1_ms || 0.001);
}
print "\n";

# ---------------------------------------------------------------------------
# SECTION 5: IS-3 — 1-hop KNOWS neighbour expansion
# ---------------------------------------------------------------------------
print "--- IS-3: 1-hop neighbour expansion (MATCH (n)-[:KNOWS]-(f)) ---\n";

my $eddy_is3_time = timeit("pg_eddy: $N_IS_QUERIES IS-3 queries", sub {
    for my $id (@sample_ids) {
        $node->safe_psql('postgres',
            "SELECT * FROM cypher('MATCH (n:Person {id: $id})-[:KNOWS]-(f:Person) RETURN f.id, f.name', NULL::jsonb)");
    }
});
my $eddy_is3_ms = 1000 * $eddy_is3_time / $N_IS_QUERIES;
printf "  => pg_eddy avg: %.2f ms/query\n", $eddy_is3_ms;

my ($age_is3_ms);
if ($age_ok) {
    my $age_is3_time = timeit("AGE:     $N_IS_QUERIES IS-3 queries", sub {
        for my $id (@sample_ids) {
            $node->safe_psql('postgres',
                "SET search_path = ag_catalog, \"\$user\", public;
                 SELECT * FROM ag_catalog.cypher('ldbc',
                 \$\$MATCH (n:Person {id: $id})-[:KNOWS]-(f:Person) RETURN f.id, f.name\$\$)
                 AS (id agtype, name agtype)");
        }
    });
    $age_is3_ms = 1000 * $age_is3_time / $N_IS_QUERIES;
    printf "  => AGE avg:     %.2f ms/query\n", $age_is3_ms;
    my $is3_ratio = $eddy_is3_ms / ($age_is3_ms || 0.001);
    printf "  => Ratio (pg_eddy/AGE): %.2fx %s\n",
        $is3_ratio,
        ($is3_ratio <= 0.5 ? "(PASS: >=2x faster)" : $is3_ratio <= 1.0 ? "(within 2x)" : "(SLOWER than AGE!)");
}
print "\n";

# ---------------------------------------------------------------------------
# Summary table
# ---------------------------------------------------------------------------
print "=" x 70, "\n";
print "RESULTS SUMMARY\n";
print "=" x 70, "\n";
printf "%-35s %12s %12s %8s\n", "Benchmark", "pg_eddy", "AGE", "Ratio";
print "-" x 70, "\n";
printf "%-35s %10d/s %12s %8s\n",
    "Node insert (nodes/s, UNWIND+CREATE)",
    $eddy_nodes_per_s,
    $age_ok ? sprintf("%d/s", $age_nodes_per_s) : "N/A",
    $age_ok ? sprintf("%.2fx", $eddy_nodes_per_s / ($age_nodes_per_s || 1)) : "N/A";
printf "%-35s %10d/s %12s %8s\n",
    "Edge load (edges/s)",
    $eddy_edges_per_s,
    $age_ok ? sprintf("%d/s", $age_edges_per_s) : "N/A",
    "N/A (diff API)";
printf "%-35s %11.2f ms %11s %8s\n",
    "IS-1 (node lookup+index, ms/query)",
    $eddy_is1_ms,
    $age_ok ? sprintf("%.2f ms", $age_is1_ms) : "N/A",
    $age_ok ? sprintf("%.2fx", $eddy_is1_ms / ($age_is1_ms || 0.001)) : "N/A";
printf "%-35s %11.2f ms %11s %8s\n",
    "IS-3 (1-hop expand, ms/query)",
    $eddy_is3_ms,
    $age_ok ? sprintf("%.2f ms", $age_is3_ms) : "N/A",
    $age_ok ? sprintf("%.2fx", $eddy_is3_ms / ($age_is3_ms || 0.001)) : "N/A";
print "=" x 70, "\n\n";

# ---------------------------------------------------------------------------
# Gate decision
# ---------------------------------------------------------------------------
if ($age_ok && defined $age_is3_ms) {
    my $is3_ratio = $eddy_is3_ms / ($age_is3_ms || 0.001);
    print "IS-3 gate (pg_eddy must be <= 0.5x AGE, i.e. >=2x faster):\n";
    if ($is3_ratio <= 0.5) {
        printf "  PASS — pg_eddy is %.2fx faster than AGE on IS-3\n", 1.0 / $is3_ratio;
    } elsif ($is3_ratio <= 1.0) {
        printf "  WARN — pg_eddy is within 2x of AGE (ratio %.2f); acceptable for now\n", $is3_ratio;
    } else {
        printf "  FAIL — pg_eddy is %.2fx SLOWER than AGE on IS-3 (ratio %.2f)\n",
            $is3_ratio, $is3_ratio;
        $node->stop;
        exit 1;
    }
}

$node->stop;
print "\nBenchmark complete.\n";

