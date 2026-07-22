#!/usr/bin/env perl
use strict;
use warnings;

use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

my $have_pg_trickle = -f '/usr/share/postgresql/18/extension/pg_trickle.control'
    && -f '/usr/lib/postgresql/18/lib/pg_trickle.so';

plan skip_all => 'pinned pg_trickle package is not installed'
    unless $have_pg_trickle;

my $node = PostgreSQL::Test::Cluster->new('ivm_cdc');
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
        'delete_capture',
        'MATCH (p:DeleteCapture) RETURN p.name AS name',
        '{}'::jsonb,
        '1h',
        'DIFFERENTIAL',
        false,
        false
    )}
);

my $node_id = $node->safe_psql(
    'postgres',
    q{SELECT create_node(
        ARRAY['DeleteCapture'],
        '{"name":"Old Value","marker":42}'::jsonb
    )}
);
$node->safe_psql('postgres', q{SELECT refresh_graph_view('delete_capture')});
is(
    $node->safe_psql(
        'postgres',
        q{SELECT name FROM _pg_eddy_views.delete_capture}
    ),
    '"Old Value"',
    'initial differential refresh materializes the source row'
);

my $stable_name = $node->safe_psql(
    'postgres',
    q{SELECT source_stable_name
      FROM pgtrickle.pgt_change_tracking
      WHERE source_relid = '_pg_eddy.ivm_nodes'::regclass}
);
like($stable_name, qr/^[a-z0-9_]+$/, 'node mirror has a safe stable CDC buffer name');

$node->safe_psql('postgres', qq{SELECT delete_node($node_id)});
is(
    $node->safe_psql(
        'postgres',
          qq{SELECT count(*) = 1
              AND bool_and(action = 'D')
              AND bool_and(node_id = $node_id)
              AND bool_and(labels = ARRAY['DeleteCapture']::text[])
              AND bool_and(properties = '{"name":"Old Value","marker":42}'::jsonb)
              FROM pgtrickle_changes.changes_$stable_name
              WHERE action = 'D' AND node_id = $node_id}
    ),
    't',
    'only the deleted row is pending and it contains complete OLD values'
);

$node->safe_psql('postgres', q{SELECT refresh_graph_view('delete_capture')});
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) FROM _pg_eddy_views.delete_capture}
    ),
    '0',
    'differential refresh removes the deleted graph-view row'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT h.action || '/' || h.was_full_fallback::text
          FROM pgtrickle.pgt_refresh_history h
          JOIN pgtrickle.pgt_stream_tables st USING (pgt_id)
          WHERE st.pgt_name = '__pgeddy_delete_capture'
            AND h.status = 'COMPLETED'
          ORDER BY h.refresh_id DESC
          LIMIT 1}
    ),
    'DIFFERENTIAL/false',
    'refresh history confirms differential processing without full fallback'
);
is(
    $node->safe_psql(
        'postgres',
        qq{SELECT NOT EXISTS (
              SELECT 1
              FROM pgtrickle_changes.changes_$stable_name changes
              JOIN pgtrickle.pgt_stream_tables stream
                ON stream.pgt_name = '__pgeddy_delete_capture'
              WHERE changes.lsn > (
                  stream.frontier->'sources'
                      ->('_pg_eddy.ivm_nodes'::regclass::oid::text)
                      ->>'lsn'
              )::pg_lsn
          )}
    ),
    't',
    'differential refresh advances beyond every pending node change'
);

$node->stop;
done_testing();
