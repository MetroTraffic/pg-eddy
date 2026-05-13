/// Constraint catalog — Rust interface for `_pg_eddy.constraint_catalog`.
///
/// Supports two constraint kinds:
/// - `UNIQUE` — enforce no two nodes with the same label have the same
///   value for the given property
/// - `EXISTS` — enforce every node with this label has the given property
///   (not null)
///
/// All public functions must be called inside an active transaction.
use pgrx::prelude::*;
use pgrx::datum::DatumWithOid;

// ---------------------------------------------------------------------------
// Constraint kind
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintKind {
    Unique,
    Exists,
}

impl ConstraintKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConstraintKind::Unique => "UNIQUE",
            ConstraintKind::Exists => "EXISTS",
        }
    }
}

// ---------------------------------------------------------------------------
// Catalog queries
// ---------------------------------------------------------------------------

/// Return all registered constraints as `(label_name, prop_name, kind)` triples.
pub fn list_constraints() -> Vec<(String, String, String)> {
    Spi::connect(|client| {
        client
            .select(
                "SELECT label_name, prop_name, kind \
                 FROM _pg_eddy.constraint_catalog \
                 ORDER BY label_name, prop_name",
                None,
                &[],
            )
            .unwrap_or_else(|e| pgrx::error!("pg_eddy: list_constraints SPI: {e}"))
            .filter_map(|row| {
                let label = row.get::<String>(1).ok().flatten()?;
                let prop  = row.get::<String>(2).ok().flatten()?;
                let kind  = row.get::<String>(3).ok().flatten()?;
                Some((label, prop, kind))
            })
            .collect()
    })
}

/// Return `true` if a `UNIQUE` constraint is registered for `(label_name, prop_name)`.
pub fn has_unique_constraint(label_name: &str, prop_name: &str) -> bool {
    Spi::get_one_with_args::<i32>(
        "SELECT 1 FROM _pg_eddy.constraint_catalog \
         WHERE label_name = $1 AND prop_name = $2 AND kind = 'UNIQUE'",
        &[DatumWithOid::from(label_name), DatumWithOid::from(prop_name)],
    )
    .unwrap_or(None)
    .is_some()
}

// ---------------------------------------------------------------------------
// Constraint management
// ---------------------------------------------------------------------------

/// Register a constraint for `(label, prop)` of the given `kind`.
///
/// Idempotent: if the same (label, prop, kind) already exists, this is a
/// no-op.  Returns the `constraint_id` of the new or existing entry.
pub fn create_constraint(label_name: &str, prop_name: &str, kind: ConstraintKind) -> i32 {
    let kind_str = kind.as_str();

    // Upsert: identical (label, prop, kind) → do nothing, still return the id.
    let constraint_id: i32 = Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.constraint_catalog(label_name, prop_name, kind) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (label_name, prop_name, kind) DO UPDATE \
           SET label_name = EXCLUDED.label_name \
         RETURNING constraint_id",
        &[
            DatumWithOid::from(label_name),
            DatumWithOid::from(prop_name),
            DatumWithOid::from(kind_str),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("pg_eddy: create_constraint SPI: {e}"))
    .unwrap_or_else(|| pgrx::error!("pg_eddy: create_constraint returned NULL"));

    constraint_id
}

/// Remove a constraint for `(label, prop, kind)`.
///
/// Returns `true` if a constraint was found and removed, `false` otherwise.
pub fn drop_constraint(label_name: &str, prop_name: &str, kind: ConstraintKind) -> bool {
    let kind_str = kind.as_str();
    let deleted: Option<i32> = Spi::get_one_with_args::<i32>(
        "DELETE FROM _pg_eddy.constraint_catalog \
         WHERE label_name = $1 AND prop_name = $2 AND kind = $3 \
         RETURNING constraint_id",
        &[
            DatumWithOid::from(label_name),
            DatumWithOid::from(prop_name),
            DatumWithOid::from(kind_str),
        ],
    )
    .unwrap_or(None);
    deleted.is_some()
}

// ---------------------------------------------------------------------------
// Uniqueness enforcement (called from write path)
// ---------------------------------------------------------------------------

/// Check that inserting `(label_name, prop_name, value_text)` for `new_node_id`
/// does not violate any UNIQUE constraint.
///
/// `value_text` should be the JSON-serialised property value (same format
/// used by `prop_value_index`).
///
/// Panics with a constraint-violation message if the uniqueness check fails.
/// This is intentionally a hard error (analogous to a PostgreSQL UNIQUE
/// constraint violation) so the transaction is aborted.
pub fn enforce_unique_on_insert(
    label_name: &str,
    prop_name: &str,
    value_text: &str,
    new_node_id: i64,
) {
    // Look up the numeric ids for the label and prop key.
    let label_id = crate::catalog::labels::ensure_label(label_name);
    let key_id   = crate::catalog::labels::ensure_prop_key(prop_name);

    // Check the property value index for an existing entry.
    let existing: Option<i64> = Spi::get_one_with_args::<i64>(
        "SELECT node_id FROM _pg_eddy.prop_value_index \
         WHERE label_id = $1 AND key_id = $2 AND value_text = $3 \
           AND node_id <> $4 \
         LIMIT 1",
        &[
            DatumWithOid::from(label_id),
            DatumWithOid::from(key_id),
            DatumWithOid::from(value_text),
            DatumWithOid::from(new_node_id),
        ],
    )
    .unwrap_or(None);

    if let Some(existing_id) = existing {
        pgrx::error!(
            "ConstraintViolation: UNIQUE constraint on :{label_name}.{prop_name} violated — \
             node {existing_id} already has value {value_text}"
        );
    }
}
