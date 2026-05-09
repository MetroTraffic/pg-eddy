-- pg_eddy--0.1.0.sql
-- Phase 0: AM skeleton, internal schema, and registry tables.
--
-- This file is appended to the pgrx-generated schema SQL and executed by
-- CREATE EXTENSION pg_eddy.
-- shared_preload_libraries = 'pg_eddy'  must be set in postgresql.conf.
--
-- Note: pg_eddy does NOT declare a `schema =` in its control file because
-- PostgreSQL 18 rejects schema names beginning with "pg_" (reserved for
-- system use).  Functions and tables are installed in whatever schema the
-- user specifies: CREATE EXTENSION pg_eddy SCHEMA myschema;  or in the
-- first writable schema of search_path (default: public).

-- ---------------------------------------------------------------------------
-- Internal schema (not user-facing, not in schema-only pg_dump)
-- _pg_eddy is valid: underscore prefix is not reserved.
-- ---------------------------------------------------------------------------
CREATE SCHEMA IF NOT EXISTS _pg_eddy;

-- ---------------------------------------------------------------------------
-- Registry tables (standard heap — small, always warmed in shared_buffers)
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.label_registry (
    label_id  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT   NOT NULL UNIQUE
);

CREATE TABLE _pg_eddy.rel_type_registry (
    type_id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT   NOT NULL UNIQUE
);

CREATE TABLE _pg_eddy.property_key_registry (
    key_id    BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT   NOT NULL UNIQUE
);

-- ---------------------------------------------------------------------------
-- AM handler functions (no schema qualifier — go to the extension schema)
-- ---------------------------------------------------------------------------
CREATE FUNCTION pg_eddy_node_handler(internal)
    RETURNS table_am_handler
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'pg_eddy_node_handler';

CREATE FUNCTION pg_eddy_edge_handler(internal)
    RETURNS table_am_handler
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'pg_eddy_edge_handler';

-- ---------------------------------------------------------------------------
-- Access Methods
-- ---------------------------------------------------------------------------
CREATE ACCESS METHOD pg_eddy_node
    TYPE TABLE
    HANDLER pg_eddy_node_handler;

CREATE ACCESS METHOD pg_eddy_edge
    TYPE TABLE
    HANDLER pg_eddy_edge_handler;

-- ---------------------------------------------------------------------------
-- Backing storage tables (custom AMs; no columns — Phase 1 adds layout)
-- ---------------------------------------------------------------------------
CREATE TABLE nodes ()
    USING pg_eddy_node;

CREATE TABLE edges ()
    USING pg_eddy_edge;
