# pg-eddy justfile
# Usage: just <recipe>
# Requires: https://github.com/casey/just

pg_version := "18"

# Build the extension (debug)
dev:
    cargo build --features pg18

# Install the extension into the system PostgreSQL 18 (needed for TAP + TCK tests)
install:
    cargo pgrx package --pg-config /usr/lib/postgresql/18/bin/pg_config
    sudo cp target/release/pg_eddy-pg18/usr/lib/postgresql/18/lib/pg_eddy.so \
        /usr/lib/postgresql/18/lib/pg_eddy.so
    sudo cp pg_eddy/sql/pg_eddy--*.sql /usr/share/postgresql/18/extension/
    sudo cp pg_eddy/pg_eddy.control /usr/share/postgresql/18/extension/

# Run all pgrx tests against PG18
test:
    cargo pgrx test pg18

# Run TAP crash / MVCC / concurrency tests against system PostgreSQL 18
# Requires: TAP::Parser::SourceHandler::pgTAP (sudo cpanm TAP::Parser::SourceHandler::pgTAP)
# and postgresql-server-dev-18 for PostgreSQL::Test::Cluster.
tap:
    PG_REGRESS=/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress \
    PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:$PERL5LIB" \
    PATH="/usr/lib/postgresql/18/bin:$PATH" \
    prove -v tests/tap/*.pl

# Run the openCypher TCK harness (all groups)
tck:
    PG_REGRESS=/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress \
    PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:$PERL5LIB" \
    PATH="/usr/lib/postgresql/18/bin:$PATH" \
    prove -v tests/tck/run_tck.pl

# Run a specific TCK clause group, e.g.: just tck-group match
tck-group group:
    PG_REGRESS=/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress \
    PERL5LIB="/usr/lib/postgresql/18/lib/pgxs/src/test/perl:$PERL5LIB" \
    PATH="/usr/lib/postgresql/18/bin:$PATH" \
    TCK_GROUPS="{{group}}" \
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
