#!/usr/bin/env perl
# 001_crash_recovery.pl — WAL crash-recovery test for node pages.
#
# Inserts 10 000 nodes, sends SIGQUIT (immediate shutdown without checkpoint),
# restarts the cluster, and verifies that every node survives replay.
#
# Uses PostgreSQL::Test::Cluster (ships with postgresql-server-dev-18).

use strict;
use warnings;
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

# Use /tmp for Unix sockets — /var/run/postgresql is not writable in dev containers.
$ENV{PGHOST} = '/tmp';

# ---------------------------------------------------------------------------
# Cluster setup
# ---------------------------------------------------------------------------
my $node = PostgreSQL::Test::Cluster->new('crash_node');
$node->init(extra => ['--no-sync']);
$node->append_conf('postgresql.conf', "shared_preload_libraries = 'pg_eddy'\n");
$node->start;

# Install the extension.
$node->safe_psql('postgres', "CREATE EXTENSION pg_eddy;");

# ---------------------------------------------------------------------------
# Insert 10 000 nodes
# ---------------------------------------------------------------------------
note "Inserting 10 000 nodes...";
$node->safe_psql('postgres', q{
    DO $$
    DECLARE i INT;
    BEGIN
        FOR i IN 1..10000 LOOP
            PERFORM create_node(
                ARRAY['Crash']::text[],
                ('{"seq":' || i || '}')::jsonb
            );
        END LOOP;
    END $$;
});

my $before = $node->safe_psql('postgres',
    "SELECT count_nodes();");
chomp $before;
is($before, '10000', "10 000 nodes before crash");

# ---------------------------------------------------------------------------
# Crash (SIGQUIT = immediate shutdown; no checkpoint is written)
# ---------------------------------------------------------------------------
note "Sending SIGQUIT for immediate shutdown...";
$node->stop('immediate');

# ---------------------------------------------------------------------------
# Restart and verify WAL replay
# ---------------------------------------------------------------------------
note "Restarting cluster...";
$node->start;

my $after = $node->safe_psql('postgres',
    "SELECT count_nodes();");
chomp $after;
is($after, '10000', "10 000 nodes survive crash + WAL replay");

# ---------------------------------------------------------------------------
# Teardown
# ---------------------------------------------------------------------------
$node->stop;
done_testing();
