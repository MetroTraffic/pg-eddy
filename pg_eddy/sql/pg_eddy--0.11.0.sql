-- pg_eddy--0.11.0.sql
-- v0.25.0: Property indexes + Schema DDL + CALL db.* procedures.
--
-- Full DDL: all tables, indexes, sequences, AM objects, and views.
-- New in v0.11.0 (v0.25.0 release):
--   _pg_eddy.prop_index_catalog   — tracks (label, prop) property index registrations
--   _pg_eddy.prop_value_index     — B-tree-indexed property value lookup table
--   pg_eddy.create_node_index     — register + backfill a property index
--   pg_eddy.drop_node_index       — remove a property index
--   pg_eddy.show_indexes          — list registered property indexes
--   Cypher DDL: CREATE INDEX ON :Label(prop) / DROP INDEX ON :Label(prop) / SHOW INDEXES
--   CALL db.labels() / db.relationshipTypes() / db.propertyKeys() (built-in procedures)
--   CALL dbms.components() (built-in procedure)
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
-- Property index catalog (v0.10.0) — tracks registered property indexes
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.prop_index_catalog (
    index_id    INT  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label_name  TEXT NOT NULL,
    prop_name   TEXT NOT NULL,
    UNIQUE (label_name, prop_name)
);

-- ---------------------------------------------------------------------------
-- Property value index (v0.10.0) — B-tree lookup: (label, prop, value) → node_id
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.prop_value_index (
    label_id    INT    NOT NULL,
    key_id      INT    NOT NULL,
    value_text  TEXT   NOT NULL,
    node_id     BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.prop_value_index USING btree (label_id, key_id, value_text);
CREATE INDEX ON _pg_eddy.prop_value_index USING btree (node_id);

-- ---------------------------------------------------------------------------
-- Node location index (v0.11.0) — shadow catalog for O(1) node-by-ID lookups
-- ---------------------------------------------------------------------------
-- Maps node_id → (page_num, offset_num) in the _pg_eddy.nodes heap.
-- Written on create_node(); bulk-loaded into a per-statement thread-local
-- HashMap at the start of each cypher() call for O(1) in-process lookups.
-- Eliminates the O(N) sequential scan in find_node_by_id / find_node_location.
CREATE TABLE _pg_eddy.node_location (
    node_id    BIGINT NOT NULL PRIMARY KEY,
    page_num   INT4   NOT NULL,
    offset_num INT4   NOT NULL
);

-- ---------------------------------------------------------------------------
-- Constraint catalog (v0.10.0) — tracks UNIQUE / EXISTS constraints
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.constraint_catalog (
    constraint_id  INT  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label_name     TEXT NOT NULL,
    prop_name      TEXT NOT NULL,
    kind           TEXT NOT NULL CHECK (kind IN ('UNIQUE', 'EXISTS')),
    UNIQUE (label_name, prop_name, kind)
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
