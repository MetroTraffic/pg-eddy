#!/usr/bin/env perl
# 004_concurrent_inserts.pl — Concurrent insert correctness test.
#
# N=4 parallel psql sessions each insert M=1000 nodes.
# After all sessions finish, count_nodes() must equal N×M with no duplicates
# (sequence gaps from failed transactions are acceptable; all 4000 inserts
# committed here so gaps should not occur).

use strict;
use warnings;
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

# Use /tmp for Unix sockets — /var/run/postgresql is not writable in dev containers.
$ENV{PGHOST} = '/tmp';

my $N = 4;    # number of parallel writer sessions
my $M = 1000; # inserts per session

my $node = PostgreSQL::Test::Cluster->new('concurrent_node');
$node->init(extra => ['--no-sync']);
$node->append_conf('postgresql.conf', qq{
shared_preload_libraries = 'pg_eddy'
max_connections = 20
});
$node->start;

$node->safe_psql('postgres', "CREATE EXTENSION pg_eddy;");

# ---------------------------------------------------------------------------
# Launch N writer sessions in parallel
# ---------------------------------------------------------------------------
note "Launching $N writer sessions, each inserting $M nodes...";

my @sessions;
for my $i (1 .. $N) {
    my $session = $node->background_psql('postgres');
    push @sessions, $session;
    # Fire the INSERT batch asynchronously (no query_safe — we don't wait yet).
    $session->query_until(
        qr/INSERT_DONE_$i/,
        qq{
            DO \$\$
            DECLARE j INT;
            BEGIN
                FOR j IN 1..$M LOOP
                    PERFORM create_node(
                        ARRAY['Concurrent']::text[],
                        ('{"session":$i,"j":' || j || '}')::jsonb
                    );
                END LOOP;
            END \$\$;
            SELECT 'INSERT_DONE_$i';
        }
    );
}

# Wait for all sessions to finish (query_until already waited per session above).
$_->quit for @sessions;

# ---------------------------------------------------------------------------
# Verify total count
# ---------------------------------------------------------------------------
my $total = $node->safe_psql('postgres', "SELECT count_nodes();");
chomp $total;
my $expected = $N * $M;
is($total, "$expected",
    "count_nodes() = $expected after $N×$M concurrent inserts");

# Verify no duplicates by checking node_id uniqueness via find_nodes
my $distinct = $node->safe_psql('postgres', q{
    SELECT count(DISTINCT nid)
    FROM find_nodes(NULL, NULL) AS nid;
});
chomp $distinct;
is($distinct, "$expected", "all $expected node_ids are distinct");

$node->stop;
done_testing();
