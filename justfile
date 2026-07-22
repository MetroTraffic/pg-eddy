# pg-eddy justfile
# Usage: just <recipe>
# Requires: https://github.com/casey/just

set shell := ["bash", "-c"]

pg_version := "18"

# Verify the optional IVM dependency uses the approved MetroTraffic fork/revision.
verify-pg-trickle:
    ./scripts/verify_pg_trickle.sh

# Build the extension (debug)
dev:
    cargo build --features pg18

# Install the extension into the system PostgreSQL 18 (needed for TAP + TCK tests)
install:
    cd pg_eddy && cargo pgrx package --pg-config /usr/lib/postgresql/18/bin/pg_config
    sudo cp target/release/pg_eddy-pg18/usr/lib/postgresql/18/lib/pg_eddy.so \
        /usr/lib/postgresql/18/lib/pg_eddy.so
    sudo cp target/release/pg_eddy-pg18/usr/share/postgresql/18/extension/pg_eddy*.sql \
        /usr/share/postgresql/18/extension/
    sudo cp target/release/pg_eddy-pg18/usr/share/postgresql/18/extension/pg_eddy.control \
        /usr/share/postgresql/18/extension/

# Build the exact pg_trickle fork used by IVM integration tests.
package-pg-trickle: verify-pg-trickle
    cd vendor/pg-trickle && cargo pgrx package --pg-config /usr/lib/postgresql/18/bin/pg_config

# Run all pgrx tests against PG18
test:
    cargo pgrx test pg18

# Run TAP crash / MVCC / concurrency tests against system PostgreSQL 18
# Requires: TAP::Parser::SourceHandler::pgTAP (sudo cpanm TAP::Parser::SourceHandler::pgTAP)
# and postgresql-server-dev-18 for PostgreSQL::Test::Cluster.
tap:
    PG_REGRESS='/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress' \
    PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:${PERL5LIB}" \
    PATH="/usr/lib/postgresql/18/bin:${PATH}" \
    prove -v tests/tap/*.pl

# Measure incremental graph-view trigger overhead. Install a release build first.
ivm-bench:
    rm -rf tmp_check/t_run_ivm_write_benchmark_ivm_write_bench_data
    PG_REGRESS='/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress' \
    PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:${PERL5LIB}" \
    PATH="/usr/lib/postgresql/18/bin:${PATH}" \
    perl benchmarks/run_ivm_write_benchmark.pl

# Run the IVM drift soak. Defaults to 72 hours; override IVM_SOAK_SECONDS for smoke runs.
ivm-soak:
    rm -rf tmp_check/t_ivm_drift_ivm_drift_soak_data
    PG_REGRESS='/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress' \
    PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:${PERL5LIB}" \
    PATH="/usr/lib/postgresql/18/bin:${PATH}" \
    perl tests/soak/ivm_drift.pl

# Run the openCypher TCK harness (all groups)
tck:
    PG_REGRESS='/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress' \
    PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:${PERL5LIB}" \
    PATH="/usr/lib/postgresql/18/bin:${PATH}" \
    prove -v tests/tck/run_tck.pl

# Run a specific TCK clause group, e.g.: just tck-group match
tck-group group:
    TCK_GROUPS={{group}} \
    PG_REGRESS='/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress' \
    PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:${PERL5LIB}" \
    PATH="/usr/lib/postgresql/18/bin:${PATH}" \
    prove -v tests/tck/run_tck.pl

# Run clippy lints
lint:
    cargo clippy --features pg18 -- -D warnings

# Generate schema SQL (useful for inspecting what pgrx produces)
schema:
    cargo pgrx schema pg18

# Package the extension (.zip suitable for installation)
package:
    cargo pgrx package --pg-config /usr/lib/postgresql/18/bin/pg_config

# Start an interactive psql session with the extension loaded
run:
    cargo pgrx run pg18

# Clean build artifacts
clean:
    cargo clean
