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
