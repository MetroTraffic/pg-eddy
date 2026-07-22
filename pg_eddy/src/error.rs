use thiserror::Error;

/// Canonical error type for pg_eddy.  All `PE###` codes are stable across versions.
#[derive(Debug, Error)]
#[allow(dead_code)]
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

    // PE600 range — IVM / pg_trickle integration errors
    #[error("PE600: pg_trickle dependency unavailable or incompatible: {0}")]
    PgTrickleUnavailable(String),

    #[error("PE601: invalid graph view definition: {0}")]
    InvalidGraphView(String),

    #[error("PE602: graph view already exists: {0}")]
    GraphViewExists(String),

    #[error("PE603: graph view not found: {0}")]
    GraphViewNotFound(String),

    #[error("PE604: unsupported graph view refresh mode: {0}")]
    UnsupportedRefreshMode(String),

    #[error("PE605: pg_trickle operation failed: {0}")]
    PgTrickleOperation(String),

    #[error("PE606: graph view catalog operation failed: {0}")]
    GraphViewCatalog(String),

    #[error("PE607: graph constraint view violation: {0}")]
    GraphConstraintViolation(String),

    // PE900 range — internal / unexpected
    #[error("PE900: internal error: {0}")]
    Internal(String),
}
