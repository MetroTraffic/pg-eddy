/// Label and property-key registry SPI helpers.
///
/// All functions require an active transaction with SPI available.

use pgrx::prelude::*;
use pgrx::datum::DatumWithOid;

/// Look up or insert a label by name, returning its `label_id`.
///
/// Uses an UPSERT so it's idempotent and returns the id in all cases.
pub fn ensure_label(name: &str) -> i32 {
    Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.label_registry(name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
         RETURNING label_id",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or_else(|e| panic!("pg_eddy: ensure_label SPI error: {e}"))
    .unwrap_or_else(|| panic!("pg_eddy: ensure_label returned NULL for '{name}'"))
}

/// Return the name of a label by its id, or `"?"` if not found.
pub fn label_name(id: i32) -> String {
    Spi::get_one_with_args::<String>(
        "SELECT name FROM _pg_eddy.label_registry WHERE label_id = $1",
        &[DatumWithOid::from(id)],
    )
    .unwrap_or(None)
    .unwrap_or_else(|| format!("?{id}"))
}

/// Look up or insert a property key by name, returning its `key_id`.
pub fn ensure_prop_key(name: &str) -> i32 {
    Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.property_key_registry(name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
         RETURNING key_id",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or_else(|e| panic!("pg_eddy: ensure_prop_key SPI error: {e}"))
    .unwrap_or_else(|| panic!("pg_eddy: ensure_prop_key returned NULL for '{name}'"))
}

/// Return the name of a property key by its id, or `"?"` if not found.
pub fn prop_key_name(id: i32) -> String {
    Spi::get_one_with_args::<String>(
        "SELECT name FROM _pg_eddy.property_key_registry WHERE key_id = $1",
        &[DatumWithOid::from(id)],
    )
    .unwrap_or(None)
    .unwrap_or_else(|| format!("?{id}"))
}

/// Allocate the next node id from `_pg_eddy.node_id_seq`.
pub fn next_node_id() -> i64 {
    Spi::get_one::<i64>("SELECT nextval('_pg_eddy.node_id_seq')")
        .unwrap_or_else(|e| panic!("pg_eddy: next_node_id SPI error: {e}"))
        .unwrap_or_else(|| panic!("pg_eddy: nextval returned NULL"))
}
