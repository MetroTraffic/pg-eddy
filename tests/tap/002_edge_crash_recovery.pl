#!/usr/bin/env perl
# 002_edge_crash_recovery.pl — WAL crash-recovery test for edge pages and
# adjacency chains.
#
# Creates a small graph (10 nodes, 20 edges), crashes the cluster, restarts,
# and verifies edges and adjacency follow-up survive WAL replay.

use strict;
use warnings;
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

# Use /tmp for Unix sockets — /var/run/postgresql is not writable in dev containers.
$ENV{PGHOST} = '/tmp';

my $node = PostgreSQL::Test::Cluster->new('edge_crash_node');
$node->init(extra => ['--no-sync']);
$node->append_conf('postgresql.conf', "shared_preload_libraries = 'pg_eddy'\n");
$node->start;

$node->safe_psql('postgres', "CREATE EXTENSION pg_eddy;");

# ---------------------------------------------------------------------------
# Build a small graph: 10 nodes, each connected to the next two in a ring
# ---------------------------------------------------------------------------
note "Building graph (10 nodes, 20 edges)...";
$node->safe_psql('postgres', q{
    DO $$
    DECLARE
        ids  BIGINT[];
        i    INT;
    BEGIN
        -- Create 10 nodes
        FOR i IN 1..10 LOOP
            ids := ids || create_node(
                ARRAY['Vertex']::text[],
                ('{"n":' || i || '}')::jsonb
            );
        END LOOP;
        -- Each node connects forward to the next two (wrapping)
        FOR i IN 1..10 LOOP
            PERFORM create_edge(
                ids[i], ids[(i % 10) + 1], 'NEXT', '{}'::jsonb);
            PERFORM create_edge(
                ids[i], ids[((i + 1) % 10) + 1], 'SKIP', '{}'::jsonb);
        END LOOP;
    END $$;
});

my $edge_count_before = $node->safe_psql('postgres',
    "SELECT count_edges();");
chomp $edge_count_before;
is($edge_count_before, '20', "20 edges before crash");

# Pick the first two node IDs for the adjacency check after restart.
my $first_id = $node->safe_psql('postgres',
    "SELECT min(node_id) FROM (SELECT find_nodes(NULL, NULL) AS node_id) t;");
chomp $first_id;

my $out_count_before = $node->safe_psql('postgres',
    "SELECT count(*) FROM neighbours($first_id, 'OUT', NULL) t(nid);");
chomp $out_count_before;
is($out_count_before, '2', "node $first_id has 2 OUT neighbours before crash");

# ---------------------------------------------------------------------------
# Immediate shutdown (no checkpoint)
# ---------------------------------------------------------------------------
note "SIGQUIT – immediate shutdown...";
$node->stop('immediate');

# ---------------------------------------------------------------------------
# Restart and verify
# ---------------------------------------------------------------------------
note "Restarting...";
$node->start;

my $edge_count_after = $node->safe_psql('postgres',
    "SELECT count_edges();");
chomp $edge_count_after;
is($edge_count_after, '20', "20 edges survive crash + WAL replay");

my $out_count_after = $node->safe_psql('postgres',
    "SELECT count(*) FROM neighbours($first_id, 'OUT', NULL) t(nid);");
chomp $out_count_after;
is($out_count_after, '2',
    "node $first_id adjacency survives crash + WAL replay");

$node->stop;
done_testing();
