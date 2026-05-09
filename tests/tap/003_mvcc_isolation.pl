#!/usr/bin/env perl
# 003_mvcc_isolation.pl — MVCC isolation test.
#
# Verifies that a reader holding a snapshot taken *before* an insert does not
# see the newly inserted node until it re-reads after the writer commits.
#
# Uses background_psql() to run two concurrent sessions.

use strict;
use warnings;
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

# Use /tmp for Unix sockets — /var/run/postgresql is not writable in dev containers.
$ENV{PGHOST} = '/tmp';

my $node = PostgreSQL::Test::Cluster->new('mvcc_node');
$node->init(extra => ['--no-sync']);
$node->append_conf('postgresql.conf', "shared_preload_libraries = 'pg_eddy'\n");
$node->start;

$node->safe_psql('postgres', "CREATE EXTENSION pg_eddy;");

# ---------------------------------------------------------------------------
# Session T2 (reader): open a repeatable-read transaction and snapshot early
# ---------------------------------------------------------------------------
my $reader = $node->background_psql('postgres');
$reader->query_safe("BEGIN ISOLATION LEVEL REPEATABLE READ;");

# Confirm empty graph from T2's snapshot
my $count_before = $reader->query_safe(
    "SELECT count_nodes();");
$count_before =~ s/\s+//g;
is($count_before, '0', "T2 sees 0 nodes before T1 inserts");

# ---------------------------------------------------------------------------
# Session T1 (writer): insert a node and commit
# ---------------------------------------------------------------------------
$node->safe_psql('postgres', q{
    SELECT create_node(ARRAY['IsolTest']::text[], '{"v":1}'::jsonb);
});

# T2 still in its repeatable-read snapshot — must not see the new node
my $count_during = $reader->query_safe(
    "SELECT count_nodes();");
$count_during =~ s/\s+//g;
is($count_during, '0',
    "T2 does not see T1's committed insert (repeatable-read snapshot)");

# T2 commits its transaction and re-reads at a fresh snapshot
$reader->query_safe("COMMIT;");
my $count_after = $reader->query_safe(
    "SELECT count_nodes();");
$count_after =~ s/\s+//g;
is($count_after, '1', "T2 sees the node after its own COMMIT + fresh read");

$reader->quit;
$node->stop;
done_testing();
