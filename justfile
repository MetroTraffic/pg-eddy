# pg-eddy justfile
# Usage: just <recipe>
# Requires: https://github.com/casey/just

pg_version := "18"

# Build the extension (debug)
dev:
    cargo build --features pg18

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
