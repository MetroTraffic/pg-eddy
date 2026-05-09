use thiserror::Error;

/// Canonical error type for pg_eddy.  All `PE###` codes are stable across versions.
#[derive(Debug, Error)]
pub enum PgEddyError {
    // PE100 range — storage errors
    #[error("PE100: node not found: {0}")]
    NodeNotFound(i64),

    #[error("PE101: edge not found: {0}")]
    EdgeNotFound(i64),

    // PE200 range — catalog errors
    #[error("PE200: unknown label: {0}")]
    UnknownLabel(String),

    #[error("PE201: unknown relationship type: {0}")]
    UnknownRelType(String),

    // PE300 range — query engine errors
    #[error("PE300: parse error: {0}")]
    ParseError(String),

    #[error("PE320: traversal memory budget exceeded (frontier size: {0})")]
    TraversalMemoryExceeded(usize),

    // PE900 range — internal / unexpected
    #[error("PE900: internal error: {0}")]
    Internal(String),
}
