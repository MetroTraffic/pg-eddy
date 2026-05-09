# Security Policy

## Reporting Security Vulnerabilities

If you discover a security vulnerability in pg_eddy, please **do not** open a public GitHub issue. Instead, please report it responsibly to the maintainers.

### Contact

Please send security vulnerability reports to the project maintainers via [GitHub's private vulnerability reporting feature](https://github.com/trickle-labs/pg-eddy/security/advisories) or by opening a private security advisory.

Include:
- A description of the vulnerability
- Steps to reproduce (if possible)
- Potential impact
- Suggested fix (if you have one)

### Response Timeline

We will:
1. Acknowledge your report within 48 hours
2. Assess the severity and impact
3. Develop and test a fix
4. Release a patched version as soon as practical
5. Credit you in the security advisory (unless you prefer anonymity)

## Security Considerations

### Current Phase

pg_eddy is in active development (Phase 0–Phase 1 released). The focus is on **correctness and safety**, not production deployment. Please do not run pg_eddy on production systems handling sensitive data until v1.0.0 is released.

### Known Limitations

- **WAL Resource Manager ID 128** is a temporary development ID. A permanent ID will be assigned before any production release.
- **Crash recovery** is implemented but not extensively stress-tested across failure scenarios.
- **Authentication & authorization** inherit from PostgreSQL; no additional access control is implemented at the extension level.

## Future Security Work

- Formal security audit before v1.0.0
- Compliance with PostgreSQL security advisories process
- Regular dependency updates and vulnerability scanning
