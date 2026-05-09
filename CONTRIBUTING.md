# Contributing to pg-eddy

Thank you for your interest in contributing! Please read this document before
opening issues or pull requests.

## Getting Started

1. Install Rust (stable, via rustup)
2. Install PostgreSQL 18 dev headers (see CI workflow for apt commands)
3. `cargo install cargo-pgrx --version 0.18.0 --locked`
4. `cargo pgrx init --pg18 /usr/lib/postgresql/18/bin/pg_config`
5. `cargo pgrx test pg18`  — all tests should pass

## Code Style

- Rust stable, Edition 2024
- `cargo clippy -- -D warnings` must pass with no errors
- `cargo fmt` enforced in CI
- `unsafe` only at FFI boundaries; document every `unsafe` block with a
  `// Safety:` comment explaining why it is sound

## Branching

- `main` — stable, tagged releases
- `develop` — integration branch; PRs target this branch
- Feature branches: `feat/<topic>` or `fix/<topic>`

## Commit Messages

Use [Conventional Commits](https://www.conventionalcommits.org/):
```
feat(storage): implement node page layout
fix(am): correct scan_begin memory context
docs(plan): update Phase 1 exit criteria
```

## Pull Request Process

1. Open a PR against `develop`
2. Ensure CI (clippy + pgrx tests) passes
3. Include a test for any new behaviour
4. Reference the implementation plan phase in the PR description

## License

All contributions are licensed under Apache 2.0.
