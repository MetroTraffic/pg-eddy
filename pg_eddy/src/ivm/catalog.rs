use pgrx::datum::{DatumWithOid, TimestampWithTimeZone};
use pgrx::prelude::*;

use crate::error::PgEddyError;

#[derive(Debug)]
pub struct GraphViewRecord {
    pub view_name: String,
    pub cypher_text: String,
    pub params: pgrx::JsonB,
    pub schedule: String,
    pub refresh_mode: String,
    pub constraint_view: bool,
    pub decode: bool,
    pub stream_table_name: String,
    pub stream_table_oid: Option<i64>,
    pub status: Option<String>,
    pub is_populated: Option<bool>,
    pub created_at: TimestampWithTimeZone,
}

#[allow(clippy::too_many_arguments)]
pub fn insert(
    name: &str,
    cypher: &str,
    params: &pgrx::JsonB,
    compiled_sql: &str,
    schedule: &str,
    refresh_mode: &str,
    constraint_view: bool,
    decode: bool,
    stream_table_name: &str,
    stream_table_oid: i64,
    pg_trickle_version: &str,
    pg_trickle_revision: &str,
) -> Result<(), PgEddyError> {
    Spi::run_with_args(
        "INSERT INTO _pg_eddy.graph_views( \
             view_name, cypher_text, params, compiled_sql, schedule, refresh_mode, \
             constraint_view, decode, stream_table_name, stream_table_oid, \
             pg_trickle_version, pg_trickle_revision \
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::oid, $11, $12)",
        &[
            DatumWithOid::from(name),
            DatumWithOid::from(cypher),
            DatumWithOid::from(pgrx::JsonB(params.0.clone())),
            DatumWithOid::from(compiled_sql),
            DatumWithOid::from(schedule),
            DatumWithOid::from(refresh_mode),
            DatumWithOid::from(constraint_view),
            DatumWithOid::from(decode),
            DatumWithOid::from(stream_table_name),
            DatumWithOid::from(stream_table_oid),
            DatumWithOid::from(pg_trickle_version),
            DatumWithOid::from(pg_trickle_revision),
        ],
    )
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))
}

pub fn exists(name: &str) -> Result<bool, PgEddyError> {
    Spi::get_one_with_args::<bool>(
        "SELECT EXISTS(SELECT 1 FROM _pg_eddy.graph_views WHERE view_name = $1)",
        &[DatumWithOid::from(name)],
    )
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))
    .map(|value| value.unwrap_or(false))
}

pub fn stream_table_name(name: &str) -> Result<String, PgEddyError> {
    Spi::get_one_with_args::<String>(
        "SELECT stream_table_name FROM _pg_eddy.graph_views WHERE view_name = $1",
        &[DatumWithOid::from(name)],
    )
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?
    .ok_or_else(|| PgEddyError::GraphViewNotFound(name.into()))
}

pub fn delete(name: &str) -> Result<(), PgEddyError> {
    Spi::run_with_args(
        "DELETE FROM _pg_eddy.graph_views WHERE view_name = $1",
        &[DatumWithOid::from(name)],
    )
    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))
}

pub fn list() -> Result<Vec<GraphViewRecord>, PgEddyError> {
    let pg_trickle_installed = crate::ivm::pg_trickle::installed_version().is_some();
    let sql = if pg_trickle_installed {
        "SELECT gv.view_name, gv.cypher_text, gv.params, \
            gv.schedule, gv.refresh_mode, gv.constraint_view, gv.decode, \
            gv.stream_table_name, gv.stream_table_oid::bigint, \
            st.status, st.is_populated, gv.created_at \
         FROM _pg_eddy.graph_views gv \
         LEFT JOIN pgtrickle.pgt_stream_tables st \
           ON st.pgt_relid = gv.stream_table_oid \
         ORDER BY gv.view_name"
    } else {
        "SELECT gv.view_name, gv.cypher_text, gv.params, \
            gv.schedule, gv.refresh_mode, gv.constraint_view, gv.decode, \
            gv.stream_table_name, gv.stream_table_oid::bigint, \
            NULL::text, NULL::boolean, gv.created_at \
         FROM _pg_eddy.graph_views gv ORDER BY gv.view_name"
    };

    Spi::connect(|client| {
        let table = client
            .select(sql, None, &[])
            .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?;
        let mut records = Vec::new();
        for row in table {
            records.push(GraphViewRecord {
                view_name: required(&row, 1, "view_name")?,
                cypher_text: required(&row, 2, "cypher_text")?,
                params: required(&row, 3, "params")?,
                schedule: required(&row, 4, "schedule")?,
                refresh_mode: required(&row, 5, "refresh_mode")?,
                constraint_view: required(&row, 6, "constraint_view")?,
                decode: required(&row, 7, "decode")?,
                stream_table_name: required(&row, 8, "stream_table_name")?,
                stream_table_oid: row
                    .get::<i64>(9)
                    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?,
                status: row
                    .get::<String>(10)
                    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?,
                is_populated: row
                    .get::<bool>(11)
                    .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?,
                created_at: required(&row, 12, "created_at")?,
            });
        }
        Ok(records)
    })
}

fn required<T: IntoDatum + FromDatum>(
    row: &pgrx::spi::SpiHeapTupleData<'_>,
    ordinal: usize,
    name: &str,
) -> Result<T, PgEddyError> {
    row.get::<T>(ordinal)
        .map_err(|error| PgEddyError::GraphViewCatalog(error.to_string()))?
        .ok_or_else(|| PgEddyError::GraphViewCatalog(format!("{name} is NULL")))
}
