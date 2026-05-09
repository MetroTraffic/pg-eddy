-- pg_eddy--0.3.0.sql
-- Phase 2: Edge Storage + Adjacency Lists.
--
-- Extends Phase 1 with:
--   • edge_id_seq sequence (backing unique edge ids)
--   • create_edge(), get_edge(), delete_edge(), edge_count() SQL functions
--   • neighbours(), expand() adjacency-follow SRFs
--
-- shared_preload_libraries = 'pg_eddy'  must be set in postgresql.conf.

-- ---------------------------------------------------------------------------
-- Internal schema
-- ---------------------------------------------------------------------------
CREATE SCHEMA IF NOT EXISTS _pg_eddy;

-- ---------------------------------------------------------------------------
-- Registry tables
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.label_registry (
    label_id  INT  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE
);

CREATE TABLE _pg_eddy.rel_type_registry (
    type_id   INT  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE
);

CREATE TABLE _pg_eddy.property_key_registry (
    key_id    INT  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE
);

-- ---------------------------------------------------------------------------
-- Sequences
-- ---------------------------------------------------------------------------
CREATE SEQUENCE _pg_eddy.node_id_seq
    START WITH 1
    INCREMENT BY 1
    NO CYCLE;

CREATE SEQUENCE _pg_eddy.edge_id_seq
    START WITH 1
    INCREMENT BY 1
    NO CYCLE;

-- ---------------------------------------------------------------------------
-- AM handler functions
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
-- Backing storage tables (custom AMs, no SQL columns)
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.nodes ()
    USING pg_eddy_node;

CREATE TABLE _pg_eddy.edges ()
    USING pg_eddy_edge;

-- ---------------------------------------------------------------------------
-- Public views (user-facing aliases in the install schema)
-- ---------------------------------------------------------------------------
CREATE VIEW nodes AS SELECT * FROM _pg_eddy.nodes;
CREATE VIEW edges AS SELECT * FROM _pg_eddy.edges;
