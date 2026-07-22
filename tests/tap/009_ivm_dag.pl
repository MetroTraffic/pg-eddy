#!/usr/bin/env perl
use strict;
use warnings;

use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;
use Time::HiRes qw(sleep time);

my $have_pg_trickle = -f '/usr/share/postgresql/18/extension/pg_trickle.control'
    && -f '/usr/lib/postgresql/18/lib/pg_trickle.so';

plan skip_all => 'pinned pg_trickle package is not installed'
    unless $have_pg_trickle;

my $node = PostgreSQL::Test::Cluster->new('ivm_dag');
$node->init(extra => ['--no-sync']);
$node->append_conf(
    'postgresql.conf',
    "shared_preload_libraries = 'pg_eddy,pg_trickle'\n" .
    "max_worker_processes = 16\n" .
    "pg_trickle.scheduler_interval_ms = 100\n" .
    "pg_trickle.min_schedule_seconds = 1\n" .
    "pg_trickle.auto_backoff = off\n"
);
$node->start;
$node->safe_psql('postgres', 'CREATE EXTENSION pg_trickle; CREATE EXTENSION pg_eddy;');

$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'dag_people',
        'MATCH (p:DagPerson) RETURN p.name AS name',
        '{}'::jsonb,
        '1s',
        'DIFFERENTIAL',
        false,
        false
    )}
);
$node->safe_psql(
    'postgres',
    q{SELECT pgtrickle.create_stream_table(
        name => '_pg_eddy_views.dag_people_copy',
        query => 'SELECT name FROM _pg_eddy_views.__pgeddy_dag_people',
        schedule => 'calculated',
        refresh_mode => 'DIFFERENTIAL',
        cdc_mode => 'trigger'
    )}
);

is(
    $node->safe_psql(
        'postgres',
        q{SELECT d.source_type
          FROM pgtrickle.pgt_dependencies d
          JOIN pgtrickle.pgt_stream_tables downstream USING (pgt_id)
          WHERE downstream.pgt_name = 'dag_people_copy'
            AND d.source_relid = '_pg_eddy_views.__pgeddy_dag_people'::regclass}
    ),
    'STREAM_TABLE',
    'pg_trickle recognizes the graph stream as an upstream DAG node'
);

$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'CREATE (:DagPerson {name: ''Eve''})',
        NULL::jsonb
    )}
);

my $deadline = time() + 30;
my $downstream_count = '0';
while (time() < $deadline) {
    $downstream_count = $node->safe_psql(
        'postgres',
        q{SELECT count(*) FROM _pg_eddy_views.dag_people_copy
          WHERE name = '"Eve"'}
    );
    last if $downstream_count eq '1';
    sleep 0.2;
}
is(
    $downstream_count,
    '1',
    'scheduler propagates a graph mutation through the DAG in topological order'
);

is(
    $node->safe_psql(
        'postgres',
        q{SELECT bool_and(status = 'ACTIVE')
          FROM pgtrickle.pgt_stream_tables
          WHERE pgt_name IN ('__pgeddy_dag_people', 'dag_people_copy')}
    ),
    't',
    'both DAG layers remain active after propagation'
);

$node->stop;
done_testing();
