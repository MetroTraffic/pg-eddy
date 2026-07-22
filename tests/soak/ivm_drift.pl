#!/usr/bin/env perl
use strict;
use warnings;

use POSIX qw(_exit strftime);
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Time::HiRes qw(sleep time);

$| = 1;

my $duration_seconds = $ENV{IVM_SOAK_SECONDS} // (72 * 60 * 60);
my $writer_count = $ENV{IVM_SOAK_WRITERS} // 2;
my $slot_count = $ENV{IVM_SOAK_SLOTS} // 64;
my $check_interval = $ENV{IVM_SOAK_CHECK_INTERVAL} // 0.2;

my $have_pg_trickle = -f '/usr/share/postgresql/18/extension/pg_trickle.control'
    && -f '/usr/lib/postgresql/18/lib/pg_trickle.so';
die "pinned pg_trickle package is not installed\n" unless $have_pg_trickle;

my $node = PostgreSQL::Test::Cluster->new('ivm_drift_soak');
$node->init(extra => ['--no-sync']);
$node->append_conf(
    'postgresql.conf',
    "shared_preload_libraries = 'pg_eddy,pg_trickle'\n" .
    "max_worker_processes = 16\n"
);
$node->start;
$node->safe_psql('postgres', 'CREATE EXTENSION pg_trickle; CREATE EXTENSION pg_eddy;');
$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'ivm_soak_live',
        'MATCH (n:IvmSoak) RETURN n.slot AS slot, n.version AS version',
        '{}'::jsonb,
        '1s',
        'IMMEDIATE',
        false,
        false
    )}
);

my $started = time();
my $deadline = $started + $duration_seconds;
my @writer_pids;

for my $writer_id (1 .. $writer_count) {
    my $pid = fork();
    die "fork failed: $!\n" unless defined $pid;
    if ($pid == 0) {
        open STDOUT, '>', "/tmp/pg_eddy_ivm_soak_writer_${writer_id}.log"
            or die "cannot redirect writer stdout: $!\n";
        open STDERR, '>&', STDOUT
            or die "cannot redirect writer stderr: $!\n";
        my $session = $node->background_psql('postgres');
        my $iteration = 0;
        my $ok = eval {
            while (time() < $deadline) {
                my $slot = (($writer_id - 1) * $slot_count + $iteration) % $slot_count;
                $session->query_safe(
                    "SELECT * FROM cypher(" .
                    "'MERGE (n:IvmSoak {slot: \$slot}) " .
                    "ON CREATE SET n.version = \$version " .
                    "ON MATCH SET n.version = \$version', " .
                    "jsonb_build_object('slot', $slot, 'version', $iteration))"
                );
                if ($iteration % 17 == 16) {
                    $session->query_safe(
                        "SELECT * FROM cypher(" .
                        "'MATCH (n:IvmSoak {slot: \$slot}) DETACH DELETE n', " .
                        "jsonb_build_object('slot', $slot))"
                    );
                }
                $iteration++;
            }
            1;
        };
        warn "writer $writer_id failed: $@\n" unless $ok;
        eval { $session->quit; };
        _exit($ok ? 0 : 1);
    }
    push @writer_pids, $pid;
}

my $checks = 0;
my $drift = 0;
my $next_progress = $started + 60;
while (time() < $deadline) {
    my $consistent = $node->safe_psql(
        'postgres',
        q{WITH view_rows AS (
              SELECT slot, version FROM _pg_eddy_views.ivm_soak_live
          ), source_rows AS (
              SELECT properties->'slot' AS slot, properties->'version' AS version
              FROM _pg_eddy.ivm_nodes
              WHERE labels @> ARRAY['IvmSoak']::text[]
          )
          SELECT NOT EXISTS (
              (SELECT * FROM view_rows EXCEPT ALL SELECT * FROM source_rows)
              UNION ALL
              (SELECT * FROM source_rows EXCEPT ALL SELECT * FROM view_rows)
          )}
    );
    $checks++;
    if ($consistent ne 't') {
        $drift = 1;
        warn "IVM drift detected after " . (time() - $started) . " seconds\n";
        last;
    }
    if (time() >= $next_progress) {
        printf "[%s] IVM soak healthy: %d checks\n",
            strftime('%Y-%m-%d %H:%M:%S', localtime), $checks;
        $next_progress += 60;
    }
    sleep $check_interval;
}

if ($drift) {
    kill 'TERM', @writer_pids;
}
my $writer_failures = 0;
for my $pid (@writer_pids) {
    waitpid($pid, 0);
    $writer_failures++ if $? != 0;
}

my $final_consistent = $node->safe_psql(
    'postgres',
    q{WITH view_rows AS (
          SELECT slot, version FROM _pg_eddy_views.ivm_soak_live
      ), source_rows AS (
          SELECT properties->'slot' AS slot, properties->'version' AS version
          FROM _pg_eddy.ivm_nodes
          WHERE labels @> ARRAY['IvmSoak']::text[]
      )
      SELECT NOT EXISTS (
          (SELECT * FROM view_rows EXCEPT ALL SELECT * FROM source_rows)
          UNION ALL
          (SELECT * FROM source_rows EXCEPT ALL SELECT * FROM view_rows)
      )}
);

$node->stop;
my $elapsed = time() - $started;
printf "IVM soak completed: %.1f seconds, %d writers, %d consistency checks\n",
    $elapsed, $writer_count, $checks;

die "IVM soak failed: drift=$drift final=$final_consistent writer_failures=$writer_failures\n"
    if $drift || $final_consistent ne 't' || $writer_failures;
