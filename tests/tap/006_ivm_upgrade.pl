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

my $node = PostgreSQL::Test::Cluster->new('ivm_upgrade');
$node->init(extra => ['--no-sync']);
$node->append_conf(
    'postgresql.conf',
    "shared_preload_libraries = 'pg_eddy,pg_trickle'\n" .
    "max_worker_processes = 16\n"
);
$node->start;
$node->safe_psql(
    'postgres',
    q{CREATE EXTENSION pg_trickle;
      CREATE EXTENSION pg_eddy VERSION '0.11.0';}
);

is(
    $node->safe_psql(
        'postgres',
        q{SELECT extversion FROM pg_extension WHERE extname = 'pg_eddy'}
    ),
    '0.11.0',
    'old extension schema installs with the current shared library'
);

my $source = $node->safe_psql(
    'postgres',
    q{SELECT create_node(ARRAY['UpgradePerson'], '{"name":"Before"}'::jsonb)}
);
my $target = $node->safe_psql(
    'postgres',
    q{SELECT create_node(ARRAY['UpgradePerson'], '{"name":"Target"}'::jsonb)}
);
$node->safe_psql(
    'postgres',
    qq{SELECT create_edge($source, $target, 'UPGRADE_EDGE', '{"since":2026}'::jsonb)}
);

is(
    $node->safe_psql(
        'postgres',
        q{SELECT to_regclass('_pg_eddy.ivm_nodes') IS NULL
            AND count_nodes() = 2
            AND count_edges() = 1}
    ),
    't',
    'pre-upgrade writes succeed while mirror relations are absent'
);

$node->safe_psql('postgres', q{ALTER EXTENSION pg_eddy UPDATE TO '0.12.0'});

is(
    $node->safe_psql(
        'postgres',
        q{SELECT extversion FROM pg_extension WHERE extname = 'pg_eddy'}
    ),
    '0.12.0',
    'extension upgrades to schema 0.12.0'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT (SELECT count(*) FROM _pg_eddy.ivm_nodes)::text
            || '/' || (SELECT count(*) FROM _pg_eddy.ivm_edges)::text}
    ),
    '2/1',
    'upgrade backfills existing nodes and edges into typed mirrors'
);

$node->safe_psql(
    'postgres',
    q{SELECT create_node(ARRAY['UpgradePerson'], '{"name":"After"}'::jsonb)}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) FROM _pg_eddy.ivm_nodes
          WHERE labels @> ARRAY['UpgradePerson']::text[]}
    ),
    '3',
    'mirror maintenance activates immediately after upgrade'
);

$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'upgrade_people',
        'MATCH (p:UpgradePerson) RETURN p.name AS name',
        '{}'::jsonb,
        '1s',
        'IMMEDIATE',
        false,
        false
    )}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT string_agg(name::text, ',' ORDER BY name::text)
          FROM _pg_eddy_views.upgrade_people}
    ),
    '"After","Before","Target"',
    'graph views are usable immediately after upgrade'
);

$node->stop;
done_testing();
