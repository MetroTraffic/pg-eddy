#!/usr/bin/env perl
use strict;
use warnings;

use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

my $node = PostgreSQL::Test::Cluster->new('ivm_mutations');
$node->init(extra => ['--no-sync']);
$node->append_conf('postgresql.conf', "shared_preload_libraries = 'pg_eddy'\n");
$node->start;
$node->safe_psql('postgres', 'CREATE EXTENSION pg_eddy;');

my $helper_node = $node->safe_psql(
    'postgres',
    q{SELECT create_node(
        ARRAY['HelperOriginal'],
        '{"keep":1,"remove":2}'::jsonb
    )}
);
is(
    $node->safe_psql(
        'postgres',
        qq{SELECT labels = ARRAY['HelperOriginal']::text[]
              AND properties = '{"keep":1,"remove":2}'::jsonb
           FROM _pg_eddy.ivm_nodes WHERE node_id = $helper_node}
    ),
    't',
    'create_node writes the node mirror'
);

$node->safe_psql(
    'postgres',
    qq{SELECT update_node(
        $helper_node,
        ARRAY['HelperUpdated'],
        '{"keep":3}'::jsonb
    )}
);
is(
    $node->safe_psql(
        'postgres',
        qq{SELECT labels = ARRAY['HelperUpdated']::text[]
              AND properties = '{"keep":3}'::jsonb
           FROM _pg_eddy.ivm_nodes WHERE node_id = $helper_node}
    ),
    't',
    'update_node replaces mirrored labels and properties'
);

$node->safe_psql('postgres', qq{SELECT add_label($helper_node, 'HelperAdded')});
$node->safe_psql('postgres', qq{SELECT remove_label($helper_node, 'HelperUpdated')});
is(
    $node->safe_psql(
        'postgres',
        qq{SELECT labels = ARRAY['HelperAdded']::text[]
           FROM _pg_eddy.ivm_nodes WHERE node_id = $helper_node}
    ),
    't',
    'label helper mutations update the node mirror'
);

my $helper_target = $node->safe_psql(
    'postgres',
    q{SELECT create_node(ARRAY['HelperTarget'], '{}'::jsonb)}
);
my $helper_edge = $node->safe_psql(
    'postgres',
    qq{SELECT create_edge(
        $helper_node,
        $helper_target,
        'HELPER_EDGE',
        '{"weight":1}'::jsonb
    )}
);
is(
    $node->safe_psql(
        'postgres',
        qq{SELECT source_node_id = $helper_node
              AND target_node_id = $helper_target
              AND properties = '{"weight":1}'::jsonb
           FROM _pg_eddy.ivm_edges WHERE rel_id = $helper_edge}
    ),
    't',
    'create_edge writes the edge mirror'
);
$node->safe_psql('postgres', qq{SELECT delete_edge($helper_edge)});
is(
    $node->safe_psql(
        'postgres',
        qq{SELECT NOT EXISTS (
            SELECT 1 FROM _pg_eddy.ivm_edges WHERE rel_id = $helper_edge
        )}
    ),
    't',
    'delete_edge removes the edge mirror row'
);

$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'CREATE (:MutationA {keep: 1, remove: 2})-[:MUTATION_REL {weight: 1, remove: true}]->(:MutationB)',
        NULL::jsonb
    )}
);
$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'MATCH (a:MutationA)-[r:MUTATION_REL]->(b:MutationB) '
        'SET a.keep = 9, a:MutationAdded, r.weight = 7 '
        'REMOVE a.remove, r.remove',
        NULL::jsonb
    )}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT labels @> ARRAY['MutationA','MutationAdded']::text[]
              AND properties = '{"keep":9}'::jsonb
          FROM _pg_eddy.ivm_nodes
          WHERE labels @> ARRAY['MutationA']::text[]}
    ),
    't',
    'Cypher SET and REMOVE update mirrored node labels and properties'
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT properties = '{"weight":7}'::jsonb
          FROM _pg_eddy.ivm_edges WHERE rel_type = 'MUTATION_REL'}
    ),
    't',
    'Cypher SET and REMOVE update mirrored relationship properties'
);

$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'MERGE (n:MergeMirror {key: 1}) '
        'ON CREATE SET n.state = ''created'' '
        'ON MATCH SET n.state = ''matched''',
        NULL::jsonb
    )}
);
$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'MERGE (n:MergeMirror {key: 1}) '
        'ON CREATE SET n.state = ''created'' '
        'ON MATCH SET n.state = ''matched''',
        NULL::jsonb
    )}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT count(*)::text || '/' || min(properties->>'state')
          FROM _pg_eddy.ivm_nodes
          WHERE labels @> ARRAY['MergeMirror']::text[]}
    ),
    '1/matched',
    'Cypher MERGE maintains one mirrored node and applies ON MATCH'
);

$node->safe_psql(
    'postgres',
    q{SELECT * FROM cypher(
        'MATCH (:MutationA)-[r:MUTATION_REL]->(:MutationB) DELETE r',
        NULL::jsonb
    )}
);
is(
    $node->safe_psql(
        'postgres',
        q{SELECT NOT EXISTS (
            SELECT 1 FROM _pg_eddy.ivm_edges WHERE rel_type = 'MUTATION_REL'
        )}
    ),
    't',
    'Cypher relationship DELETE removes the edge mirror row'
);

my $detach_source = $node->safe_psql(
    'postgres',
    q{SELECT create_node(ARRAY['DetachSource'], '{}'::jsonb)}
);
my $detach_target = $node->safe_psql(
    'postgres',
    q{SELECT create_node(ARRAY['DetachTarget'], '{}'::jsonb)}
);
$node->safe_psql(
    'postgres',
    qq{SELECT create_edge(
        $detach_source,
        $detach_target,
        'DETACH_EDGE',
        '{}'::jsonb
    )}
);
$node->safe_psql('postgres', qq{SELECT detach_delete_node($detach_source)});
is(
    $node->safe_psql(
        'postgres',
        qq{SELECT NOT EXISTS (
              SELECT 1 FROM _pg_eddy.ivm_nodes WHERE node_id = $detach_source
           ) AND NOT EXISTS (
              SELECT 1 FROM _pg_eddy.ivm_edges WHERE rel_type = 'DETACH_EDGE'
           )}
    ),
    't',
    'detach_delete_node removes mirrored node and attached edges'
);

$node->safe_psql('postgres', qq{SELECT delete_node($helper_node)});
is(
    $node->safe_psql(
        'postgres',
        qq{SELECT NOT EXISTS (
            SELECT 1 FROM _pg_eddy.ivm_nodes WHERE node_id = $helper_node
        )}
    ),
    't',
    'delete_node removes the node mirror row'
);

$node->stop;
done_testing();
