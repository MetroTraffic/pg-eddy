#!/usr/bin/env perl
# benchmarks/run_prop_benchmark.pl — Property-rich traversal benchmark
#
# Purpose: demonstrate and guard the OPT-2 catalog name cache, OPT-3 OID
# cache, and OPT-6 chain coalescing on workloads that actually exercise them.
#
# The LDBC IS-3 benchmark uses nodes with only 2 properties (id, name).
# At that scale OPT-2 saves at most 2 SPI calls per decoded node — invisible
# in the noise.  Here each node has 7 properties, and the full-graph PB-1
# query decodes every destination node in a single cypher() call:
#
#   Without OPT-2: N_EDGES × 7 SPI calls for prop_key_name() = ~70 000 SPI
#   With    OPT-2: 7 SPI calls to warm the cache, then 0 per subsequent node
#
# Scale: 2 000 nodes × 7 properties / 10 000 edges
#
# Run (MUST be a release build):
#   cargo pgrx install --release --features pg18
#   rm -rf tmp_check/t_run_prop_benchmark_prop_bench_data
#   PG_REGRESS=/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress \
#   PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl"               \
#   PATH="/usr/lib/postgresql/18/bin:$PATH"                                 \
#   perl benchmarks/run_prop_benchmark.pl

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
my $N_NODES      = 2_000;   # Person nodes, each with 7 properties
my $N_EDGES      = 10_000;  # KNOWS edges (avg out-degree ~5)
my $BATCH_SIZE   = 100;     # nodes per UNWIND batch
my $N_PB_QUERIES = 20;      # queries per PB-2 section

# ---------------------------------------------------------------------------
# Cluster setup
# ---------------------------------------------------------------------------
my $node = PostgreSQL::Test::Cluster->new('prop_bench');
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
printf "  pg_eddy — Property-rich traversal benchmark\n";
printf "  Scale: %d nodes (7 props each) / %d edges\n", $N_NODES, $N_EDGES;
printf "  Targets: OPT-2 catalog cache, OPT-3 OID cache, OPT-6 chain coalescing\n";
printf "  Date:    %s\n", strftime("%Y-%m-%d %H:%M:%S", localtime);
print "=" x 70, "\n\n";

# ---------------------------------------------------------------------------
# Install extensions
# ---------------------------------------------------------------------------
$node->safe_psql('postgres', "CREATE EXTENSION pg_eddy;");
eval { $node->safe_psql('postgres', "CREATE EXTENSION age;") };
my $age_ok = !$@;
print "WARNING: AGE extension not available — skipping AGE comparisons\n\n"
    unless $age_ok;

# ---------------------------------------------------------------------------
# Dataset: deterministic, 7 properties per node
# ---------------------------------------------------------------------------
my @cities    = ("London", "Berlin", "Paris", "Amsterdam", "Madrid", "Rome", "Vienna", "Warsaw");
my @countries = ("UK", "Germany", "France", "Netherlands", "Spain", "Italy", "Austria", "Poland");
my @genders   = ("M", "F");

sub make_node {
    my ($id) = @_;
    return {
        id        => $id,
        firstName => "First$id",
        lastName  => "Last$id",
        age       => 20 + ($id % 50),
        city      => $cities[$id % 8],
        country   => $countries[$id % 8],
        gender    => $genders[$id % 2],
    };
}

my @node_ids = (1 .. $N_NODES);
my @edge_pairs;
srand(42);
for (1 .. $N_EDGES) {
    my $src = int(rand($N_NODES)) + 1;
    my $dst = int(rand($N_NODES)) + 1;
    $dst = ($dst == $src) ? ($dst % $N_NODES) + 1 : $dst;
    push @edge_pairs, [$src, $dst];
}

# ---------------------------------------------------------------------------
# Helper
# ---------------------------------------------------------------------------
sub timeit {
    my ($label, $code) = @_;
    my $t0 = [gettimeofday];
    $code->();
    my $elapsed = tv_interval($t0);
    printf "  %-56s %6.3f s\n", $label, $elapsed;
    return $elapsed;
}

# ---------------------------------------------------------------------------
# SECTION 1: pg_eddy — batch node insert (7 properties per node)
# ---------------------------------------------------------------------------
print "--- pg_eddy: property-rich node insert (7 props/node) ---\n";

my $eddy_node_time = timeit("$N_NODES Person nodes", sub {
    my @ids = @node_ids;
    while (@ids) {
        my @batch = splice(@ids, 0, $BATCH_SIZE);
        my $json = encode_json({ persons => [ map { make_node($_) } @batch ] });
        $json =~ s/'/''/g;
        $node->safe_psql('postgres',
            "SELECT * FROM cypher('UNWIND \$persons AS p "
          . "CREATE (n:Person {id: p.id, firstName: p.firstName, lastName: p.lastName, "
          . "age: p.age, city: p.city, country: p.country, gender: p.gender})', "
          . "'$json'::jsonb)");
    }
});
my $eddy_nodes_per_s = int($N_NODES / ($eddy_node_time || 0.001));
printf "  => Throughput: %d nodes/s\n\n", $eddy_nodes_per_s;

# ---------------------------------------------------------------------------
# SECTION 2: pg_eddy — edge loading
# ---------------------------------------------------------------------------
print "--- pg_eddy: edge load ---\n";

my $eddy_edge_time = timeit("$N_EDGES KNOWS edges", sub {
    my $vals = join(",\n  ", map { "($_->[0]::bigint, $_->[1]::bigint)" } @edge_pairs);
    $node->safe_psql('postgres',
        "SELECT create_edge(s, d, 'KNOWS', '{}'::jsonb) "
      . "FROM (VALUES\n  $vals\n) AS t(s, d)");
});
printf "  => Throughput: %d edges/s\n\n", int($N_EDGES / ($eddy_edge_time || 0.001));

# ---------------------------------------------------------------------------
# SECTION 3: AGE — equivalent dataset
# ---------------------------------------------------------------------------
my ($age_node_time, $age_edge_time);
if ($age_ok) {
    print "--- AGE: node insert (7 props/node) ---\n";
    $node->safe_psql('postgres',
        "SET search_path = ag_catalog, \"\$user\", public; SELECT create_graph('prop_bench');");

    $age_node_time = timeit("$N_NODES Person nodes", sub {
        my @ids = @node_ids;
        while (@ids) {
            my @batch = splice(@ids, 0, $BATCH_SIZE);
            my $list = join(",", map {
                my $n = make_node($_);
                "{id: $n->{id}, firstName: '$n->{firstName}', lastName: '$n->{lastName}', "
              . "age: $n->{age}, city: '$n->{city}', country: '$n->{country}', "
              . "gender: '$n->{gender}'}"
            } @batch);
            $node->safe_psql('postgres',
                "SET search_path = ag_catalog, \"\$user\", public;"
              . "SELECT * FROM ag_catalog.cypher('prop_bench', "
              . "\$\$UNWIND [$list] AS p "
              . "CREATE (n:Person {id: p.id, firstName: p.firstName, lastName: p.lastName, "
              . "age: p.age, city: p.city, country: p.country, gender: p.gender})\$\$"
              . ") AS (v agtype)");
        }
    });
    printf "  => Throughput: %d nodes/s\n\n", int($N_NODES / ($age_node_time || 0.001));

    print "--- AGE: edge load ---\n";
    $age_edge_time = timeit("$N_EDGES KNOWS edges (batches of $BATCH_SIZE)", sub {
        my @pairs = @edge_pairs;
        while (@pairs) {
            my @batch = splice(@pairs, 0, $BATCH_SIZE);
            my $list = join(",", map { "{src: $_->[0], dst: $_->[1]}" } @batch);
            $node->safe_psql('postgres',
                "SET search_path = ag_catalog, \"\$user\", public;"
              . "SELECT * FROM ag_catalog.cypher('prop_bench', "
              . "\$\$UNWIND [$list] AS r "
              . "MATCH (a:Person {id: r.src}), (b:Person {id: r.dst}) "
              . "CREATE (a)-[:KNOWS]->(b)\$\$) AS (v agtype)");
        }
    });
    printf "  => Throughput: %d edges/s\n\n", int($N_EDGES / ($age_edge_time || 0.001));
}

# ---------------------------------------------------------------------------
# Property index on Person.id
# ---------------------------------------------------------------------------
print "--- Build property index on Person.id ---\n";
timeit("CREATE INDEX ON :Person(id)", sub {
    $node->safe_psql('postgres', "SELECT create_node_index('Person', 'id')");
});
print "\n";

# ---------------------------------------------------------------------------
# PB-1: full-graph 1-hop expand — ONE query, decodes every destination node
#
# With OPT-1 (node_location cache), this is now O(1) per destination node
# lookup instead of O(N) sequential scan.  The bottleneck shifts to property
# decoding and buffer reads rather than node lookup.
# ---------------------------------------------------------------------------
print "--- PB-1: full-graph expand (single query, decodes all $N_EDGES edges) ---\n";
print "    NOTE: With OPT-1 (v0.25.0), node lookups are O(1) cache hits.\n";

my $pb1_eddy_time = timeit("pg_eddy: MATCH (n)-[:KNOWS]->(f) RETURN f.id,f.firstName,f.city", sub {
    $node->safe_psql('postgres',
        "SELECT * FROM cypher("
      . "'MATCH (n:Person)-[:KNOWS]->(f) RETURN f.id, f.firstName, f.city', "
      . "NULL::jsonb)");
});
my $pb1_eddy_ms = $pb1_eddy_time * 1000;
printf "  => pg_eddy: %.0f rows/s  (%.0f ms total)\n",
    $N_EDGES / ($pb1_eddy_time || 0.001), $pb1_eddy_ms;

my ($pb1_age_time);
if ($age_ok) {
    $pb1_age_time = timeit("AGE:     MATCH (n)-[:KNOWS]->(f) RETURN f.id,f.firstName,f.city", sub {
        $node->safe_psql('postgres',
            "SET search_path = ag_catalog, \"\$user\", public;"
          . "SELECT * FROM ag_catalog.cypher('prop_bench', "
          . "\$\$MATCH (n:Person)-[:KNOWS]->(f) RETURN f.id, f.firstName, f.city\$\$) "
          . "AS (id agtype, firstName agtype, city agtype)");
    });
    printf "  => AGE:     %.0f rows/s  (%.0f ms total)\n",
        $N_EDGES / ($pb1_age_time || 0.001), $pb1_age_time * 1000;
    printf "  => Ratio (pg_eddy/AGE): %.2fx\n",
        $pb1_eddy_time / ($pb1_age_time || 0.001);
}
print "\n";

# ---------------------------------------------------------------------------
# PB-2: filtered 1-hop with property return (property index + adjacency follow)
# This mirrors IS-3 but returns 3 properties per result node.
# ---------------------------------------------------------------------------
print "--- PB-2: filtered 1-hop with property return ($N_PB_QUERIES queries) ---\n";
print "    MATCH (n:Person {id:X})-[:KNOWS]->(f) RETURN f.id, f.firstName, f.city\n";

my @sample_ids = map { int(rand($N_NODES)) + 1 } (1 .. $N_PB_QUERIES);

my $pb2_eddy_time = timeit("pg_eddy: $N_PB_QUERIES queries", sub {
    for my $id (@sample_ids) {
        $node->safe_psql('postgres',
            "SELECT * FROM cypher("
          . "'MATCH (n:Person {id: $id})-[:KNOWS]->(f) RETURN f.id, f.firstName, f.city', "
          . "NULL::jsonb)");
    }
});
my $pb2_eddy_ms = 1000 * $pb2_eddy_time / $N_PB_QUERIES;
printf "  => pg_eddy avg: %.2f ms/query\n", $pb2_eddy_ms;

my ($pb2_age_ms);
if ($age_ok) {
    my $pb2_age_time = timeit("AGE:     $N_PB_QUERIES queries", sub {
        for my $id (@sample_ids) {
            $node->safe_psql('postgres',
                "SET search_path = ag_catalog, \"\$user\", public;"
              . "SELECT * FROM ag_catalog.cypher('prop_bench', "
              . "\$\$MATCH (n:Person {id: $id})-[:KNOWS]->(f) "
              . "RETURN f.id, f.firstName, f.city\$\$) "
              . "AS (id agtype, firstName agtype, city agtype)");
        }
    });
    $pb2_age_ms = 1000 * $pb2_age_time / $N_PB_QUERIES;
    my $pb2_ratio = $pb2_eddy_ms / ($pb2_age_ms || 0.001);
    printf "  => AGE avg:     %.2f ms/query\n", $pb2_age_ms;
    printf "  => Ratio (pg_eddy/AGE): %.2fx %s\n", $pb2_ratio,
        ($pb2_ratio <= 0.5 ? "(PASS: >=2x faster)"
       : $pb2_ratio <= 1.0 ? "(within 2x)" : "(SLOWER than AGE!)");
}
print "\n";

# ---------------------------------------------------------------------------
# PB-3: 2-hop expand with property return
# ---------------------------------------------------------------------------
print "--- PB-3: 2-hop expand with property return (10 queries) ---\n";
print "    MATCH (n:Person {id:X})-[:KNOWS*2]->(f) RETURN f.id, f.firstName\n";

my @sample_ids_2hop = map { int(rand($N_NODES)) + 1 } (1 .. 10);

my $pb3_eddy_time = timeit("pg_eddy: 10 queries", sub {
    for my $id (@sample_ids_2hop) {
        $node->safe_psql('postgres',
            "SELECT * FROM cypher("
          . "'MATCH (n:Person {id: $id})-[:KNOWS*2]->(f) RETURN f.id, f.firstName', "
          . "NULL::jsonb)");
    }
});
my $pb3_eddy_ms = 1000 * $pb3_eddy_time / 10;
printf "  => pg_eddy avg: %.2f ms/query\n\n", $pb3_eddy_ms;

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
print "=" x 70, "\n";
print "RESULTS SUMMARY\n";
print "=" x 70, "\n";
printf "%-44s %10s %10s %8s\n", "Benchmark", "pg_eddy", "AGE", "Ratio";
print "-" x 70, "\n";
printf "%-44s %8d/s %10s %8s\n",
    "Node insert (nodes/s, 7 props)",
    $eddy_nodes_per_s,
    $age_ok ? sprintf("%d/s", int($N_NODES / ($age_node_time || 0.001))) : "N/A",
    "N/A";
printf "%-44s %8.0f ms %10s %8s\n",
    "PB-1: full-graph expand ($N_EDGES rows, total)",
    $pb1_eddy_ms,
    $age_ok ? sprintf("%.0f ms", $pb1_age_time * 1000) : "N/A",
    $age_ok ? sprintf("%.2fx", $pb1_eddy_time / ($pb1_age_time || 0.001)) : "N/A";
printf "%-44s %8.2f ms %10s %8s\n",
    "PB-2: 1-hop+props (ms/query)",
    $pb2_eddy_ms,
    $age_ok ? sprintf("%.2f ms", $pb2_age_ms) : "N/A",
    $age_ok ? sprintf("%.2fx", $pb2_eddy_ms / ($pb2_age_ms || 0.001)) : "N/A";
printf "%-44s %8.2f ms %10s %8s\n",
    "PB-3: 2-hop+props (ms/query)",
    $pb3_eddy_ms, "N/A", "N/A";
print "=" x 70, "\n\n";

# ---------------------------------------------------------------------------
# Gate decision
# ---------------------------------------------------------------------------
# PB-1 is now measured with a pass/fail gate since OPT-1 shipped (v0.25.0):
# pg_eddy must be <=2.0x AGE latency (full-graph expand including property decode).
if ($age_ok && defined $pb1_age_time) {
    my $pb1_ratio = $pb1_eddy_time / ($pb1_age_time || 0.001);
    print "PB-1 gate (full-graph expand: pg_eddy must be <=2.0x AGE):\n";
    if ($pb1_ratio <= 1.0) {
        printf "  PASS — pg_eddy is %.2fx faster than AGE on PB-1\n", 1.0 / $pb1_ratio;
    } elsif ($pb1_ratio <= 2.0) {
        printf "  PASS — pg_eddy within 2x of AGE (ratio %.2f)\n", $pb1_ratio;
    } else {
        printf "  FAIL — pg_eddy is %.2fx slower than AGE on PB-1 (ratio %.2f)\n",
            $pb1_ratio, $pb1_ratio;
        $node->stop;
        exit 1;
    }
}

# PB-2 gate: parity (+10% noise) vs AGE is acceptable.
if ($age_ok && defined $pb2_age_ms) {
    my $pb2_ratio = $pb2_eddy_ms / ($pb2_age_ms || 0.001);
    print "PB-2 gate (pg_eddy must be <=1.1x AGE — parity +10% noise tolerance):\n";
    if ($pb2_ratio <= 0.5) {
        printf "  PASS — pg_eddy is %.2fx faster than AGE on PB-2\n", 1.0 / $pb2_ratio;
    } elsif ($pb2_ratio <= 1.1) {
        printf "  PASS — pg_eddy is at parity with AGE (ratio %.2f)\n", $pb2_ratio;
    } else {
        printf "  FAIL — pg_eddy is %.2fx SLOWER than AGE on PB-2 (ratio %.2f)\n",
            $pb2_ratio, $pb2_ratio;
        $node->stop;
        exit 1;
    }
}

$node->stop;
print "\nBenchmark complete.\n";
