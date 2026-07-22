#!/usr/bin/env perl
use strict;
use warnings;

use JSON::PP qw(decode_json);
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;

my $slot_name = 'pg_eddy_semantic_test';

sub take_bytes {
    my ($state, $length) = @_;
    die "truncated frame at offset $state->{offset}"
        if $state->{offset} + $length > length($state->{bytes});
    my $value = substr($state->{bytes}, $state->{offset}, $length);
    $state->{offset} += $length;
    return $value;
}

sub read_u16 { return unpack('n', take_bytes($_[0], 2)); }
sub read_u32 { return unpack('N', take_bytes($_[0], 4)); }
sub read_u64 { return unpack('Q>', take_bytes($_[0], 8)); }
sub read_i64 { return unpack('q>', take_bytes($_[0], 8)); }

sub read_blob {
    my ($state) = @_;
    return take_bytes($state, read_u32($state));
}

sub read_text {
    my ($state) = @_;
    my $value = read_blob($state);
    utf8::decode($value) or die 'invalid UTF-8 in frame';
    return $value;
}

sub read_properties {
    my ($state) = @_;
    my $value = decode_json(read_blob($state));
    die 'properties are not an object' unless ref($value) eq 'HASH';
    return $value;
}

sub read_node {
    my ($state) = @_;
    my $node_id = read_i64($state);
    my $label_count = read_u16($state);
    my @labels = map { read_text($state) } 1 .. $label_count;
    return {
        node_id => $node_id,
        labels => \@labels,
        properties => read_properties($state),
    };
}

sub read_edge {
    my ($state) = @_;
    return {
        rel_id => read_i64($state),
        rel_type => read_text($state),
        source_node_id => read_i64($state),
        target_node_id => read_i64($state),
        properties => read_properties($state),
    };
}

sub decode_frame {
    my ($hex) = @_;
    my $bytes = pack('H*', $hex);
    my $state = { bytes => $bytes, offset => 0 };

    is(take_bytes($state, 4), 'PEDY', 'frame has pg_eddy magic');
    is(read_u16($state), 1, 'frame uses protocol major 1');
    my $kind = unpack('C', take_bytes($state, 1));
    is(unpack('C', take_bytes($state, 1)), 0, 'frame has no unknown flags');
    my $payload_length = read_u32($state);
    is($payload_length, length($bytes) - 12, 'frame payload length is exact');

    my %frame;
    if ($kind == 0x01) {
        %frame = (kind => 'BEGIN', xid => read_u32($state));
    } elsif ($kind == 0x02) {
        %frame = (
            kind => 'COMMIT',
            xid => read_u32($state),
            commit_lsn => read_u64($state),
            end_lsn => read_u64($state),
        );
    } else {
        $frame{event_lsn} = read_u64($state);
        if ($kind == 0x10) {
            @frame{qw(kind new)} = ('NODE_INSERT', read_node($state));
        } elsif ($kind == 0x11) {
            @frame{qw(kind old new)} = ('NODE_UPDATE', read_node($state), read_node($state));
        } elsif ($kind == 0x12) {
            @frame{qw(kind old)} = ('NODE_DELETE', read_node($state));
        } elsif ($kind == 0x20) {
            @frame{qw(kind new)} = ('EDGE_INSERT', read_edge($state));
        } elsif ($kind == 0x21) {
            @frame{qw(kind old new)} = ('EDGE_UPDATE', read_edge($state), read_edge($state));
        } elsif ($kind == 0x22) {
            @frame{qw(kind old)} = ('EDGE_DELETE', read_edge($state));
        } elsif ($kind == 0x30) {
            $frame{kind} = 'GRAPH_RESET';
        } else {
            die sprintf('unknown frame kind 0x%02x', $kind);
        }
    }

    is($state->{offset}, length($bytes), 'frame has no trailing bytes');
    return \%frame;
}

sub slot_rows {
    my ($node, $operation) = @_;
    my $function = $operation eq 'get'
        ? 'pg_logical_slot_get_binary_changes'
        : 'pg_logical_slot_peek_binary_changes';
    my $output = $node->safe_psql(
        'postgres',
        "SELECT lsn::text || E'\\t' || xid::text || E'\\t' || encode(data, 'hex') " .
        "FROM $function('$slot_name', NULL, NULL)"
    );
    return [] if $output eq '';
    return [map {
        my ($lsn, $xid, $hex) = split /\t/, $_, 3;
        +{ lsn => $lsn, xid => $xid + 0, hex => $hex, frame => decode_frame($hex) };
    } split /\n/, $output];
}

sub confirmed_lsn {
    my ($node) = @_;
    return $node->safe_psql(
        'postgres',
        "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name = '$slot_name'"
    );
}

sub create_slot {
    my ($node) = @_;
    is(
        $node->safe_psql(
            'postgres',
            "SELECT slot_name FROM pg_create_logical_replication_slot('$slot_name', 'pg_eddy')"
        ),
        $slot_name,
        'created publication-free pg_eddy logical slot'
    );
}

my $node = PostgreSQL::Test::Cluster->new('wal_cdc_output');
$node->init(extra => ['--no-sync']);
$node->append_conf(
    'postgresql.conf',
    "shared_preload_libraries = 'pg_eddy'\n" .
    "wal_level = logical\n" .
    "max_replication_slots = 4\n" .
    "max_wal_senders = 4\n"
);
$node->start;
$node->safe_psql('postgres', 'CREATE EXTENSION pg_eddy;');
create_slot($node);

$node->safe_psql('postgres', 'CHECKPOINT');
$node->safe_psql(
    'postgres',
    q{BEGIN;
      SELECT create_node(
          ARRAY['WalPerson'],
          jsonb_build_object('name', 'Alice', 'large', repeat('x', 2500))
      );
      SELECT create_node(ARRAY['WalPerson'], '{"name":"Bob"}'::jsonb);
      SELECT create_edge(1, 2, 'WAL_REL', '{"weight":1}'::jsonb);
      COMMIT;}
);

my $before_peek = confirmed_lsn($node);
my $created = slot_rows($node, 'peek');
is_deeply(
    [map { $_->{frame}{kind} } @$created],
    [qw(BEGIN NODE_INSERT NODE_INSERT EDGE_INSERT COMMIT)],
    'committed multi-event transaction is framed in order'
);
is($created->[1]{frame}{new}{properties}{name}, 'Alice', 'node insert carries properties');
is(length($created->[1]{frame}{new}{properties}{large}), 2500, 'overflow-sized property is complete');
is_deeply($created->[1]{frame}{new}{labels}, ['WalPerson'], 'node insert carries public labels');
is($created->[3]{frame}{new}{rel_type}, 'WAL_REL', 'edge insert carries public relationship type');
is($created->[3]{frame}{new}{source_node_id}, 1, 'edge insert carries source id');
is($created->[3]{frame}{new}{target_node_id}, 2, 'edge insert carries target id');
ok($created->[1]{frame}{event_lsn} > 0, 'mutation frame carries message LSN');
is($created->[0]{frame}{xid}, $created->[-1]{frame}{xid}, 'BEGIN and COMMIT xids match');
ok($created->[-1]{frame}{end_lsn} >= $created->[-1]{frame}{commit_lsn}, 'COMMIT carries durable end LSN');
is(confirmed_lsn($node), $before_peek, 'peek does not advance the slot');
is_deeply(slot_rows($node, 'peek'), $created, 'peek re-delivers identical frames');
is_deeply(slot_rows($node, 'get'), $created, 'get returns the validated frames');
is_deeply(slot_rows($node, 'peek'), [], 'get advances beyond consumed transaction');

$node->safe_psql(
    'postgres',
    q{BEGIN;
      SELECT create_node(ARRAY['RolledBack'], '{"name":"rollback"}'::jsonb);
      ROLLBACK;}
);
is_deeply(slot_rows($node, 'peek'), [], 'rolled-back transaction emits no frames');

$node->safe_psql(
    'postgres',
    q{BEGIN;
      SAVEPOINT doomed;
      SELECT create_node(ARRAY['SavepointRolledBack'], '{"name":"doomed"}'::jsonb);
      ROLLBACK TO SAVEPOINT doomed;
      SELECT create_node(ARRAY['WalPerson'], '{"name":"Committed"}'::jsonb);
      COMMIT;}
);
my $savepoint = slot_rows($node, 'get');
is_deeply(
    [map { $_->{frame}{kind} } @$savepoint],
    [qw(BEGIN NODE_INSERT COMMIT)],
    'savepoint rollback suppresses only rolled-back semantic event'
);
is($savepoint->[1]{frame}{new}{properties}{name}, 'Committed', 'post-savepoint mutation is retained');
my $committed_node = $savepoint->[1]{frame}{new}{node_id};

$node->safe_psql(
    'postgres',
    qq{BEGIN;
       SELECT update_node(
           $committed_node,
           ARRAY['WalPerson','Updated'],
           '{"name":"Committed","version":2}'::jsonb
       );
       SELECT * FROM cypher(
           'MATCH (:WalPerson {name: ''Alice''})-[r:WAL_REL]->(:WalPerson {name: ''Bob''}) '
           'SET r.weight = 2',
           NULL::jsonb
       );
       COMMIT;}
);
my $updated = slot_rows($node, 'get');
is_deeply(
    [map { $_->{frame}{kind} } @$updated],
    [qw(BEGIN NODE_UPDATE EDGE_UPDATE COMMIT)],
    'node and edge updates emit complete update frames'
);
is($updated->[1]{frame}{old}{properties}{version}, undef, 'node update OLD omits new property');
is($updated->[1]{frame}{new}{properties}{version}, 2, 'node update NEW carries changed property');
is_deeply($updated->[1]{frame}{new}{labels}, ['WalPerson', 'Updated'], 'node update NEW carries changed labels');
is($updated->[2]{frame}{old}{properties}{weight}, 1, 'edge update carries OLD properties');
is($updated->[2]{frame}{new}{properties}{weight}, 2, 'edge update carries NEW properties');

$node->safe_psql(
    'postgres',
    qq{BEGIN;
       SELECT delete_edge(1);
       SELECT delete_node($committed_node);
       COMMIT;}
);
my $deleted = slot_rows($node, 'get');
is_deeply(
    [map { $_->{frame}{kind} } @$deleted],
    [qw(BEGIN EDGE_DELETE NODE_DELETE COMMIT)],
    'deletes emit complete OLD rows'
);
is($deleted->[1]{frame}{old}{properties}{weight}, 2, 'edge delete OLD sees latest properties');
is($deleted->[2]{frame}{old}{properties}{version}, 2, 'node delete OLD sees latest properties');

$node->safe_psql('postgres', 'SELECT clear()');
my $reset = slot_rows($node, 'get');
is_deeply(
    [map { $_->{frame}{kind} } @$reset],
    [qw(BEGIN GRAPH_RESET COMMIT)],
    'clear emits one graph-reset transaction'
);

my $bad_frame = unpack('H*', pack('a4nCCNQ>', 'PEDY', 2, 0x30, 0, 8, 0));
$node->safe_psql(
    'postgres',
    "SELECT pg_logical_emit_message(true, 'pg_eddy/v1', decode('$bad_frame', 'hex'))"
);
my $before_bad = confirmed_lsn($node);
my ($bad_stdout, $bad_stderr) = ('', '');
my $bad_status = $node->psql(
    'postgres',
    "SELECT * FROM pg_logical_slot_peek_binary_changes('$slot_name', NULL, NULL)",
    stdout => \$bad_stdout,
    stderr => \$bad_stderr
);
isnt($bad_status, 0, 'malformed protocol frame is rejected');
like($bad_stderr, qr/unsupported CDC protocol major version 2/, 'malformed frame reports protocol error');
is(confirmed_lsn($node), $before_bad, 'failed decode does not advance slot');
$node->safe_psql('postgres', "SELECT pg_drop_replication_slot('$slot_name')");
create_slot($node);

$node->safe_psql(
    'postgres',
    q{SELECT create_node(ARRAY['RestartReplay'], '{"name":"replay"}'::jsonb)}
);
my $before_restart = slot_rows($node, 'peek');
is_deeply(
    [map { $_->{frame}{kind} } @$before_restart],
    [qw(BEGIN NODE_INSERT COMMIT)],
    'restart transaction is visible before restart'
);
$node->restart;
my $after_restart = slot_rows($node, 'peek');
is_deeply($after_restart, $before_restart, 'unconsumed transaction is re-delivered after restart');
slot_rows($node, 'get');

$node->safe_psql('postgres', "SELECT pg_drop_replication_slot('$slot_name')");
$node->stop;
done_testing();
