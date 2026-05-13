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

/// Look up or insert a relationship type by name, returning its `type_id`.
pub fn ensure_rel_type(name: &str) -> i32 {
    Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.rel_type_registry(name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
         RETURNING type_id",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or_else(|e| panic!("pg_eddy: ensure_rel_type SPI error: {e}"))
    .unwrap_or_else(|| panic!("pg_eddy: ensure_rel_type returned NULL for '{name}'"))
}

/// Return the name of a relationship type by its id, or `"?"` if not found.
pub fn rel_type_name(id: i32) -> String {
    Spi::get_one_with_args::<String>(
        "SELECT name FROM _pg_eddy.rel_type_registry WHERE type_id = $1",
        &[DatumWithOid::from(id)],
    )
    .unwrap_or(None)
    .unwrap_or_else(|| format!("?{id}"))
}

/// Allocate the next edge id from `_pg_eddy.edge_id_seq`.
pub fn next_edge_id() -> i64 {
    Spi::get_one::<i64>("SELECT nextval('_pg_eddy.edge_id_seq')")
        .unwrap_or_else(|e| panic!("pg_eddy: next_edge_id SPI error: {e}"))
        .unwrap_or_else(|| panic!("pg_eddy: nextval returned NULL"))
}

/// Look up a label id by name; returns `None` if the label does not exist yet.
pub fn label_id_by_name(name: &str) -> Option<i32> {
    Spi::get_one_with_args::<i32>(
        "SELECT label_id FROM _pg_eddy.label_registry WHERE name = $1",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or(None)
}

/// Look up a property key by name, returning its `key_id` if it exists.
pub fn prop_key_id_by_name(name: &str) -> Option<i32> {
    Spi::get_one_with_args::<i32>(
        "SELECT key_id FROM _pg_eddy.property_key_registry WHERE name = $1",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or(None)
}

/// Return all label names in the registry.
pub fn all_labels() -> Vec<String> {
    Spi::connect(|client| {
        let tup_table = client
            .select("SELECT name FROM _pg_eddy.label_registry ORDER BY label_id", None, &[])
            .unwrap_or_else(|e| panic!("pg_eddy: all_labels SPI error: {e}"));
        tup_table
            .filter_map(|row| row.get::<String>(1).ok().flatten())
            .collect()
    })
}

/// Return all relationship type names in the registry.
pub fn all_rel_types() -> Vec<String> {
    Spi::connect(|client| {
        let tup_table = client
            .select("SELECT name FROM _pg_eddy.rel_type_registry ORDER BY type_id", None, &[])
            .unwrap_or_else(|e| panic!("pg_eddy: all_rel_types SPI error: {e}"));
        tup_table
            .filter_map(|row| row.get::<String>(1).ok().flatten())
            .collect()
    })
}

/// Return all property key names in the registry.
pub fn all_prop_keys() -> Vec<String> {
    Spi::connect(|client| {
        let tup_table = client
            .select("SELECT name FROM _pg_eddy.property_key_registry ORDER BY key_id", None, &[])
            .unwrap_or_else(|e| panic!("pg_eddy: all_prop_keys SPI error: {e}"));
        tup_table
            .filter_map(|row| row.get::<String>(1).ok().flatten())
            .collect()
    })
}
