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
use Time::HiRes qw(gettimeofday tv_interval);

$| = 1;

my $rows = $ENV{IVM_BENCH_ROWS} // 10_000;
my $warmup_rows = $ENV{IVM_BENCH_WARMUP_ROWS} // 500;
my $max_overhead_us = $ENV{IVM_MAX_TRIGGER_OVERHEAD_US};
my $max_wal_overhead_us = $ENV{IVM_MAX_WAL_OVERHEAD_US};
my $min_wal_speedup = $ENV{IVM_MIN_WAL_SPEEDUP};
my $output_path = $ENV{IVM_BENCH_OUTPUT};

my $repo = abs_path("$FindBin::Bin/..");
my $eddy_package = "$repo/target/release/pg_eddy-pg18/usr";
my $trickle_package = "$repo/vendor/pg-trickle/target/release/pg_trickle-pg18/usr";
my $eddy_library_dir = "$eddy_package/lib/postgresql/18/lib";
my $trickle_library_dir = "$trickle_package/lib/postgresql/18/lib";
my $eddy_library = "$eddy_library_dir/pg_eddy.so";
my $trickle_library = "$trickle_library_dir/pg_trickle.so";
my $eddy_extension = "$eddy_package/share/postgresql/18/extension";
my $trickle_extension = "$trickle_package/share/postgresql/18/extension";
die "release packages for pg_eddy and pg_trickle are required\n"
    unless -f $eddy_library
        && -f $trickle_library
        && -f "$eddy_extension/pg_eddy.control"
        && -f "$trickle_extension/pg_trickle.control";

my $control_root = tempdir('pg_eddy_ivm_bench_controls_XXXX', TMPDIR => 1, CLEANUP => 1);
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

my $node = PostgreSQL::Test::Cluster->new('ivm_write_bench');
$node->init(extra => ['--no-sync']);
$node->append_conf(
    'postgresql.conf',
    "extension_control_path = '$control_root:\$system'\n" .
    "dynamic_library_path = '$eddy_library_dir:$trickle_library_dir:\$libdir'\n" .
    "shared_preload_libraries = 'pg_eddy,pg_trickle'\n" .
    "max_worker_processes = 16\n" .
    "wal_level = logical\n" .
    "max_replication_slots = 8\n" .
    "max_wal_senders = 8\n" .
    "pg_trickle.scheduler_interval_ms = 100\n" .
    "synchronous_commit = off\n" .
    "shared_buffers = 256MB\n"
);
$node->start;
$node->safe_psql('postgres', 'CREATE EXTENSION pg_trickle; CREATE EXTENSION pg_eddy;');
my $versions = $node->safe_psql(
        'postgres',
        q{SELECT string_agg(extname || '=' || extversion, ', ' ORDER BY extname)
            FROM pg_extension WHERE extname IN ('pg_eddy', 'pg_trickle')}
);
print "extensions: $versions\n";

sub insert_nodes {
    my ($label, $count) = @_;
    my $started = [gettimeofday];
    $node->safe_psql(
        'postgres',
        qq{SELECT create_node(
               ARRAY['$label'],
               jsonb_build_object('seq', sequence_number)
           )
           FROM generate_series(1, $count) AS generated(sequence_number)}
    );
    return tv_interval($started);
}

sub wait_for_custom_wal {
    for (1 .. 300) {
        my $active = $node->safe_psql(
            'postgres',
            q{SELECT bool_and(d.cdc_mode = 'PG_EDDY_WAL')
              FROM pgtrickle.pgt_dependencies d
              JOIN pgtrickle.pgt_stream_tables st USING (pgt_id)
              WHERE st.pgt_name = '__pgeddy_ivm_bench_nodes'
                AND d.source_type = 'TABLE'}
        );
        return if $active eq 't';
        $node->safe_psql('postgres', 'SELECT pg_sleep(0.1)');
    }
    die "timed out waiting for pg_eddy_wal activation\n";
}

insert_nodes('IvmBenchWarmup', $warmup_rows);
$node->safe_psql('postgres', 'SELECT clear()');

print "measuring typed-mirror baseline...\n";
my $baseline_seconds = insert_nodes('IvmBenchNode', $rows);
$node->safe_psql('postgres', 'SELECT clear()');

print "measuring trigger CDC...\n";
$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'ivm_bench_nodes',
        'MATCH (n:IvmBenchNode) RETURN n.seq AS seq',
        '{}'::jsonb,
        '1h',
        'DIFFERENTIAL',
        false,
        false
    )}
);
my $ivm_seconds = insert_nodes('IvmBenchNode', $rows);
$node->safe_psql('postgres', q{SELECT drop_graph_view('ivm_bench_nodes'); SELECT clear()});

print "measuring semantic-WAL CDC...\n";
$node->safe_psql(
    'postgres',
    q{SELECT create_graph_view(
        'ivm_bench_nodes',
        'MATCH (n:IvmBenchNode) RETURN n.seq AS seq',
        '{}'::jsonb,
        '1h',
        'DIFFERENTIAL',
        true,
        false
    )}
);
wait_for_custom_wal();
my $wal_seconds = insert_nodes('IvmBenchNode', $rows);
$node->safe_psql(
    'postgres',
    q{CHECKPOINT; SELECT * FROM pgtrickle.poll_pg_eddy_connector()}
);

my $baseline_us = $baseline_seconds * 1_000_000 / $rows;
my $ivm_us = $ivm_seconds * 1_000_000 / $rows;
my $wal_us = $wal_seconds * 1_000_000 / $rows;
my $overhead_us = $ivm_us - $baseline_us;
my $wal_overhead_us = $wal_us - $baseline_us;
my $overhead_pct = $baseline_us > 0 ? 100 * $overhead_us / $baseline_us : 0;
my $wal_overhead_pct = $baseline_us > 0 ? 100 * $wal_overhead_us / $baseline_us : 0;
my $wal_latency_speedup = $wal_us > 0 ? $ivm_us / $wal_us : 0;
my $incremental_speedup = $wal_overhead_us > 0
    ? $overhead_us / $wal_overhead_us
    : undef;

my $report = "\npg_eddy IVM write-overhead benchmark\n";
$report .= "rows: $rows\n";
$report .= sprintf(
    "baseline (typed mirror, no graph view): %.2f us/row (%.3f s)\n",
    $baseline_us,
    $baseline_seconds
);
$report .= sprintf(
    "trigger IVM (one differential graph view): %.2f us/row (%.3f s)\n",
    $ivm_us,
    $ivm_seconds
);
$report .= sprintf(
    "semantic-WAL IVM (decode=true): %.2f us/row (%.3f s)\n",
    $wal_us,
    $wal_seconds
);
$report .= sprintf(
    "trigger incremental overhead: %.2f us/row (%+.1f%%)\n",
    $overhead_us,
    $overhead_pct
);
$report .= sprintf(
    "semantic-WAL incremental overhead: %.2f us/row (%+.1f%%)\n",
    $wal_overhead_us,
    $wal_overhead_pct
);
$report .= sprintf(
    "total write-latency improvement vs trigger: %.2fx\n",
    $wal_latency_speedup
);
if (defined $incremental_speedup) {
    $report .= sprintf("incremental CDC overhead reduction: %.2fx\n", $incremental_speedup);
} else {
    $report .= "incremental CDC overhead reduction: below measurement noise\n";
}
print $report;
if (defined $output_path) {
    open my $output, '>>', $output_path or die "write benchmark report $output_path: $!\n";
    print {$output} $report;
    close $output;
}

$node->safe_psql('postgres', q{SELECT drop_graph_view('ivm_bench_nodes')});
$node->stop;

if (defined $max_overhead_us && $overhead_us > $max_overhead_us) {
    die sprintf(
        "IVM overhead %.2f us/row exceeds configured limit %.2f us/row\n",
        $overhead_us,
        $max_overhead_us
    );
}
if (defined $max_wal_overhead_us && $wal_overhead_us > $max_wal_overhead_us) {
    die sprintf(
        "semantic-WAL overhead %.2f us/row exceeds configured limit %.2f us/row\n",
        $wal_overhead_us,
        $max_wal_overhead_us
    );
}
if (defined $min_wal_speedup && $wal_latency_speedup < $min_wal_speedup) {
    die sprintf(
        "semantic-WAL latency speedup %.2fx is below configured minimum %.2fx\n",
        $wal_latency_speedup,
        $min_wal_speedup
    );
}
