-- pg_eddy--0.6.0.sql
-- Phase 5 v0.6.0: Cypher parser, logical planner, and executor.
--
-- Full DDL: schemas, sequences, AM objects, registry tables, label_index,
-- edge_type_src, edge_type_dst, views.
-- New in v0.6.0:
--   cypher(query TEXT, params JSONB) RETURNS SETOF JSONB  — execute Cypher
--   cypher_explain(query TEXT) RETURNS TEXT               — show logical plan
--   (both registered by pgrx; no manual DDL needed here)
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
-- Label index (Phase 4) — fast lookup of nodes by label
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.label_index (
    label_id  INT    NOT NULL,
    node_id   BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.label_index USING btree (label_id, node_id);
CREATE INDEX ON _pg_eddy.label_index USING btree (node_id);

-- ---------------------------------------------------------------------------
-- Rel-type index tables (v0.5.1) — fast edge lookup by type
-- ---------------------------------------------------------------------------

-- Maps (type_id, src_node_id) → edge_id.
CREATE TABLE _pg_eddy.edge_type_src (
    type_id      INT    NOT NULL,
    src_node_id  BIGINT NOT NULL,
    edge_id      BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.edge_type_src USING btree (type_id, src_node_id);
CREATE INDEX ON _pg_eddy.edge_type_src USING btree (edge_id);

-- Maps (type_id, dst_node_id) → edge_id.
CREATE TABLE _pg_eddy.edge_type_dst (
    type_id      INT    NOT NULL,
    dst_node_id  BIGINT NOT NULL,
    edge_id      BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.edge_type_dst USING btree (type_id, dst_node_id);
CREATE INDEX ON _pg_eddy.edge_type_dst USING btree (edge_id);

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
