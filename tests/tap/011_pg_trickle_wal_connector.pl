#!/usr/bin/env perl
use strict;
use warnings;

use Cwd qw(abs_path);
use File::Copy qw(copy);
use File::Path qw(make_path);
use File::Temp qw(tempdir);
use FindBin;
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

my $repo = abs_path("$FindBin::Bin/../..");
my $eddy_package = "$repo/target/release/pg_eddy-pg18/usr";
my $trickle_package = "$repo/vendor/pg-trickle/target/release/pg_trickle-pg18/usr";
my $eddy_library = "$eddy_package/lib/postgresql/18/lib/pg_eddy.so";
my $trickle_library = "$trickle_package/lib/postgresql/18/lib/pg_trickle.so";
my $eddy_library_dir = "$eddy_package/lib/postgresql/18/lib";
my $trickle_library_dir = "$trickle_package/lib/postgresql/18/lib";
my $eddy_extension = "$eddy_package/share/postgresql/18/extension";
my $trickle_extension = "$trickle_package/share/postgresql/18/extension";

plan skip_all => 'release packages for pg_eddy and pg_trickle are required'
    unless -f $eddy_library
        && -f $trickle_library
        && -f "$eddy_extension/pg_eddy.control"
        && -f "$trickle_extension/pg_trickle.control";

my $control_root = tempdir('pg_eddy_connector_controls_XXXX', TMPDIR => 1, CLEANUP => 1);
my $control_dir = "$control_root/extension";
make_path($control_dir);

sub stage_extension {
    my ($source_dir, $control_name, $library) = @_;
    for my $sql (glob "$source_dir/*.sql") {
        copy($sql, $control_dir) or die "copy $sql: $!";
    }

    open my $source, '<', "$source_dir/$control_name.control"
        or die "open $control_name.control: $!";
    local $/;
    my $control = <$source>;
    close $source;
    $control =~ s/^module_pathname\s*=.*$/module_pathname = '$library'/m;
    open my $target, '>', "$control_dir/$control_name.control"
        or die "write staged $control_name.control: $!";
    print {$target} $control;
    close $target;
}

stage_extension($eddy_extension, 'pg_eddy', $eddy_library);
stage_extension($trickle_extension, 'pg_trickle', $trickle_library);

my $node = PostgreSQL::Test::Cluster->new('pg_trickle_wal_connector');
$node->init(extra => ['--no-sync']);
$node->append_conf(
    'postgresql.conf',
    "extension_control_path = '$control_root:\$system'\n" .
    "dynamic_library_path = '$eddy_library_dir:$trickle_library_dir:\$libdir'\n" .
    "shared_preload_libraries = 'pg_eddy,pg_trickle'\n" .
    "wal_level = logical\n" .
    "max_replication_slots = 8\n" .
    "max_wal_senders = 8\n" .
    "max_worker_processes = 16\n" .
    "pg_trickle.scheduler_interval_ms = 100\n"
);
$node->start;
$node->safe_psql('postgres', 'CREATE EXTENSION pg_trickle; CREATE EXTENSION pg_eddy;');

$node->safe_psql('postgres', 'CREATE DATABASE pgtrickle_connector_upgrade');
$node->safe_psql(
    'pgtrickle_connector_upgrade',
    q{CREATE EXTENSION pg_trickle VERSION '0.81.0';
      ALTER EXTENSION pg_trickle UPDATE TO '0.82.0';}
);
is(
    $node->safe_psql(
        'pgtrickle_connector_upgrade',
        q{SELECT extversion = '0.82.0'
              AND to_regprocedure('pgtrickle.poll_pg_eddy_connector()') IS NOT NULL
              AND to_regprocedure('pgtrickle.retry_pg_eddy_connector()') IS NOT NULL
              AND to_regclass('pgtrickle.pgt_cdc_connector_status') IS NOT NULL
          FROM pg_extension WHERE extname = 'pg_trickle'}
    ),
    't',
    '0.81.0 to 0.82.0 migration installs the complete connector API'
);

sub wait_for {
    my ($sql, $expected, $description) = @_;
    my $last = '';
    for (1 .. 300) {
        my $ok = eval {
            $last = $node->safe_psql('postgres', $sql);
            1;
        };
        if ($ok && $last eq $expected) {
            pass($description);
            return;
        }
        $node->safe_psql('postgres', 'SELECT pg_sleep(0.1)');
    }
    is($last, $expected, $description);
}

is(
    $node->safe_psql(
        'postgres',
        q{SELECT extname || '=' || extversion
          FROM pg_extension
          WHERE extname IN ('pg_eddy', 'pg_trickle')
          ORDER BY extname}
    ),
    "pg_eddy=0.12.0\npg_trickle=0.82.0",
    'workspace packages load the expected extension versions'
);

$node->safe_psql('postgres', 'CREATE TABLE unrelated_source(id bigint PRIMARY KEY)');
my ($invalid_stdout, $invalid_stderr) = ('', '');
my $invalid_status = $node->psql(
    'postgres',
    q{SELECT pgtrickle.create_stream_table(
          name => 'invalid_custom_wal',
          query => 'SELECT * FROM unrelated_source',
          schedule => '1h',
          refresh_mode => 'DIFFERENTIAL',
          cdc_mode => 'pg_eddy_wal'
      )},
    stdout => \$invalid_stdout,
    stderr => \$invalid_stderr
);
isnt($invalid_status, 0, 'pg_eddy_wal rejects an unrelated source table');
like(
    $invalid_stderr,
    qr/supported only for the pg_eddy typed mirror tables/,
    'source validation reports the custom connector boundary'
);

$node->safe_psql(
    'postgres',
    q{SELECT pgtrickle.create_stream_table(
          name => 'pgeddy_wal_nodes',
          query => 'SELECT node_id, labels, properties FROM _pg_eddy.ivm_nodes',
          schedule => '1h',
          refresh_mode => 'DIFFERENTIAL',
          cdc_mode => 'pg_eddy_wal'
      );
      SELECT pgtrickle.create_stream_table(
          name => 'pgeddy_wal_edges',
          query => 'SELECT rel_id, rel_type, source_node_id, target_node_id, properties FROM _pg_eddy.ivm_edges',
          schedule => '1h',
          refresh_mode => 'DIFFERENTIAL',
          cdc_mode => 'pg_eddy_wal'
      )}
);

wait_for(
    q{SELECT count(DISTINCT source_relid) = 2
      FROM pgtrickle.pgt_dependencies
      WHERE cdc_mode = 'PG_EDDY_WAL'},
    't',
    'scheduler activates custom WAL for both typed mirrors'
);

is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) = 1
              AND bool_and(plugin = 'pg_eddy')
              AND bool_and(database = current_database())
          FROM pg_replication_slots
          WHERE slot_name LIKE 'pgtrickle_pgeddy_%'}
    ),
    't',
    'one database-scoped pg_eddy slot is shared by both mirrors'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT NOT EXISTS (
              SELECT 1 FROM pg_trigger
              WHERE tgrelid IN (
                  '_pg_eddy.ivm_nodes'::regclass,
                  '_pg_eddy.ivm_edges'::regclass
              )
                AND tgname LIKE 'pg_trickle_cdc_%'
                AND NOT tgisinternal
          )}
    ),
    't',
    'trigger-covered handoff removes both CDC triggers'
);

$node->safe_psql(
    'postgres',
    q{BEGIN;
      SELECT create_node(ARRAY['WalPerson'], '{"name":"Alice"}'::jsonb);
      SELECT create_node(ARRAY['WalPerson'], '{"name":"Bob"}'::jsonb);
      SELECT create_edge(1, 2, 'KNOWS', '{"weight":1}'::jsonb);
      COMMIT;}
);
$node->safe_psql('postgres', 'SELECT * FROM pgtrickle.poll_pg_eddy_connector()');

my $node_buffer = $node->safe_psql(
    'postgres',
    q{SELECT 'pgtrickle_changes.changes_' || source_stable_name
      FROM pgtrickle.pgt_change_tracking
      WHERE source_relid = '_pg_eddy.ivm_nodes'::regclass}
);
my $edge_buffer = $node->safe_psql(
    'postgres',
    q{SELECT 'pgtrickle_changes.changes_' || source_stable_name
      FROM pgtrickle.pgt_change_tracking
      WHERE source_relid = '_pg_eddy.ivm_edges'::regclass}
);

is(
    $node->safe_psql(
        'postgres',
        "SELECT count(*) = 2 AND bool_and(action = 'I') " .
        "AND bool_and(labels = ARRAY['WalPerson']::text[]) " .
        "FROM $node_buffer WHERE node_id IN (1, 2)"
    ),
    't',
    'node frames map into the typed node change buffer'
);
is(
    $node->safe_psql(
        'postgres',
        "SELECT count(*) = 1 AND bool_and(action = 'I') " .
        "AND bool_and(rel_type = 'KNOWS') " .
        "AND bool_and(source_node_id = 1 AND target_node_id = 2) " .
        "AND bool_and(properties = '{\"weight\":1}'::jsonb) " .
        "FROM $edge_buffer WHERE rel_id = 1"
    ),
    't',
    'edge frames map endpoints and properties into the typed edge buffer'
);

$node->safe_psql(
    'postgres',
    q{SELECT pgtrickle.refresh_stream_table('pgeddy_wal_nodes')}
);
$node->safe_psql(
    'postgres',
    q{SELECT pgtrickle.refresh_stream_table('pgeddy_wal_edges')}
);
is(
    $node->safe_psql('postgres', 'SELECT count(*) FROM pgeddy_wal_nodes'),
    '2',
    'manual refresh consumes node WAL changes'
);
is(
    $node->safe_psql('postgres', 'SELECT count(*) FROM pgeddy_wal_edges'),
    '1',
    'manual refresh consumes edge WAL changes'
);

my $before_update = $node->safe_psql(
    'postgres',
    "SELECT COALESCE(max(change_id), 0) FROM $node_buffer"
);
$node->safe_psql(
    'postgres',
    q{SELECT update_node(
          1,
          ARRAY['WalPerson','Updated'],
          '{"name":"Alicia","version":2}'::jsonb
      )}
);
$node->safe_psql('postgres', 'SELECT * FROM pgtrickle.poll_pg_eddy_connector()');
is(
    $node->safe_psql(
        'postgres',
        "SELECT string_agg(action, '' ORDER BY change_id) = 'DI' " .
        "AND bool_or(action = 'D' AND properties = '{\"name\":\"Alice\"}'::jsonb) " .
        "AND bool_or(action = 'I' AND properties = '{\"name\":\"Alicia\",\"version\":2}'::jsonb) " .
        "FROM $node_buffer WHERE change_id > $before_update AND node_id = 1"
    ),
    't',
    'node UPDATE is applied atomically as complete OLD delete plus NEW insert'
);

$node->safe_psql(
    'postgres',
    q{SELECT create_node(ARRAY['RestartReplay'], '{"name":"durable"}'::jsonb)}
);
$node->safe_psql('postgres', 'SELECT * FROM pgtrickle.poll_pg_eddy_connector()');
my $restart_rows = $node->safe_psql(
    'postgres',
    "SELECT count(*) FROM $node_buffer " .
    "WHERE labels = ARRAY['RestartReplay']::text[]"
);
$node->restart;
$node->safe_psql('postgres', 'SELECT * FROM pgtrickle.poll_pg_eddy_connector()');
is(
    $node->safe_psql(
        'postgres',
        "SELECT count(*) FROM $node_buffer " .
        "WHERE labels = ARRAY['RestartReplay']::text[]"
    ),
    $restart_rows,
    'durable cursor prevents duplicate replay after restart'
);

$node->safe_psql('postgres', 'SELECT clear()');
$node->safe_psql('postgres', 'SELECT * FROM pgtrickle.poll_pg_eddy_connector()');
is(
    $node->safe_psql(
        'postgres',
        q{SELECT bool_and(needs_reinit)
          FROM pgtrickle.pgt_stream_tables
          WHERE pgt_name IN ('pgeddy_wal_nodes', 'pgeddy_wal_edges')}
    ),
    't',
    'GRAPH_RESET marks every downstream stream table for reinitialization'
);
$node->safe_psql(
    'postgres',
    q{SELECT pgtrickle.refresh_stream_table('pgeddy_wal_nodes')}
);
$node->safe_psql(
    'postgres',
    q{SELECT pgtrickle.refresh_stream_table('pgeddy_wal_edges')}
);

my $slot_name = $node->safe_psql(
    'postgres',
    q{SELECT slot_name FROM pgtrickle.pgt_cdc_connectors
      WHERE connector_kind = 'PG_EDDY_WAL'}
);
my $refresh_before_fallback = $node->safe_psql(
    'postgres',
    q{SELECT COALESCE(max(refresh_id), 0) FROM pgtrickle.pgt_refresh_history}
);
$node->safe_psql('postgres', "SELECT pg_drop_replication_slot('$slot_name')");
wait_for(
    q{SELECT state = 'FALLBACK'
      FROM pgtrickle.pgt_cdc_connectors
      WHERE connector_kind = 'PG_EDDY_WAL'},
    't',
    'externally dropped shared slot forces connector fallback'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(DISTINCT source_relid) = 2
              AND bool_and(cdc_mode = 'TRIGGER')
          FROM pgtrickle.pgt_dependencies
          WHERE source_relid IN (
              '_pg_eddy.ivm_nodes'::regclass,
              '_pg_eddy.ivm_edges'::regclass
          )}
    ),
    't',
    'fallback restores trigger mode for every active mirror source'
);
is(
    $node->safe_psql(
        'postgres',
                qq{SELECT bool_and(
                            st.needs_reinit OR EXISTS (
                                    SELECT 1 FROM pgtrickle.pgt_refresh_history h
                                    WHERE h.pgt_id = st.pgt_id
                                        AND h.refresh_id > $refresh_before_fallback
                                        AND h.action = 'REINITIALIZE'
                                        AND h.status = 'COMPLETED'
                            )
                    )
                    FROM pgtrickle.pgt_stream_tables st
                    WHERE st.pgt_name IN ('pgeddy_wal_nodes', 'pgeddy_wal_edges')}
    ),
    't',
    'missing-slot fallback requires a full refresh to close the data gap'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) >= 2
          FROM pg_trigger
          WHERE tgrelid IN (
              '_pg_eddy.ivm_nodes'::regclass,
              '_pg_eddy.ivm_edges'::regclass
          )
            AND tgname LIKE 'pg_trickle_cdc_%'
            AND NOT tgisinternal}
    ),
    't',
    'fallback recreates CDC triggers before leaving custom WAL mode'
);

my $fallback_change_id = $node->safe_psql(
    'postgres',
    "SELECT COALESCE(max(change_id), 0) FROM $node_buffer"
);
$node->safe_psql(
    'postgres',
    q{SELECT create_node(ARRAY['TriggerFallback'], '{"name":"captured"}'::jsonb)}
);
is(
    $node->safe_psql(
        'postgres',
        "SELECT count(*) FROM $node_buffer " .
        "WHERE change_id > $fallback_change_id " .
        "AND labels = ARRAY['TriggerFallback']::text[] AND action = 'I'"
    ),
    '1',
    'fallback trigger captures writes immediately'
);

$node->safe_psql('postgres', 'SELECT pgtrickle.retry_pg_eddy_connector()');
wait_for(
    q{SELECT count(DISTINCT source_relid) = 2
      FROM pgtrickle.pgt_dependencies
      WHERE cdc_mode = 'PG_EDDY_WAL'},
    't',
    'operator retry recreates the shared slot and reactivates custom WAL'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT state = 'ACTIVE' AND confirmed_flush_lsn IS NOT NULL
          FROM pgtrickle.pgt_cdc_connector_status}
    ),
    't',
    'connector status view exposes active durable slot progress'
);

$node->safe_psql(
    'postgres',
    q{SELECT pgtrickle.drop_stream_table('pgeddy_wal_nodes', false);
      SELECT pgtrickle.drop_stream_table('pgeddy_wal_edges', false);}
);
wait_for(
    q{SELECT NOT EXISTS (
          SELECT 1 FROM pgtrickle.pgt_cdc_connectors
          WHERE connector_kind = 'PG_EDDY_WAL'
      )},
    't',
    'unused connector catalog state is removed'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT NOT EXISTS (
              SELECT 1 FROM pg_replication_slots
              WHERE slot_name LIKE 'pgtrickle_pgeddy_%'
          )}
    ),
    't',
    'unused shared slot is dropped without affecting other CDC resources'
);

my ($immediate_stdout, $immediate_stderr) = ('', '');
my $immediate_status = $node->psql(
    'postgres',
    q{SELECT create_graph_view(
          name => 'invalid_immediate_decode',
          cypher => 'MATCH (p:DecodedPerson) RETURN p.name AS name',
          refresh_mode => 'IMMEDIATE',
          decode => true
      )},
    stdout => \$immediate_stdout,
    stderr => \$immediate_stderr
);
isnt($immediate_status, 0, 'decode=true rejects IMMEDIATE graph views');
like(
    $immediate_stderr,
    qr/asynchronous and cannot be used with IMMEDIATE/,
    'IMMEDIATE rejection explains the asynchronous WAL boundary'
);

$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
          name => 'decoded_people',
          cypher => 'MATCH (p:DecodedPerson) RETURN p.name AS name',
          params => '{}'::jsonb,
          schedule => '1h',
          refresh_mode => 'DIFFERENTIAL',
          decode => true
      )}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT requested_cdc_mode
          FROM pgtrickle.pgt_stream_tables
          WHERE pgt_name = '__pgeddy_decoded_people'}
    ),
    'pg_eddy_wal',
    'decode=true requests pg_eddy_wal from the pinned pg_trickle fork'
);
wait_for(
    q{SELECT bool_and(d.cdc_mode = 'PG_EDDY_WAL')
      FROM pgtrickle.pgt_dependencies d
      JOIN pgtrickle.pgt_stream_tables st USING (pgt_id)
      WHERE st.pgt_name = '__pgeddy_decoded_people'
        AND d.source_type = 'TABLE'},
    't',
    'decoded graph view completes shared-slot handoff'
);
$node->safe_psql(
    'postgres',
    q{SELECT create_node(
          ARRAY['DecodedPerson'],
          '{"name":"Via WAL"}'::jsonb
      );
      SELECT refresh_graph_view('decoded_people');}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT name FROM _pg_eddy_views.decoded_people}
    ),
    '"Via WAL"',
    'decode=true graph view materializes semantic WAL changes'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT decode FROM list_graph_views() WHERE name = 'decoded_people'}
    ),
    't',
    'graph-view catalog records the decode mode'
);
$node->safe_psql('postgres', q{SELECT drop_graph_view('decoded_people')});
wait_for(
    q{SELECT NOT EXISTS (
          SELECT 1 FROM pgtrickle.pgt_cdc_connectors
          WHERE connector_kind = 'PG_EDDY_WAL'
      )},
    't',
    'dropping the last decoded graph view cleans up its connector'
);

$node->stop;
done_testing();
