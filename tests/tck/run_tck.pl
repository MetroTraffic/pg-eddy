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

# Cypher features not yet implemented in pg_eddy v0.6.0.
# A scenario whose query contains any of these patterns is skipped.
my @UNSUPPORTED_QUERY_PATTERNS = (
    [ qr/\bOPTIONAL\s+MATCH\b/i,   'OPTIONAL MATCH'    ],
    [ qr/\bWITH\b/i,               'WITH'              ],
    [ qr/\bORDER\s+BY\b/i,         'ORDER BY'          ],
    [ qr/\bSKIP\b/i,               'SKIP'              ],
    [ qr/\bLIMIT\b/i,              'LIMIT'             ],
    [ qr/\bCALL\b/i,               'CALL'              ],
    [ qr/\bCREATE\b/i,             'CREATE'            ],
    [ qr/\bMERGE\b/i,              'MERGE'             ],
    [ qr/\bDELETE\b/i,             'DELETE'            ],
    [ qr/\bSET\b/i,                'SET'               ],
    [ qr/\bREMOVE\b/i,             'REMOVE'            ],
    [ qr/\bUNION\b/i,              'UNION'             ],
    [ qr/\bFOREACH\b/i,            'FOREACH'           ],
    [ qr/\bUNWIND\b/i,             'UNWIND'            ],
    [ qr/\bSTARTS\s+WITH\b/i,      'STARTS WITH'       ],
    [ qr/\bENDS\s+WITH\b/i,        'ENDS WITH'         ],
    [ qr/\bCONTAINS\b/i,           'CONTAINS'          ],
    [ qr/=~/,                      '=~ regex'          ],
    [ qr/\bIN\b\s*\[/i,            'IN [list]'         ],
    [ qr/-\[.*\*.*\]-/,            'variable-length path' ],
    [ qr/\bshortestPath\b/i,       'shortestPath'      ],
    [ qr/\ballShortestPaths\b/i,   'allShortestPaths'  ],
    [ qr/\bcount\b\s*\(/i,         'count()'           ],
    [ qr/\bsum\b\s*\(/i,           'sum()'             ],
    [ qr/\bmin\b\s*\(/i,           'min()'             ],
    [ qr/\bmax\b\s*\(/i,           'max()'             ],
    [ qr/\bavg\b\s*\(/i,           'avg()'             ],
    [ qr/\bcollect\b\s*\(/i,       'collect()'         ],
    [ qr/\bhead\b\s*\(/i,          'head()'            ],
    [ qr/\btail\b\s*\(/i,          'tail()'            ],
    [ qr/\blast\b\s*\(/i,          'last()'            ],
    [ qr/\bsize\b\s*\(/i,          'size()'            ],
    [ qr/\blength\b\s*\(/i,        'length()'          ],
    [ qr/\btoBoolean\b\s*\(/i,     'toBoolean()'       ],
    [ qr/\babs\b\s*\(/i,           'abs()'             ],
    [ qr/\bceil\b\s*\(/i,          'ceil()'            ],
    [ qr/\bfloor\b\s*\(/i,         'floor()'           ],
    [ qr/\bround\b\s*\(/i,         'round()'           ],
    [ qr/\bsign\b\s*\(/i,          'sign()'            ],
    [ qr/\bsqrt\b\s*\(/i,          'sqrt()'            ],
    [ qr/\blog\b\s*\(/i,           'log()'             ],
    [ qr/\bexp\b\s*\(/i,           'exp()'             ],
    [ qr/\bsin\b\s*\(/i,           'sin()'             ],
    [ qr/\bcos\b\s*\(/i,           'cos()'             ],
    [ qr/\btan\b\s*\(/i,           'tan()'             ],
    [ qr/\basin\b\s*\(/i,          'asin()'            ],
    [ qr/\bacos\b\s*\(/i,          'acos()'            ],
    [ qr/\batan\b\s*\(/i,          'atan()'            ],
    [ qr/\batan2\b\s*\(/i,         'atan2()'           ],
    [ qr/\bnodes\b\s*\(/i,         'nodes()'           ],
    [ qr/\brelationships\b\s*\(/i, 'relationships()'   ],
    [ qr/\breverse\b\s*\(/i,       'reverse()'         ],
    [ qr/\brange\b\s*\(/i,         'range()'           ],
    [ qr/\bsplit\b\s*\(/i,         'split()'           ],
    [ qr/\btrim\b\s*\(/i,          'trim()'            ],
    [ qr/\bupper\b\s*\(/i,         'upper()'           ],
    [ qr/\blower\b\s*\(/i,         'lower()'           ],
    [ qr/\bsubstring\b\s*\(/i,     'substring()'       ],
    [ qr/\breplace\b\s*\(/i,       'replace()'         ],
    [ qr/\bextract\b\s*\(/i,       'extract()'         ],
    [ qr/\bfilter\b\s*\(/i,        'filter()'          ],
    [ qr/\bany\b\s*\(/i,           'any()'             ],
    [ qr/\ball\b\s*\(/i,           'all()'             ],
    [ qr/\bnone\b\s*\(/i,          'none()'            ],
    [ qr/\bsingle\b\s*\(/i,        'single()'          ],
    [ qr/\bexists\b\s*\(/i,        'exists()'          ],
    [ qr/\[\s*\]/,                 'empty list literal' ],
    [ qr/\[\s*\d/,                 'list literal'      ],
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

    for my $step (@{$sc->{steps}}) {
        my ($kw, $text, $doc) = ($step->{kw}, $step->{text} // '', $step->{docstring} // '');

        # Skip if setup is needed — pg_eddy doesn't support Cypher CREATE yet.
        if ($text =~ /\bhaving executed\b/i) {
            return ('skip', 'requires data setup (Cypher CREATE not yet supported)');
        }
        if ($kw eq 'Given' && $text =~ /\bany graph\b/i) {
            return ('skip', 'requires any graph (setup unknown)');
        }

        # Collect the test query.
        if ($kw eq 'When' && $text =~ /executing query/i && $doc) {
            $test_query = $doc;
        }

        # Skip if expecting an error type we can't validate.
        if ($text =~ /\b(ParameterMissing|ProcedureNotFound|UnknownFunction|InvalidArgumentValue)\b/i) {
            return ('skip', "error scenario not verifiable for $1");
        }
    }

    # Skip if query uses unsupported features.
    if ($test_query) {
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

    eval { $node->safe_psql('postgres', 'BEGIN') };
    return ('fail', "BEGIN failed: $@") if $@;

    my ($test_query, @expect_steps, $expects_error, $expected_err_type, $ordered);

    for my $step (@{$sc->{steps}}) {
        my ($kw, $text, $doc) = ($step->{kw}, $step->{text} // '', $step->{docstring} // '');

        if ($kw eq 'When' && $text =~ /executing query/i && $doc) {
            ($test_query = $doc) =~ s/^\s+|\s+$//g;
        }
        if ($text =~ /(\w*Error) should be raised/i) {
            $expects_error     = 1;
            $expected_err_type = $1;
        }
        if ($text =~ /the result should be,?\s*(in any order|in order)/i) {
            $ordered = ($1 =~ /in order/i) ? 1 : 0;
            push @expect_steps, $step;
        }
        if ($text =~ /an empty result|no results/i && $kw =~ /Then|And/) {
            push @expect_steps, { kw => 'empty' };
        }
    }

    unless ($test_query) {
        eval { $node->safe_psql('postgres', 'ROLLBACK') };
        return ('skip', 'no test query');
    }

    my $escaped = $test_query;
    $escaped =~ s/'/''/g;
    my $sql = "SELECT row_to_json(r)::text FROM (SELECT * FROM cypher('$escaped', NULL::jsonb)) AS r";

    my ($stdout, $stderr) = $node->psql('postgres', $sql);
    eval { $node->safe_psql('postgres', 'ROLLBACK') };

    if ($expects_error) {
        return ($stderr && $stderr =~ /ERROR/ ? 'pass' : 'fail',
                $stderr && $stderr =~ /ERROR/ ? '' : "expected $expected_err_type but query succeeded");
    }

    if ($stderr && $stderr =~ /ERROR/) {
        return ('fail', "query failed: " . ($stderr =~ /ERROR:\s*(.+)/)[0]);
    }

    my @actual = parse_jsonb_rows($stdout // '');

    for my $es (@expect_steps) {
        if (ref $es eq 'HASH' && ($es->{kw} // '') eq 'empty') {
            return ('fail', "expected empty result but got " . scalar(@actual) . " rows") if @actual;
            next;
        }
        next unless $es->{table} && @{$es->{table}};
        my @tbl  = @{$es->{table}};
        my @hdrs = @{$tbl[0]};
        my @exps = @tbl[1..$#tbl];
        my $err  = compare_results(\@exps, \@actual, \@hdrs, $ordered // 0);
        return ('fail', $err) if $err;
    }

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
    if ($exp =~ /^\[/) {
        return undef if ref($act) eq 'HASH' && edge_display_matches($exp, $act);
        return "edge mismatch (expected $exp)";
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

sub _repr { defined $_[0] ? (ref($_[0]) ? encode_json($_[0]) : $_[0]) : 'undef' }

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

    my $flush_step = sub {
        return unless $sc && $step;
        $step->{table} = [@tbl_rows] if $in_tbl;
        push @{$sc->{steps}}, $step;
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
        elsif ($line =~ /^\s*(Scenario Outline|Scenario):\s*(.*)/) {
            $flush_sc->();
            $sc = { label => "$feature_name — $2", file => $file,
                    is_outline => ($1 eq 'Scenario Outline'), steps => [] };
        }
        elsif ($sc && $line =~ /^\s*Examples:/)                 { $flush_step->(); $in_ex = 1; }
        elsif ($sc && $line =~ /^\s*(Given|When|Then|And|But)\s+(.*)/) {
            $flush_step->(); $step = { kw => $1, text => $2 };
        }
        elsif ($sc && $step && $line =~ /^\s*"""/)              { $in_doc = 1; $doc_buf = ''; }
        elsif ($sc && $step && $line =~ /^\s*\|/)               { push @tbl_rows, [_split_row($line)]; $in_tbl = 1; }
    }
    $flush_sc->();
    return @scenarios;
}

sub _subst { my ($t, $b) = @_; return $t unless defined $t; $t =~ s/<([^>]+)>/defined($b->{$1}) ? $b->{$1} : "<$1>"/ge; $t }
sub _split_row { my ($l) = @_; $l =~ s/^\s*\|\s*//; $l =~ s/\s*\|\s*$//; map { my $c=$_; $c=~s/^\s+|\s+$//g; $c } split /\s*\|\s*/, $l }
