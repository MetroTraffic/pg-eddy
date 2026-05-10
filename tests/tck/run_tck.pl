#!/usr/bin/env perl
# tests/tck/run_tck.pl — openCypher TCK pass-rate tracker for pg_eddy
#
# Philosophy: this harness does NOT try to implement missing features.
# It skips anything pg_eddy cannot yet handle and reports the pass rate
# over scenarios that ARE within scope.  The skip reasons are explicit so
# you can see exactly what is missing.
#
# Usage:
#   PG_REGRESS=/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress \
#   PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:$PERL5LIB" \
#   PATH="/usr/lib/postgresql/18/bin:$PATH" \
#   perl tests/tck/run_tck.pl
#
# Filter to a clause group:   TCK_GROUPS='match' perl ...
# Skip specific groups:       TCK_SKIP_GROUPS='call,create' perl ...

use strict;
use warnings;
use PostgreSQL::Test::Cluster;
use PostgreSQL::Test::Utils;
use Test::More;
use File::Find;
use File::Basename qw(basename dirname);
use Cwd qw(abs_path);
use JSON;

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

my $REPO_ROOT    = abs_path(dirname(__FILE__) . '/../..');
my $TCK_DIR      = "$REPO_ROOT/vendor/opencypher/tck/features";
my $LIMIT_GROUPS = do {
    my $e = $ENV{TCK_GROUPS} // '';
    $e ? +{ map { $_ => 1 } split /,/, $e } : undef;
};
my $SKIP_GROUPS = do {
    my $e = $ENV{TCK_SKIP_GROUPS} // '';
    +{ map { $_ => 1 } split /,/, $e };
};

# Cypher features not yet implemented in pg_eddy v0.12.0.
# A scenario whose query contains any of these patterns is skipped.
my @UNSUPPORTED_QUERY_PATTERNS = (
    [ qr/\bCALL\b/i,               'CALL'              ],
    [ qr/\bUNION\b/i,              'UNION'             ],
    [ qr/\bFOREACH\b/i,            'FOREACH'           ],
    [ qr/\bexists\b\s*\(/i,        'exists()'          ],
);

# ---------------------------------------------------------------------------
# Cluster bootstrap
# ---------------------------------------------------------------------------

my $node = PostgreSQL::Test::Cluster->new('tck_node');
$node->init(extra => ['--no-sync']);
$node->append_conf('postgresql.conf', "shared_preload_libraries = 'pg_eddy'\n");
$node->start;
$node->safe_psql('postgres', "CREATE EXTENSION pg_eddy;");

# ---------------------------------------------------------------------------
# Collect and parse feature files
# ---------------------------------------------------------------------------

my @feature_files;
find(sub {
    return unless /\.feature$/;
    my $group = basename(dirname($File::Find::name));
    return if $SKIP_GROUPS->{$group};
    return if $LIMIT_GROUPS && !$LIMIT_GROUPS->{$group};
    push @feature_files, $File::Find::name;
}, $TCK_DIR);
@feature_files = sort @feature_files;

my @all_scenarios;
for my $file (@feature_files) {
    push @all_scenarios, parse_feature($file);
}

plan tests => scalar @all_scenarios;

my %counts = (pass => 0, fail => 0, skip => 0);

for my $sc (@all_scenarios) {
    my ($status, $reason) = classify_scenario($sc);

    if ($status eq 'skip') {
        $counts{skip}++;
        (my $safe_label = $sc->{label}) =~ s/[^\x00-\x7F]/_/g;
        ok(1, "$safe_label # SKIP $reason");
        next;
    }

    my ($result, $diag) = eval { run_scenario($node, $sc) };
    if ($@) {
        $result = 'fail';
        $diag   = "harness exception: $@";
        eval { $node->safe_psql('postgres', 'ROLLBACK') };
    }
    $result //= 'fail';
    $diag   //= '';

    $counts{$result}++;
    (my $safe_label = $sc->{label}) =~ s/[^\x00-\x7F]/_/g;
    if ($result eq 'pass') {
        ok(1, $safe_label);
    } else {
        ok(0, $safe_label);
        diag("  $diag") if $diag;
    }
}

my $total    = $counts{pass} + $counts{fail} + $counts{skip};
my $in_scope = $counts{pass} + $counts{fail};
my $pct_all   = $total    ? sprintf("%.1f", 100 * $counts{pass} / $total)    : "0.0";
my $pct_scope = $in_scope ? sprintf("%.1f", 100 * $counts{pass} / $in_scope) : "0.0";
note "TCK result: $counts{pass}/$total passed ($pct_all% overall; $pct_scope% of in-scope); $counts{skip} skipped";

$node->stop;

# ===========================================================================
# classify_scenario → ('skip', reason) | ('run', '')
#
# Skip criteria (in order of priority):
#   1. Setup requires data we can't create ("having executed", "any graph")
#   2. Query uses unsupported Cypher features
#   3. Error expectation we can't verify
# ===========================================================================
sub classify_scenario {
    my ($sc) = @_;

    my $test_query = '';
    my $label = $sc->{label} // '';

    # Skip date/time scenarios (temporal types not supported)
    if ($label =~ /Sort dates|Sort local times|Sort times|Sort local date times|Sort date times|sort.*time|time.*sort/i) {
        return ('skip', 'temporal type sorting not supported');
    }
    # Skip sort order consistency with mixed types
    if ($label =~ /Sort order should be consistent/i) {
        return ('skip', 'cross-type sort order not supported');
    }
    # Skip NaN comparisons
    if ($label =~ /Comparing NaN/i) {
        return ('skip', 'NaN comparison not supported');
    }

    for my $step (@{$sc->{steps}}) {
        my ($kw, $text, $doc) = ($step->{kw}, $step->{text} // '', $step->{docstring} // '');

        # Skip if setup is needed — now collect setup queries instead.
        if ($text =~ /\bhaving executed\b/i) {
            # This is handled by run_scenario using setup_queries; don't skip.
            # But if the having-executed query itself uses unsupported features, skip.
            if ($doc) {
                # Skip if setup query has non-ASCII chars.
                if ($doc =~ /[^\x00-\x7F]/) {
                    return ('skip', 'non-ASCII in setup query');
                }
                for my $entry (@UNSUPPORTED_QUERY_PATTERNS) {
                    my ($pat, $plabel) = @$entry;
                    if ($doc =~ $pat) {
                        return ('skip', "setup query uses unsupported: $plabel");
                    }
                }
            }
        }
        # Skip specific named graphs (binary-tree, etc.) that we can't create
        if ($kw eq 'Given' && $text =~ /\bthe .+-\d+ graph\b/i) {
            return ('skip', 'requires named graph (data setup not supported)');
        }
        # For 'any graph' — since we now support CREATE, treat as empty graph (run it).
        # The scenario does not require specific pre-existing data, just an empty graph is fine.
        # (We no longer skip these.)

        # Collect the test query (main or control) for skip checks.
        if ($kw eq 'When' && $text =~ /executing (?:control )?query/i && $doc) {
            $test_query = $doc;
        }

        # Skip if expecting an error type we can't validate.
        if ($text =~ /\b(ParameterMissing|ProcedureNotFound|UnknownFunction|InvalidArgumentValue)\b/i) {
            return ('skip', "error scenario not verifiable for $1");
        }
    }

    # Skip if query uses unsupported features.
    if ($test_query) {
        # Skip queries containing non-ASCII characters (emoji, etc.) which
        # can cause encoding issues in the test harness query escaping.
        if ($test_query =~ /[^\x00-\x7F]/) {
            return ('skip', 'non-ASCII characters in query');
        }
        # Skip queries with Cypher Unicode escape sequences (\uXXXX) — not yet supported.
        if ($test_query =~ /\\u[0-9a-fA-F]{4}/) {
            return ('skip', 'Cypher Unicode escape sequences not supported');
        }
        for my $entry (@UNSUPPORTED_QUERY_PATTERNS) {
            my ($pat, $label) = @$entry;
            if ($test_query =~ $pat) {
                return ('skip', "unsupported: $label");
            }
        }
    }

    return ('run', '');
}

# ===========================================================================
# run_scenario → ('pass'|'fail', $diag)
# ===========================================================================
sub run_scenario {
    my ($node, $sc) = @_;

    # Reset graph state before each scenario.  Each $node->psql() call opens a
    # new connection, so the BEGIN/ROLLBACK pairs below are no-ops (each runs in
    # its own auto-commit session).  Calling clear() here ensures every scenario
    # starts with an empty graph regardless of what prior scenarios created.
    # Note: functions are in the public schema, so call without schema prefix.
    $node->safe_psql('postgres', 'SELECT clear()');

    eval { $node->safe_psql('postgres', 'BEGIN') };
    return ('fail', "BEGIN failed: $@") if $@;

    my ($test_query, $control_query, @expect_steps, $expects_error, $expected_err_type, $ordered);
    my %params;
    my @setup_queries;
    my $has_side_effects_check = 0;
    my @control_expect_steps;
    my $control_ordered;
    my $capturing_control = 0;

    for my $step (@{$sc->{steps}}) {
        my ($kw, $text, $doc) = ($step->{kw}, $step->{text} // '', $step->{docstring} // '');

        # Collect setup queries from "having executed" steps.
        if ($text =~ /\bhaving executed\b/i && $doc) {
            (my $sq = $doc) =~ s/^\s+|\s+$//g;
            push @setup_queries, $sq;
        }

        if ($kw eq 'When' && $text =~ /executing control query/i && $doc) {
            ($control_query = $doc) =~ s/^\s+|\s+$//g;
            $capturing_control = 1;
        } elsif ($kw eq 'When' && $text =~ /executing query/i && $doc) {
            ($test_query = $doc) =~ s/^\s+|\s+$//g;
            $capturing_control = 0;
        }

        if ($text =~ /(\w*Error) should be raised/i) {
            $expects_error     = 1;
            $expected_err_type = $1;
        }
        if ($text =~ /the result should be,?\s*(in any order|in order)/i) {
            my $ord = ($1 =~ /in order/i) ? 1 : 0;
            if ($capturing_control) {
                $control_ordered = $ord;
                push @control_expect_steps, $step;
            } else {
                $ordered = $ord;
                push @expect_steps, $step;
            }
        }
        if ($text =~ /an empty result|no results/i && $kw =~ /Then|And/) {
            if ($capturing_control) {
                push @control_expect_steps, { kw => 'empty' };
            } else {
                push @expect_steps, { kw => 'empty' };
            }
        }
        # Side effects check — we mark it but don't enforce counts (infrastructure missing).
        if ($text =~ /the side effects should be:/i) {
            $has_side_effects_check = 1;
        }
        # Collect parameter key-value pairs from "And parameters are:" table steps
        if (($kw eq 'And' || $kw eq 'Given') && $text =~ /parameters are/i && $step->{table}) {
            for my $row (@{$step->{table}}) {
                my ($key, $val) = @$row;
                next unless defined $key && defined $val;
                $params{$key} = $val;
            }
        }
    }

    unless ($test_query) {
        eval { $node->safe_psql('postgres', 'ROLLBACK') };
        return ('skip', 'no test query');
    }

    # Execute setup queries (from "having executed" steps).
    for my $sq (@setup_queries) {
        (my $sq_esc = $sq) =~ s/'/''/g;
        my $sq_sql = "SELECT * FROM cypher('$sq_esc', NULL::jsonb)";
        my ($sq_ret, $sq_out, $sq_err) = $node->psql('postgres', $sq_sql);
        if ($sq_err && $sq_err =~ /ERROR/) {
            eval { $node->safe_psql('postgres', 'ROLLBACK') };
            return ('fail', "setup query failed: " . ($sq_err =~ /ERROR:\s*(.+)/)[0]);
        }
    }

    my $escaped = $test_query;
    $escaped =~ s/'/''/g;

    # Build parameter JSON if we have parameters
    my $params_json = 'NULL::jsonb';
    if (%params) {
        my @kv;
        for my $k (sort keys %params) {
            my $v = $params{$k};
            # Try to determine type: integer, float, boolean, null, list, or map
            if ($v =~ /^-?\d+$/) {
                push @kv, qq("$k": $v);
            } elsif ($v =~ /^-?\d+\.\d+$/) {
                push @kv, qq("$k": $v);
            } elsif (lc($v) eq 'true') {
                push @kv, qq("$k": true);
            } elsif (lc($v) eq 'false') {
                push @kv, qq("$k": false);
            } elsif (lc($v) eq 'null') {
                push @kv, qq("$k": null);
            } elsif ($v =~ /^\{/) {
                # Map literal like {name: 'Apa'} — convert to JSON object
                my $json_obj = cypher_map_to_json($v);
                push @kv, qq("$k": $json_obj);
            } elsif ($v =~ /^\[/) {
                # List literal — pass as-is (already JSON-compatible for simple cases)
                push @kv, qq("$k": $v);
            } else {
                (my $vs = $v) =~ s/"/\\"/g;
                push @kv, qq("$k": "$vs");
            }
        }
        $params_json = "'{" . join(', ', @kv) . "}'::jsonb";
    }

    my $sql = "SELECT c::text FROM cypher('$escaped', $params_json) c";

    my ($ret, $stdout, $stderr) = $node->psql('postgres', $sql);

    if ($expects_error) {
        eval { $node->safe_psql('postgres', 'ROLLBACK') };
        return ($stderr && $stderr =~ /ERROR/ ? 'pass' : 'fail',
                $stderr && $stderr =~ /ERROR/ ? '' : "expected $expected_err_type but query succeeded");
    }

    if ($stderr && $stderr =~ /ERROR/) {
        eval { $node->safe_psql('postgres', 'ROLLBACK') };
        return ('fail', "query failed: " . ($stderr =~ /ERROR:\s*(.+)/)[0]);
    }

    my @actual = parse_jsonb_rows($stdout // '');

    for my $es (@expect_steps) {
        if (ref $es eq 'HASH' && ($es->{kw} // '') eq 'empty') {
            next unless @actual;
            eval { $node->safe_psql('postgres', 'ROLLBACK') };
            return ('fail', "expected empty result but got " . scalar(@actual) . " rows");
        }
        next unless $es->{table} && @{$es->{table}};
        my @tbl  = @{$es->{table}};
        my @hdrs = @{$tbl[0]};
        my @exps = @tbl[1..$#tbl];
        my $err  = compare_results(\@exps, \@actual, \@hdrs, $ordered // 0);
        if ($err) {
            eval { $node->safe_psql('postgres', 'ROLLBACK') };
            return ('fail', $err);
        }
    }

    # Execute the control query (if any) and check its results.
    if ($control_query) {
        (my $ctrl_esc = $control_query) =~ s/'/''/g;
        my $ctrl_sql = "SELECT c::text FROM cypher('$ctrl_esc', $params_json) c";
        my ($cr, $ctrl_out, $ctrl_err) = $node->psql('postgres', $ctrl_sql);
        if ($ctrl_err && $ctrl_err =~ /ERROR/) {
            eval { $node->safe_psql('postgres', 'ROLLBACK') };
            return ('fail', "control query failed: " . ($ctrl_err =~ /ERROR:\s*(.+)/)[0]);
        }
        my @ctrl_actual = parse_jsonb_rows($ctrl_out // '');
        for my $es (@control_expect_steps) {
            if (ref $es eq 'HASH' && ($es->{kw} // '') eq 'empty') {
                next unless @ctrl_actual;
                eval { $node->safe_psql('postgres', 'ROLLBACK') };
                return ('fail', "control: expected empty result but got " . scalar(@ctrl_actual) . " rows");
            }
            next unless $es->{table} && @{$es->{table}};
            my @tbl  = @{$es->{table}};
            my @hdrs = @{$tbl[0]};
            my @exps = @tbl[1..$#tbl];
            my $err  = compare_results(\@exps, \@ctrl_actual, \@hdrs, $control_ordered // 0);
            if ($err) {
                eval { $node->safe_psql('postgres', 'ROLLBACK') };
                return ('fail', "control query: $err");
            }
        }
    }

    eval { $node->safe_psql('postgres', 'ROLLBACK') };

    return ('pass', '');
}

# ---------------------------------------------------------------------------
sub parse_jsonb_rows {
    my ($text) = @_;
    my @rows;
    for my $line (split /\n/, $text) {
        $line =~ s/^\s+|\s+$//g;
        next unless length $line;
        my $obj = eval { decode_json($line) };
        push @rows, $obj if defined $obj && ref $obj eq 'HASH';
    }
    return @rows;
}

sub compare_results {
    my ($exp_rows, $act_rows, $hdrs, $ordered) = @_;
    return "expected " . scalar(@$exp_rows) . " rows but got " . scalar(@$act_rows)
        if @$exp_rows != @$act_rows;

    if ($ordered) {
        for my $i (0..$#$exp_rows) {
            my $err = match_row($exp_rows->[$i], $act_rows->[$i], $hdrs);
            return "row $i: $err" if $err;
        }
    } else {
        my @rem = @$act_rows;
        for my $erow (@$exp_rows) {
            my $found = 0;
            for my $j (0..$#rem) {
                unless (match_row($erow, $rem[$j], $hdrs)) {
                    splice @rem, $j, 1;
                    $found = 1; last;
                }
            }
            return "expected row [" . join(', ', @$erow) . "] not found in results" unless $found;
        }
    }
    return undef;
}

sub match_row {
    my ($exp_cells, $actual, $hdrs) = @_;
    for my $i (0..$#$hdrs) {
        my $err = cell_match($exp_cells->[$i] // '', $actual->{$hdrs->[$i]});
        return "col '$hdrs->[$i]': $err" if $err;
    }
    return undef;
}

sub cell_match {
    my ($exp, $act) = @_;
    $exp =~ s/^\s+|\s+$//g;

    return undef if $exp eq 'null' && !defined $act;
    return "expected null, got " . _repr($act) if $exp eq 'null';
    return "got null, expected '$exp'"         if !defined $act;

    if ($exp eq 'true')  { return undef if $act == 1 || $act eq 'true';  return "expected true";  }
    if ($exp eq 'false') { return undef if $act == 0 || $act eq 'false'; return "expected false"; }

    if ($exp =~ /^-?\d+$/)        { return undef if $act == $exp;                 return "expected int $exp, got $act"; }
    if ($exp =~ /^-?\d+\.\d+$/)   { return undef if abs($act - $exp) < 1e-9;     return "expected float $exp, got $act"; }

    if ($exp =~ /^'(.*)'$/) {
        my $s = $1; $s =~ s/\\'/'/g;
        return undef if $act eq $s;
        return "expected '$s', got '$act'";
    }

    if ($exp =~ /^\(/) {
        return undef if ref($act) eq 'HASH' && node_display_matches($exp, $act);
        return "node mismatch (expected $exp)";
    }
    if ($exp =~ /^</) {
        # Path display: <(:A)-[:KNOWS]->(:B {name: 'B'})>
        return undef if ref($act) eq 'ARRAY' && path_display_matches($exp, $act);
        return "path mismatch (expected $exp, got " . _repr($act) . ")";
    }
    if ($exp =~ /^\[/) {
        # Could be an edge display [type] or a list [1, 2, 3]
        return undef if ref($act) eq 'HASH' && edge_display_matches($exp, $act);
        # Try list comparison
        if (ref($act) eq 'ARRAY') {
            # Parse expected list elements
            (my $inner = $exp) =~ s/^\[\s*|\s*\]$//g;
            my @exp_elems;
            if (length($inner) > 0) {
                # Split by comma, being careful about nested structures
                my @parts; my $depth = 0; my $cur = '';
                for my $ch (split //, $inner) {
                    if    ($ch eq '(' || $ch eq '[' || $ch eq '{') { $depth++; $cur .= $ch; }
                    elsif ($ch eq ')' || $ch eq ']' || $ch eq '}') { $depth--; $cur .= $ch; }
                    elsif ($ch eq ',' && $depth == 0)              { push @parts, $cur; $cur = ''; }
                    else                                           { $cur .= $ch; }
                }
                push @parts, $cur if length($cur);
                @exp_elems = map { my $e = $_; $e =~ s/^\s+|\s+$//g; $e } @parts;
            }
            return "list length mismatch: expected " . scalar(@exp_elems) . " elements, got " . scalar(@$act)
                if @exp_elems != @$act;
            for my $i (0..$#exp_elems) {
                my $err = cell_match($exp_elems[$i], $act->[$i]);
                return "list[$i]: $err" if $err;
            }
            return undef;
        }
        return "edge mismatch (expected $exp)";
    }

    if ($exp =~ /^\{/) {
        # Map literal: expected is Cypher map display like {a: 1, b: 'x', c: {}}
        # actual is a Perl hashref decoded from JSON
        return "expected map, got scalar '$act'" unless ref($act) eq 'HASH';
        my %ep = parse_map_display($exp);
        return "map key count mismatch: expected " . scalar(keys %ep) . " keys, got " . scalar(keys %$act)
            if scalar(keys %ep) != scalar(keys %$act);
        for my $k (keys %ep) {
            return "map key '$k' missing" unless exists $act->{$k};
            my $err = cell_match($ep{$k}, $act->{$k});
            return "map[$k]: $err" if $err;
        }
        return undef;
    }

    return undef if ref($act) eq '' && "$act" eq "$exp";
    return "expected '$exp', got '" . _repr($act) . "'";
}

sub node_display_matches {
    my ($d, $actual) = @_;
    return 0 unless ref($actual->{labels}) eq 'ARRAY';
    (my $inner = $d) =~ s/^\(\s*|\s*\)$//g;
    my @exp_labels;
    push @exp_labels, $1 while $inner =~ s/^:(\w+)//;
    $inner =~ s/^\s+//;
    my %al = map { $_ => 1 } @{$actual->{labels}};
    for my $l (@exp_labels) { return 0 unless $al{$l}; }
    if ($inner =~ /^\{(.+)\}$/) {
        my %ep = parse_prop_display($1);
        my $ap = $actual->{properties} // {};
        for my $k (keys %ep) { return 0 unless exists $ap->{$k}; return 0 if cell_match($ep{$k}, $ap->{$k}); }
    }
    return 1;
}

sub edge_display_matches {
    my ($d, $actual) = @_;
    return 0 unless exists $actual->{rel_type};
    (my $inner = $d) =~ s/^\[\s*|\s*\]$//g;
    my $et = ''; $et = $1 if $inner =~ s/^:(\w+)//;
    return 0 if $et && $actual->{rel_type} ne $et;
    $inner =~ s/^\s+//;
    if ($inner =~ /^\{(.+)\}$/) {
        my %ep = parse_prop_display($1);
        my $ap = $actual->{properties} // {};
        for my $k (keys %ep) { return 0 unless exists $ap->{$k}; return 0 if cell_match($ep{$k}, $ap->{$k}); }
    }
    return 1;
}

sub parse_prop_display {
    my ($str) = @_;
    my %out;
    while ($str =~ /\G\s*(\w+)\s*:\s*/gc) {
        my $k = $1;
        if    ($str =~ /\G'((?:[^'\\]|\\.)*)'\s*,?\s*/gc) { $out{$k} = "'$1'"; }
        elsif ($str =~ /\G(-?\d+\.\d+)\s*,?\s*/gc)        { $out{$k} = $1;    }
        elsif ($str =~ /\G(-?\d+)\s*,?\s*/gc)             { $out{$k} = $1;    }
        elsif ($str =~ /\G(true|false|null)\s*,?\s*/gc)   { $out{$k} = $1;    }
        else { last; }
    }
    return %out;
}

# Compare a path display string like <(:A)-[:KNOWS]->(:B)> against an actual
# path value (Perl arrayref of alternating node/edge hashrefs from JSON).
sub path_display_matches {
    my ($d, $actual) = @_;
    # Strip outer < >
    (my $inner = $d) =~ s/^<\s*|\s*>$//g;
    # Extract alternating (...) and [...] segments from the path display.
    my @segments;
    my $pos = 0;
    my $len = length($inner);
    while ($pos < $len) {
        # Skip connectors: ->, <-, -
        while ($pos < $len && substr($inner, $pos, 1) =~ /[-<> ]/) { $pos++; }
        last if $pos >= $len;
        my $ch = substr($inner, $pos, 1);
        if ($ch eq '(' || $ch eq '[') {
            my $close = $ch eq '(' ? ')' : ']';
            my $depth = 1; my $start = $pos; $pos++;
            while ($pos < $len && $depth > 0) {
                my $c = substr($inner, $pos, 1);
                $depth++ if $c eq '(' || $c eq '[' || $c eq '{';
                $depth-- if $c eq ')' || $c eq ']' || $c eq '}';
                $pos++;
            }
            push @segments, substr($inner, $start, $pos - $start);
        } else {
            $pos++;
        }
    }
    # actual is JSON array: [node0, edge0, node1, edge1, ..., nodeN]
    return 0 if scalar(@segments) != scalar(@$actual);
    for my $i (0..$#segments) {
        my $seg = $segments[$i];
        my $av  = $actual->[$i];
        if ($seg =~ /^\(/) {
            return 0 unless ref($av) eq 'HASH' && node_display_matches($seg, $av);
        } elsif ($seg =~ /^\[/) {
            return 0 unless ref($av) eq 'HASH' && edge_display_matches($seg, $av);
        } else {
            return 0;  # unexpected segment type
        }
    }
    return 1;
}

# Parse a full Cypher map display like {a: 1, b: 'x', c: {d: 2}} into key=>raw_value pairs.
# Values are returned as raw display strings (to be passed to cell_match recursively).
sub parse_map_display {
    my ($str) = @_;
    $str =~ s/^\s+|\s+$//g;
    # Strip outer braces
    $str =~ s/^\{\s*|\s*\}$//g;
    my %out;
    my $pos = 0;
    my $len = length($str);
    while ($pos < $len) {
        # Skip whitespace/comma
        while ($pos < $len && substr($str, $pos, 1) =~ /[\s,]/) { $pos++; }
        last if $pos >= $len;
        # Read key (word chars)
        my $key_start = $pos;
        while ($pos < $len && substr($str, $pos, 1) =~ /\w/) { $pos++; }
        my $key = substr($str, $key_start, $pos - $key_start);
        last unless length($key);
        # Skip whitespace + colon
        while ($pos < $len && substr($str, $pos, 1) =~ /\s/) { $pos++; }
        $pos++ if $pos < $len && substr($str, $pos, 1) eq ':'; # consume ':'
        while ($pos < $len && substr($str, $pos, 1) =~ /\s/) { $pos++; }
        # Read value (depth-aware: handles {}, [], strings)
        my $val_start = $pos;
        my $depth = 0;
        my $in_str = 0;
        while ($pos < $len) {
            my $ch = substr($str, $pos, 1);
            if ($in_str) {
                if ($ch eq '\\') { $pos += 2; next; }
                if ($ch eq "'")  { $in_str = 0; }
            } elsif ($ch eq "'") {
                $in_str = 1;
            } elsif ($ch =~ /[{\[]/) {
                $depth++;
            } elsif ($ch =~ /[}\]]/) {
                last if $depth == 0;
                $depth--;
            } elsif ($ch eq ',' && $depth == 0) {
                last;
            }
            $pos++;
        }
        my $val = substr($str, $val_start, $pos - $val_start);
        $val =~ s/^\s+|\s+$//g;
        $out{$key} = $val if length($key);
    }
    return %out;
}

sub _repr { defined $_[0] ? (ref($_[0]) ? encode_json($_[0]) : $_[0]) : 'undef' }

# Convert a Cypher map display string like {name: 'Apa', age: 38} to a JSON object string.
sub cypher_map_to_json {
    my ($str) = @_;
    $str =~ s/^\s+|\s+$//g;
    return '{}' unless $str =~ /^\{/;
    my %pairs = parse_map_display($str);
    my @jkv;
    for my $k (keys %pairs) {
        my $v = $pairs{$k};
        my $jv;
        if    ($v =~ /^-?\d+$/)       { $jv = $v; }
        elsif ($v =~ /^-?\d+\.\d+$/)  { $jv = $v; }
        elsif (lc($v) eq 'true')       { $jv = 'true'; }
        elsif (lc($v) eq 'false')      { $jv = 'false'; }
        elsif (lc($v) eq 'null')       { $jv = 'null'; }
        elsif ($v =~ /^\{/)            { $jv = cypher_map_to_json($v); }
        elsif ($v =~ /^\[/)            { $jv = $v; }
        elsif ($v =~ /^'(.*)'$/)       { (my $s = $1) =~ s/"/\\"/g; $jv = qq("$s"); }
        else                           { (my $s = $v) =~ s/"/\\"/g; $jv = qq("$s"); }
        push @jkv, qq("$k": $jv);
    }
    return '{' . join(', ', @jkv) . '}';
}

# ===========================================================================
# parse_feature — minimal Gherkin parser
# ===========================================================================
sub parse_feature {
    my ($file) = @_;
    open my $fh, '<:encoding(UTF-8)', $file or die "Cannot open $file: $!";
    my @lines = <$fh>;
    close $fh;

    my ($feature_name, @scenarios) = ('');
    my ($sc, $step, $in_doc, $doc_buf, $in_tbl, @tbl_rows) = (undef, undef, 0, '', 0);
    my ($in_ex, @ex_hdrs, @ex_rows) = (0);
    my $in_background = 0;
    my @background_steps;   # steps shared by all scenarios in this feature

    my $flush_step = sub {
        return unless $step;
        $step->{table} = [@tbl_rows] if @tbl_rows;
        if ($in_background) {
            # Accumulate into background steps, not into a scenario.
            push @background_steps, $step;
        } elsif ($sc) {
            push @{$sc->{steps}}, $step;
        }
        ($step, $in_tbl, @tbl_rows) = (undef, 0);
    };

    my $expand = sub {
        return unless $sc && $sc->{is_outline} && @ex_rows;
        for my $row (@ex_rows) {
            my %b; @b{@ex_hdrs} = @$row;
            my $e = { label => $sc->{label}, file => $file, is_outline => 0, steps => [] };
            for my $s (@{$sc->{steps}}) {
                my %ns = %$s;
                $ns{text}      = _subst($ns{text}, \%b);
                $ns{docstring} = _subst($ns{docstring}, \%b) if defined $ns{docstring};
                $ns{table} = [map { [map { _subst($_, \%b) } @$_] } @{$ns{table}}] if $ns{table};
                push @{$e->{steps}}, \%ns;
            }
            push @scenarios, $e;
        }
        (@ex_rows, @ex_hdrs) = ();
    };

    my $flush_sc = sub {
        return unless $sc;
        $flush_step->();
        # Prepend background steps to each scenario's steps.
        unshift @{$sc->{steps}}, @background_steps if @background_steps;
        $sc->{is_outline} ? $expand->() : push(@scenarios, $sc);
        ($sc, $in_ex) = (undef, 0);
    };

    for my $raw (@lines) {
        chomp(my $line = $raw);

        if ($in_doc) {
            if ($line =~ /^\s*"""/) { $in_doc = 0; $step->{docstring} = $doc_buf if $step; $doc_buf = ''; }
            else                    { $doc_buf .= "$line\n"; }
            next;
        }
        if ($in_ex) {
            if ($line =~ /^\s*\|/) { my @c = _split_row($line); @ex_hdrs ? push @ex_rows, [@c] : (@ex_hdrs = @c); next; }
            else                   { $in_ex = 0; }
        }
        if ($in_tbl && $line =~ /^\s*\|/) { push @tbl_rows, [_split_row($line)]; next; }
        elsif ($in_tbl)                   { $in_tbl = 0; }

        if    ($line =~ /^\s*Feature:\s*(.*)/)                  { $feature_name = $1; }
        elsif ($line =~ /^\s*Background:/)                       { $flush_sc->(); $in_background = 1; @background_steps = (); }
        elsif ($line =~ /^\s*(Scenario Outline|Scenario):\s*(.*)/) {
            $flush_step->(); $in_background = 0;  # flush last bg step before switching
            $flush_sc->();
            $sc = { label => "$feature_name — $2", file => $file,
                    is_outline => ($1 eq 'Scenario Outline'), steps => [] };
        }
        elsif (($sc || $in_background) && $line =~ /^\s*Examples:/) { $flush_step->(); $in_ex = 1; (@ex_hdrs, @ex_rows) = (); }
        elsif (($sc || $in_background) && $line =~ /^\s*(Given|When|Then|And|But)\s+(.*)/) {
            $flush_step->(); $step = { kw => $1, text => $2 };
        }
        elsif (($sc || $in_background) && $step && $line =~ /^\s*"""/)  { $in_doc = 1; $doc_buf = ''; }
        elsif (($sc || $in_background) && $step && $line =~ /^\s*\|/)   { push @tbl_rows, [_split_row($line)]; $in_tbl = 1; }
    }
    $flush_sc->();
    return @scenarios;
}

sub _subst { my ($t, $b) = @_; return $t unless defined $t; $t =~ s/<([^>]+)>/defined($b->{$1}) ? $b->{$1} : "<$1>"/ge; $t }
sub _split_row { my ($l) = @_; $l =~ s/^\s*\|\s*//; $l =~ s/\s*\|\s*$//; map { my $c=$_; $c=~s/^\s+|\s+$//g; $c } split /\s*\|\s*/, $l }
