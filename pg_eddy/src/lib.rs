// pg_eddy — Phase 0: AM skeleton + WAL RMGR skeleton
//
// This is the extension entry point.  At _PG_init we:
//   1. Register the custom WAL resource manager (no-op redo for now).
//   2. Nothing else for Phase 0; AM objects are created by the SQL script.
//
// shared_preload_libraries = 'pg_eddy'  is required from this version.

use pgrx::prelude::*;

mod error;
mod storage;

pgrx::pg_module_magic!();

// ---------------------------------------------------------------------------
// Extension SQL – schemas, registry tables, and AM objects.
// The file is loaded in order by pgrx during CREATE EXTENSION.
// ---------------------------------------------------------------------------
extension_sql_file!("../sql/pg_eddy--0.1.0.sql", name = "pg_eddy_schema", finalize);

// ---------------------------------------------------------------------------
// _PG_init  — runs at postmaster start (shared_preload_libraries)
// ---------------------------------------------------------------------------
#[allow(non_snake_case)]
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    // Phase 0: register the custom WAL resource manager.
    // Redo is a no-op; this proves the registration path works.
    storage::wal::register_rmgr();
}

// ---------------------------------------------------------------------------
// Basic health-check function (smoke-test CREATE EXTENSION worked)
// ---------------------------------------------------------------------------
#[pg_extern]
fn health_check() -> &'static str {
    "pg_eddy OK"
}

// ---------------------------------------------------------------------------
// pg_test module — pgrx unit tests
// ---------------------------------------------------------------------------
#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn test_health_check() {
        assert_eq!("pg_eddy OK", crate::health_check());
    }
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'pg_eddy'"]
    }
}
