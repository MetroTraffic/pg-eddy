use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;

use crate::error::PgEddyError;
use crate::ivm::mirror::{PG_TRICKLE_REPOSITORY, PG_TRICKLE_REVISION, PG_TRICKLE_VERSION};

#[derive(Debug, Clone)]
pub struct DependencyInfo {
    pub installed_version: String,
}

pub fn check_compatibility() -> Result<DependencyInfo, PgEddyError> {
    let installed_version = Spi::get_one::<String>(
        "SELECT extversion FROM pg_catalog.pg_extension WHERE extname = 'pg_trickle'",
    )
    .map_err(|error| dependency_error(format!("extension lookup failed: {error}")))?
    .ok_or_else(|| dependency_error("extension pg_trickle is not installed"))?;
    if installed_version != PG_TRICKLE_VERSION {
        return Err(dependency_error(format!(
            "installed version {installed_version}, expected {PG_TRICKLE_VERSION} \
             from {PG_TRICKLE_REPOSITORY}@{PG_TRICKLE_REVISION}",
        )));
    }

    let api_ok = Spi::get_one::<bool>(
        "SELECT \
           to_regprocedure('pgtrickle.create_stream_table(text,text,text,text,boolean,text,text,text,boolean,boolean,text,integer,double precision,text,boolean,text,integer)') IS NOT NULL \
           AND to_regprocedure('pgtrickle.drop_stream_table(text,boolean)') IS NOT NULL \
           AND to_regprocedure('pgtrickle.refresh_stream_table(text)') IS NOT NULL \
           AND to_regprocedure('pgtrickle.poll_pg_eddy_connector()') IS NOT NULL \
           AND to_regclass('pgtrickle.pgt_cdc_connector_status') IS NOT NULL \
           AND to_regclass('pgtrickle.pg_stat_stream_tables') IS NOT NULL",
    )
    .map_err(|error| dependency_error(format!("API lookup failed: {error}")))?
    .unwrap_or(false);
    if !api_ok {
        return Err(dependency_error(
            "installed extension does not expose the pinned lifecycle API",
        ));
    }
    Ok(DependencyInfo { installed_version })
}

pub fn create_stream_table(
    name: &str,
    query: &str,
    schedule: &str,
    refresh_mode: &str,
    cdc_mode: &str,
) -> Result<i64, PgEddyError> {
    check_compatibility()?;
    Spi::run_with_args(
        "SELECT pgtrickle.create_stream_table( \
             name => $1, query => $2, schedule => $3, refresh_mode => $4, \
               cdc_mode => $5 \
         )",
        &[
            DatumWithOid::from(name),
            DatumWithOid::from(query),
            DatumWithOid::from(schedule),
            DatumWithOid::from(refresh_mode),
            DatumWithOid::from(cdc_mode),
        ],
    )
    .map_err(|error| PgEddyError::PgTrickleOperation(error.to_string()))?;

    Spi::get_one_with_args::<i64>(
        "SELECT to_regclass($1)::oid::bigint",
        &[DatumWithOid::from(name)],
    )
    .map_err(|error| PgEddyError::PgTrickleOperation(error.to_string()))?
    .ok_or_else(|| {
        PgEddyError::PgTrickleOperation(format!("stream table `{name}` was not created",))
    })
}

pub fn verify_cdc_mode_request(
    stream_table_oid: i64,
    expected_mode: &str,
) -> Result<(), PgEddyError> {
    let verified = Spi::get_one_with_args::<bool>(
        "SELECT lower(st.requested_cdc_mode) = lower($2) \
           AND (lower($2) <> 'trigger' OR NOT EXISTS ( \
                   SELECT 1 FROM pgtrickle.pgt_dependencies d \
                   WHERE d.pgt_id = st.pgt_id \
                     AND d.source_type = 'TABLE' \
                     AND d.cdc_mode <> 'TRIGGER' \
               )) \
         FROM pgtrickle.pgt_stream_tables st \
         WHERE st.pgt_relid = $1::oid",
        &[
            DatumWithOid::from(stream_table_oid),
            DatumWithOid::from(expected_mode),
        ],
    )
    .map_err(|error| PgEddyError::PgTrickleOperation(error.to_string()))?
    .unwrap_or(false);
    if !verified {
        return Err(PgEddyError::PgTrickleOperation(format!(
            "pg_trickle did not preserve requested CDC mode `{expected_mode}`"
        )));
    }
    Ok(())
}

pub fn drop_stream_table(name: &str) -> Result<(), PgEddyError> {
    check_compatibility()?;
    Spi::run_with_args(
        "SELECT pgtrickle.drop_stream_table($1, false)",
        &[DatumWithOid::from(name)],
    )
    .map_err(|error| PgEddyError::PgTrickleOperation(error.to_string()))
}

pub fn refresh_stream_table(name: &str) -> Result<(), PgEddyError> {
    check_compatibility()?;
    Spi::run_with_args(
        "SELECT pgtrickle.refresh_stream_table($1)",
        &[DatumWithOid::from(name)],
    )
    .map_err(|error| PgEddyError::PgTrickleOperation(error.to_string()))
}

pub fn installed_version() -> Option<String> {
    Spi::get_one::<String>(
        "SELECT extversion FROM pg_catalog.pg_extension WHERE extname = 'pg_trickle'",
    )
    .unwrap_or(None)
}

fn dependency_error(message: impl Into<String>) -> PgEddyError {
    PgEddyError::PgTrickleUnavailable(message.into())
}
