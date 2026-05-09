/// Catalog module — label and property-key registry access via SPI.
///
/// Phase 1: all lookups go to the `_pg_eddy.label_registry` and
/// `_pg_eddy.property_key_registry` tables via SPI.  No in-backend cache yet
/// (Phase 3 will add a per-backend `HashMap` cache backed by syscache
/// invalidation).
///
/// All public functions **must be called inside a transaction** (SPI requires
/// an active transaction context).
pub mod labels;
