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

my $node = PostgreSQL::Test::Cluster->new('ivm_graph_views');
$node->init(extra => ['--no-sync']);
$node->append_conf(
    'postgresql.conf',
    "shared_preload_libraries = 'pg_eddy,pg_trickle'\n" .
    "max_worker_processes = 16\n"
);
$node->start;
$node->safe_psql('postgres', 'CREATE EXTENSION pg_trickle; CREATE EXTENSION pg_eddy;');

is(
    $node->safe_psql(
        'postgres',
        q{SELECT graph_view_dependency_info()->>'repository'}
    ),
    'MetroTraffic/pg-trickle',
    'runtime diagnostics identify the pinned fork'
);

$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'CREATE (:Person {name: ''Alice'', age: 30})-[:KNOWS]->(:Person {name: ''Bob'', age: 31})',
        NULL::jsonb
    )}
);

is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) FROM _pg_eddy.ivm_nodes}
    ),
    '2',
    'Cypher node writes populate typed mirrors'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) FROM _pg_eddy.ivm_edges}
    ),
    '1',
    'Cypher relationship writes populate typed mirrors'
);

my ($rollback_stdout, $rollback_stderr) = ('', '');
my $rollback_result = $node->psql(
    'postgres',
    q{BEGIN;
      SELECT * FROM cypher('CREATE (:RolledBack {value: 1})', NULL::jsonb);
      ROLLBACK;},
    stdout => \$rollback_stdout,
    stderr => \$rollback_stderr
);
is($rollback_result, 0, 'rolled-back Cypher write executes cleanly');
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) FROM _pg_eddy.ivm_nodes
          WHERE labels @> ARRAY['RolledBack']::text[]}
    ),
    '0',
    'rolled-back writes leave no mirror rows'
);

$node->safe_psql('postgres', 'TRUNCATE _pg_eddy.ivm_edges, _pg_eddy.ivm_nodes');
is(
    $node->safe_psql('postgres', 'SELECT rebuild_ivm_sources()'),
    '3',
    'mirror rebuild restores every node and edge'
);

$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'friends',
        'MATCH (a:Person {name: $name})-[:KNOWS]->(b:Person) RETURN b.name AS friend',
        '{"name":"Alice"}'::jsonb,
        '1s',
        'DIFFERENTIAL',
        false,
        false
    )}
);

is(
    $node->safe_psql(
        'postgres',
        q{SELECT requested_cdc_mode
          FROM pgtrickle.pgt_stream_tables
          WHERE pgt_name = '__pgeddy_friends'}
    ),
    'trigger',
    'graph view pins pg_trickle to trigger CDC'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT DISTINCT d.cdc_mode
          FROM pgtrickle.pgt_dependencies d
          JOIN pgtrickle.pgt_stream_tables st USING (pgt_id)
          WHERE st.pgt_name = '__pgeddy_friends'}
    ),
    'TRIGGER',
    'effective source CDC mode is trigger'
);
is(
    $node->safe_psql('postgres', 'SELECT friend FROM _pg_eddy_views.friends'),
    '"Bob"',
    'public graph view exposes JSONB Cypher values'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT has_schema_privilege('public', '_pg_eddy_views', 'USAGE')
            AND has_table_privilege('public', '_pg_eddy_views.friends', 'SELECT')}
    ),
    't',
    'public role can read the graph-view projection'
);

my ($invalid_stdout, $invalid_stderr) = ('', '');
my $invalid_result = $node->psql(
    'postgres',
    q{SELECT create_graph_view(
        'invalid_write', 'CREATE (:Person)', '{}'::jsonb
    )},
    stdout => \$invalid_stdout,
    stderr => \$invalid_stderr
);
isnt($invalid_result, 0, 'write query is rejected as a graph view');
like($invalid_stderr, qr/PE601:/, 'invalid graph view reports stable PE601 code');

$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'MATCH (a:Person {name: ''Alice''}) CREATE (a)-[:KNOWS]->(:Person {name: ''Carol'', age: 29})',
        NULL::jsonb
    )}
);
$node->safe_psql('postgres', q{SELECT refresh_graph_view('friends')});
is(
    $node->safe_psql(
        'postgres',
        q{SELECT string_agg(friend::text, ',' ORDER BY friend::text)
          FROM _pg_eddy_views.friends}
    ),
    '"Bob","Carol"',
    'differential refresh propagates new Cypher writes'
);

is(
    $node->safe_psql(
        'postgres',
        q{SELECT status FROM list_graph_views() WHERE name = 'friends'}
    ),
    'ACTIVE',
    'local graph-view catalog joins pg_trickle status'
);

$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'two_hop_friends',
        'MATCH (a:Person {name: ''Alice''})-[:KNOWS]->(b:Person) '
        'MATCH (b)-[:KNOWS]->(c:Person) RETURN c.name AS friend',
        '{}'::jsonb,
        '1s',
        'DIFFERENTIAL',
        false,
        false
    )}
);
$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'MATCH (b:Person {name: ''Bob''}) '
        'CREATE (b)-[:KNOWS]->(:Person {name: ''Deep''})',
        NULL::jsonb
    )}
);
$node->safe_psql('postgres', q{SELECT refresh_graph_view('two_hop_friends')});
is(
    $node->safe_psql(
        'postgres',
        q{SELECT friend FROM _pg_eddy_views.two_hop_friends}
    ),
    '"Deep"',
    'chained MATCH graph view materializes through pg_trickle'
);
$node->safe_psql('postgres', q{SELECT drop_graph_view('two_hop_friends')});

$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'people_union',
        'MATCH (p:Person {name: ''Alice''}) RETURN p.name AS name '
        'UNION ALL '
        'MATCH (p:Person {name: ''Bob''}) RETURN p.name AS name',
        '{}'::jsonb,
        '1s',
        'DIFFERENTIAL',
        false,
        false
    )}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT string_agg(name::text, ',' ORDER BY name::text)
          FROM _pg_eddy_views.people_union}
    ),
    '"Alice","Bob"',
    'UNION ALL graph view materializes compatible branches'
);
$node->safe_psql('postgres', q{SELECT drop_graph_view('people_union')});

$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'people_live',
        'MATCH (p:Person) RETURN p.name AS name',
        '{}'::jsonb,
        '1s',
        'IMMEDIATE',
        false,
        false
    )}
);
$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'CREATE (:Person {name: ''Dora'', age: 27})', NULL::jsonb
    )}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) FROM _pg_eddy_views.people_live WHERE name = '"Dora"'::jsonb}
    ),
    '1',
    'IMMEDIATE graph view reflects Cypher writes without manual refresh'
);
$node->safe_psql('postgres', q{SELECT drop_graph_view('people_live')});

my $existing_violation = $node->safe_psql(
    'postgres',
    q{SELECT create_node(
        ARRAY['ExistingViolation'],
        '{"name":"Already invalid"}'::jsonb
    )}
);
my ($initial_constraint_stdout, $initial_constraint_stderr) = ('', '');
my $initial_constraint_result = $node->psql(
    'postgres',
    q{SELECT create_graph_view(
        'invalid_initial',
        'MATCH (p:ExistingViolation) RETURN p AS offender',
        '{}'::jsonb,
        '1s',
        'IMMEDIATE',
        false,
        true
    )},
    stdout => \$initial_constraint_stdout,
    stderr => \$initial_constraint_stderr
);
isnt($initial_constraint_result, 0, 'constraint creation rejects existing violations');
like(
    $initial_constraint_stderr,
    qr/PE607:/,
    'initial constraint violation reports stable PE607 code'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT to_regclass('_pg_eddy_views.invalid_initial') IS NULL
            AND to_regclass('_pg_eddy_views.__pgeddy_invalid_initial') IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM _pg_eddy.graph_views
                WHERE view_name = 'invalid_initial'
            )
            AND NOT EXISTS (
                SELECT 1 FROM pgtrickle.pgt_stream_tables
                WHERE pgt_name = '__pgeddy_invalid_initial'
            )}
    ),
    't',
    'failed constraint creation rolls back every lifecycle artifact'
);
$node->safe_psql('postgres', qq{SELECT delete_node($existing_violation)});

$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'missing_email',
        'MATCH (p:ConstrainedPerson) WHERE p.email IS NULL RETURN p AS offender',
        '{}'::jsonb,
        '1s',
        'IMMEDIATE',
        false,
        true
    )}
);
$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'CREATE (:ConstrainedPerson {name: ''Valid'', email: ''valid@example.com''})',
        NULL::jsonb
    )}
);
$node->safe_psql(
    'postgres',
    q{BEGIN;
      SELECT * FROM cypher(
          'CREATE (:ConstrainedPerson {name: ''Repaired''})', NULL::jsonb
      );
      SELECT * FROM cypher(
          'MATCH (p:ConstrainedPerson {name: ''Repaired''}) '
          'SET p.email = ''repaired@example.com''', NULL::jsonb
      );
      COMMIT;}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) FROM _pg_eddy.ivm_nodes
          WHERE labels @> ARRAY['ConstrainedPerson']::text[]}
    ),
    '2',
    'deferred constraint allows a violation repaired before commit'
);
my ($constraint_stdout, $constraint_stderr) = ('', '');
my $constraint_result = $node->psql(
    'postgres',
    q{SELECT * FROM cypher(
        'CREATE (:ConstrainedPerson {name: ''Invalid''})', NULL::jsonb
    )},
    stdout => \$constraint_stdout,
    stderr => \$constraint_stderr
);
isnt($constraint_result, 0, 'constraint view rejects a violating write');
like($constraint_stderr, qr/PE607:/, 'constraint violation reports stable PE607 code');
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*) FROM _pg_eddy.ivm_nodes
          WHERE labels @> ARRAY['ConstrainedPerson']::text[]}
    ),
    '2',
    'constraint failure rolls back the violating mirror write'
);
$node->safe_psql('postgres', q{SELECT drop_graph_view('missing_email')});
is(
    $node->safe_psql(
        'postgres',
        q{SELECT NOT EXISTS (
            SELECT 1 FROM pg_trigger
            WHERE tgname IN (
                'pgeddy_gvc_n_missing_email',
                'pgeddy_gvc_e_missing_email'
            )
        )}
    ),
    't',
    'dropping a constraint view removes both deferred triggers'
);

$node->safe_psql('postgres', q{SELECT drop_graph_view('friends')});
is(
    $node->safe_psql(
        'postgres',
        q{SELECT to_regclass('_pg_eddy_views.friends') IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM _pg_eddy.graph_views WHERE view_name = 'friends'
            )}
    ),
    't',
    'drop removes projection view and local metadata'
);

$node->safe_psql('postgres', 'SELECT clear()');
is(
    $node->safe_psql(
        'postgres',
        q{SELECT (SELECT count(*) FROM _pg_eddy.ivm_nodes)
                + (SELECT count(*) FROM _pg_eddy.ivm_edges)}
    ),
    '0',
    'clear removes typed mirror rows'
);

$node->stop;
done_testing();