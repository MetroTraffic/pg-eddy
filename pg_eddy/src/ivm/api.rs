use pgrx::datum::TimestampWithTimeZone;
use pgrx::prelude::*;

use crate::error::PgEddyError;
use crate::ivm::{catalog, mirror, pg_trickle};

#[pg_extern]
fn create_graph_view(
    name: &str,
    cypher: &str,
    params: default!(pgrx::JsonB, "'{}'::jsonb"),
    schedule: default!(&str, "'1s'"),
    refresh_mode: default!(&str, "'AUTO'"),
    decode: default!(bool, false),
    constraint: default!(bool, false),
) {
    if let Err(error) = create_graph_view_impl(
        name,
        cypher,
        &params,
        schedule,
        refresh_mode,
        decode,
        constraint,
    ) {
        pgrx::error!("{error}");
    }
}

#[allow(clippy::too_many_arguments)]
fn create_graph_view_impl(
    name: &str,
    cypher: &str,
    params: &pgrx::JsonB,
    schedule: &str,
    refresh_mode: &str,
    decode: bool,
    constraint: bool,
) -> Result<(), PgEddyError> {
    validate_name(name)?;
    if cypher.trim().is_empty() {
        return Err(PgEddyError::InvalidGraphView(
            "Cypher query is empty".into(),
        ));
    }
    let refresh_mode = normalize_refresh_mode(refresh_mode)?;
    if decode && refresh_mode == "IMMEDIATE" {
        return Err(PgEddyError::UnsupportedRefreshMode(
            "decode => true is asynchronous and cannot be used with IMMEDIATE refresh".into(),
        ));
    }
    if constraint && refresh_mode != "IMMEDIATE" {
        return Err(PgEddyError::UnsupportedRefreshMode(
            "constraint views require IMMEDIATE".into(),
        ));
    }
    if schedule.trim().is_empty() && refresh_mode != "IMMEDIATE" {
        return Err(PgEddyError::InvalidGraphView("schedule is empty".into()));
    }
    if catalog::exists(name)? {
        return Err(PgEddyError::GraphViewExists(name.into()));
    }

    let compiled = crate::cypher::sql::compile(cypher, params)?;
    let dependency = pg_trickle::check_compatibility()?;
    let stream_table_name = format!("_pg_eddy_views.__pgeddy_{name}");
    let cdc_mode = if decode { "pg_eddy_wal" } else { "trigger" };
    let stream_table_oid = pg_trickle::create_stream_table(
        &stream_table_name,
        &compiled.sql,
        schedule,
        refresh_mode,
        cdc_mode,
    )?;
    pg_trickle::verify_cdc_mode_request(stream_table_oid, cdc_mode)?;
    create_projection_view(name, &stream_table_name, &compiled.columns)?;
    if constraint {
        ensure_constraint_is_satisfied(name)?;
        install_constraint_triggers(name)?;
    }
    catalog::insert(
        name,
        cypher,
        params,
        &compiled.sql,
        schedule,
        refresh_mode,
        constraint,
        decode,
        &stream_table_name,
        stream_table_oid,
        &dependency.installed_version,
        mirror::PG_TRICKLE_REVISION,
    )
}

#[pg_extern]
fn drop_graph_view(name: &str) {
    if let Err(error) = drop_graph_view_impl(name) {
        pgrx::error!("{error}");
    }
}

fn drop_graph_view_impl(name: &str) -> Result<(), PgEddyError> {
    let stream_table_name = catalog::stream_table_name(name)?;
    drop_constraint_triggers(name)?;
    Spi::run(&format!(
        "DROP VIEW IF EXISTS _pg_eddy_views.{}",
        crate::cypher::sql::quote_identifier(name),
    ))
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?;
    pg_trickle::drop_stream_table(&stream_table_name)?;
    catalog::delete(name)
}

#[pg_extern]
fn refresh_graph_view(name: &str) {
    if let Err(error) = refresh_graph_view_impl(name) {
        pgrx::error!("{error}");
    }
}

fn refresh_graph_view_impl(name: &str) -> Result<(), PgEddyError> {
    let stream_table_name = catalog::stream_table_name(name)?;
    pg_trickle::refresh_stream_table(&stream_table_name)
}

#[pg_extern]
#[allow(clippy::type_complexity)]
fn list_graph_views() -> TableIterator<
    'static,
    (
        name!(name, String),
        name!(cypher, String),
        name!(params, pgrx::JsonB),
        name!(schedule, String),
        name!(refresh_mode, String),
        name!(constraint_view, bool),
        name!(decode, bool),
        name!(stream_table_name, String),
        name!(stream_table_oid, Option<i64>),
        name!(status, Option<String>),
        name!(is_populated, Option<bool>),
        name!(created_at, TimestampWithTimeZone),
    ),
> {
    let rows = catalog::list().unwrap_or_else(|error| pgrx::error!("{error}"));
    TableIterator::new(rows.into_iter().map(|record| {
        (
            record.view_name,
            record.cypher_text,
            record.params,
            record.schedule,
            record.refresh_mode,
            record.constraint_view,
            record.decode,
            record.stream_table_name,
            record.stream_table_oid,
            record.status,
            record.is_populated,
            record.created_at,
        )
    }))
}

#[pg_extern]
fn graph_view_dependency_info() -> pgrx::JsonB {
    pgrx::JsonB(serde_json::json!({
        "repository": mirror::PG_TRICKLE_REPOSITORY,
        "revision": mirror::PG_TRICKLE_REVISION,
        "expected_version": mirror::PG_TRICKLE_VERSION,
        "installed_version": pg_trickle::installed_version(),
    }))
}

fn validate_name(name: &str) -> Result<(), PgEddyError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(PgEddyError::InvalidGraphView("view name is empty".into()));
    };
    if name.len() > 48
        || !(first == '_' || first.is_ascii_lowercase())
        || !chars.all(|character| {
            character == '_' || character.is_ascii_lowercase() || character.is_ascii_digit()
        })
    {
        return Err(PgEddyError::InvalidGraphView(
            "view name must be a lowercase SQL identifier up to 48 bytes".into(),
        ));
    }
    Ok(())
}

fn normalize_refresh_mode(mode: &str) -> Result<&'static str, PgEddyError> {
    match mode.to_ascii_uppercase().as_str() {
        "AUTO" => Ok("AUTO"),
        "FULL" => Ok("FULL"),
        "DIFFERENTIAL" => Ok("DIFFERENTIAL"),
        "IMMEDIATE" => Ok("IMMEDIATE"),
        _ => Err(PgEddyError::UnsupportedRefreshMode(mode.into())),
    }
}

fn create_projection_view(
    name: &str,
    stream_table_name: &str,
    columns: &[String],
) -> Result<(), PgEddyError> {
    let projections = columns
        .iter()
        .map(|column| {
            let quoted = crate::cypher::sql::quote_identifier(column);
            format!("{quoted}::jsonb AS {quoted}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let view_name = crate::cypher::sql::quote_identifier(name);
    Spi::run(&format!(
        "CREATE VIEW _pg_eddy_views.{view_name} AS \
         SELECT {projections} FROM {stream_table_name}",
    ))
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?;
    Spi::run(&format!(
        "GRANT SELECT ON _pg_eddy_views.{view_name} TO PUBLIC",
    ))
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))
}

fn ensure_constraint_is_satisfied(name: &str) -> Result<(), PgEddyError> {
    let view_name = crate::cypher::sql::quote_identifier(name);
    let violated = Spi::get_one::<bool>(&format!(
        "SELECT EXISTS (SELECT 1 FROM _pg_eddy_views.{view_name})",
    ))
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?
    .unwrap_or(false);
    if violated {
        return Err(PgEddyError::GraphConstraintViolation(name.into()));
    }
    Ok(())
}

fn install_constraint_triggers(name: &str) -> Result<(), PgEddyError> {
    let node_trigger = crate::cypher::sql::quote_identifier(&format!("pgeddy_gvc_n_{name}"));
    let edge_trigger = crate::cypher::sql::quote_identifier(&format!("pgeddy_gvc_e_{name}"));
    let view_name = name.replace('\'', "''");
    Spi::run(&format!(
        "CREATE CONSTRAINT TRIGGER {node_trigger} \
         AFTER INSERT OR UPDATE OR DELETE ON _pg_eddy.ivm_nodes \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
         EXECUTE FUNCTION _pg_eddy.check_graph_view_constraint('{view_name}')",
    ))
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?;
    Spi::run(&format!(
        "CREATE CONSTRAINT TRIGGER {edge_trigger} \
         AFTER INSERT OR UPDATE OR DELETE ON _pg_eddy.ivm_edges \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
         EXECUTE FUNCTION _pg_eddy.check_graph_view_constraint('{view_name}')",
    ))
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))
}

fn drop_constraint_triggers(name: &str) -> Result<(), PgEddyError> {
    let node_trigger = crate::cypher::sql::quote_identifier(&format!("pgeddy_gvc_n_{name}"));
    let edge_trigger = crate::cypher::sql::quote_identifier(&format!("pgeddy_gvc_e_{name}"));
    Spi::run(&format!(
        "DROP TRIGGER IF EXISTS {node_trigger} ON _pg_eddy.ivm_nodes",
    ))
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?;
    Spi::run(&format!(
        "DROP TRIGGER IF EXISTS {edge_trigger} ON _pg_eddy.ivm_edges",
    ))
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use super::*;

    #[pg_test]
    fn test_graph_view_dependency_is_optional() {
        let error = pg_trickle::check_compatibility()
            .expect_err("pg_trickle should not be installed in the base test cluster");
        assert!(error.to_string().starts_with("PE600:"));
    }

    #[pg_test]
    fn test_list_graph_views_without_dependency() {
        assert_eq!(catalog::list().expect("catalog list should work").len(), 0);
    }
}
