#!/usr/bin/env perl
use strict;
use warnings;

use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Time::HiRes qw(gettimeofday tv_interval);

my $rows = $ENV{WAL_PRODUCER_BENCH_ROWS} // 10_000;
my $warmup_rows = $ENV{WAL_PRODUCER_WARMUP_ROWS} // 500;
my $reference_us = $ENV{WAL_PRODUCER_REFERENCE_US};
my $max_slot_write_overhead_us = $ENV{WAL_PRODUCER_MAX_SLOT_OVERHEAD_US};
my $slot_name = 'pg_eddy_wal_bench';

my $node = PostgreSQL::Test::Cluster->new('wal_producer_bench');
$node->init(extra => ['--no-sync']);
$node->append_conf(
    'postgresql.conf',
    "shared_preload_libraries = 'pg_eddy'\n" .
    "wal_level = logical\n" .
    "max_replication_slots = 4\n" .
    "max_wal_senders = 4\n" .
    "synchronous_commit = off\n" .
    "shared_buffers = 256MB\n"
);
$node->start;
$node->safe_psql('postgres', 'CREATE EXTENSION pg_eddy;');

sub current_lsn {
    return $node->safe_psql('postgres', 'SELECT pg_current_wal_insert_lsn()::text');
}

sub wal_bytes_between {
    my ($start_lsn, $end_lsn) = @_;
    return 0 + $node->safe_psql(
        'postgres',
        "SELECT pg_wal_lsn_diff('$end_lsn', '$start_lsn')::bigint"
    );
}

sub insert_nodes {
    my ($count) = @_;
    my $start_lsn = current_lsn();
    my $started = [gettimeofday];
    $node->safe_psql(
        'postgres',
        qq{SELECT create_node(
               ARRAY['WalProducerBench'],
               jsonb_build_object('seq', sequence_number, 'active', true)
           )
           FROM generate_series(1, $count) AS generated(sequence_number)}
    );
    my $elapsed = tv_interval($started);
    my $end_lsn = current_lsn();
    return ($elapsed, wal_bytes_between($start_lsn, $end_lsn));
}

insert_nodes($warmup_rows);
$node->safe_psql('postgres', 'SELECT clear()');

my ($no_slot_seconds, $no_slot_wal_bytes) = insert_nodes($rows);
$node->safe_psql('postgres', 'SELECT clear()');

$node->safe_psql(
    'postgres',
    "SELECT slot_name FROM pg_create_logical_replication_slot('$slot_name', 'pg_eddy')"
);
my ($slot_seconds, $slot_wal_bytes) = insert_nodes($rows);

# The write measurement uses synchronous_commit=off to match existing release
# benchmarks. Flush outside the timed region so logical decoding can see the
# committed transaction without charging checkpoint latency to the producer.
$node->safe_psql('postgres', 'SELECT pg_switch_wal(); CHECKPOINT;');

my $drain_started = [gettimeofday];
my $frame_count = 0 + $node->safe_psql(
    'postgres',
    "SELECT count(*) FROM pg_logical_slot_get_binary_changes('$slot_name', NULL, NULL)"
);
my $drain_seconds = tv_interval($drain_started);
my $expected_frames = $rows + 2;

my $no_slot_us = $no_slot_seconds * 1_000_000 / $rows;
my $slot_us = $slot_seconds * 1_000_000 / $rows;
my $slot_overhead_us = $slot_us - $no_slot_us;
my $drain_us = $drain_seconds * 1_000_000 / $rows;
my $drain_rows_per_second = $rows / ($drain_seconds || 0.000_001);
my $no_slot_wal_per_row = $no_slot_wal_bytes / $rows;
my $slot_wal_per_row = $slot_wal_bytes / $rows;

print "\npg_eddy semantic WAL producer benchmark\n";
print "rows: $rows\n";
printf "logical messages, no slot: %.2f us/row (%.3f s), %.1f WAL bytes/row\n",
    $no_slot_us, $no_slot_seconds, $no_slot_wal_per_row;
printf "logical messages, slot present: %.2f us/row (%.3f s), %.1f WAL bytes/row\n",
    $slot_us, $slot_seconds, $slot_wal_per_row;
printf "slot-presence write overhead: %+.2f us/row\n", $slot_overhead_us;
printf "synchronous slot drain: %.2f us/event-row (%.3f s), %.0f mutations/s\n",
    $drain_us, $drain_seconds, $drain_rows_per_second;
printf "write + drain: %.2f us/row\n", $slot_us + $drain_us;
printf "decoded frames: %d (expected %d: BEGIN + %d mutations + COMMIT)\n",
    $frame_count, $expected_frames, $rows;
if (defined $reference_us) {
    printf "historical/reference write latency: %.2f us/row; current no-slot delta: %+.2f us/row\n",
        $reference_us, $no_slot_us - $reference_us;
}

$node->safe_psql('postgres', "SELECT pg_drop_replication_slot('$slot_name')");
$node->stop;

die "decoded $frame_count frames, expected $expected_frames\n"
    if $frame_count != $expected_frames;
if (defined $max_slot_write_overhead_us
    && $slot_overhead_us > $max_slot_write_overhead_us) {
    die sprintf(
        "slot-presence overhead %.2f us/row exceeds configured limit %.2f us/row\n",
        $slot_overhead_us,
        $max_slot_write_overhead_us
    );
}
