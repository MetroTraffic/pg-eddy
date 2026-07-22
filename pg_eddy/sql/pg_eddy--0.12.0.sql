-- pg_eddy--0.12.0.sql
-- Full DDL for typed trigger-CDC mirrors and graph-view metadata.

CREATE SCHEMA IF NOT EXISTS _pg_eddy;
CREATE SCHEMA IF NOT EXISTS _pg_eddy_views;
GRANT USAGE ON SCHEMA _pg_eddy_views TO PUBLIC;

CREATE TABLE _pg_eddy.label_registry (
    label_id  INT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE
);

CREATE TABLE _pg_eddy.rel_type_registry (
    type_id   INT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE
);

CREATE TABLE _pg_eddy.property_key_registry (
    key_id    INT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE
);

CREATE TABLE _pg_eddy.label_index (
    label_id  INT NOT NULL,
    node_id   BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.label_index USING btree (label_id, node_id);
CREATE INDEX ON _pg_eddy.label_index USING btree (node_id);

CREATE TABLE _pg_eddy.edge_type_src (
    type_id      INT NOT NULL,
    src_node_id  BIGINT NOT NULL,
    edge_id      BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.edge_type_src USING btree (type_id, src_node_id);
CREATE INDEX ON _pg_eddy.edge_type_src USING btree (edge_id);

CREATE TABLE _pg_eddy.edge_type_dst (
    type_id      INT NOT NULL,
    dst_node_id  BIGINT NOT NULL,
    edge_id      BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.edge_type_dst USING btree (type_id, dst_node_id);
CREATE INDEX ON _pg_eddy.edge_type_dst USING btree (edge_id);

CREATE TABLE _pg_eddy.prop_index_catalog (
    index_id    INT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label_name  TEXT NOT NULL,
    prop_name   TEXT NOT NULL,
    UNIQUE (label_name, prop_name)
);

CREATE TABLE _pg_eddy.prop_value_index (
    label_id    INT NOT NULL,
    key_id      INT NOT NULL,
    value_text  TEXT NOT NULL,
    node_id     BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.prop_value_index USING btree (label_id, key_id, value_text);
CREATE INDEX ON _pg_eddy.prop_value_index USING btree (node_id);

CREATE TABLE _pg_eddy.node_location (
    node_id    BIGINT NOT NULL PRIMARY KEY,
    page_num   INT4 NOT NULL,
    offset_num INT4 NOT NULL
);

CREATE TABLE _pg_eddy.constraint_catalog (
    constraint_id  INT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label_name     TEXT NOT NULL,
    prop_name      TEXT NOT NULL,
    kind           TEXT NOT NULL CHECK (kind IN ('UNIQUE', 'EXISTS')),
    UNIQUE (label_name, prop_name, kind)
);

CREATE SEQUENCE _pg_eddy.node_id_seq START WITH 1 INCREMENT BY 1 NO CYCLE;
CREATE SEQUENCE _pg_eddy.edge_id_seq START WITH 1 INCREMENT BY 1 NO CYCLE;

CREATE FUNCTION pg_eddy_node_handler(internal)
    RETURNS table_am_handler
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'pg_eddy_node_handler';

CREATE FUNCTION pg_eddy_edge_handler(internal)
    RETURNS table_am_handler
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'pg_eddy_edge_handler';

CREATE ACCESS METHOD pg_eddy_node
    TYPE TABLE
    HANDLER pg_eddy_node_handler;

CREATE ACCESS METHOD pg_eddy_edge
    TYPE TABLE
    HANDLER pg_eddy_edge_handler;

CREATE TABLE _pg_eddy.nodes () USING pg_eddy_node;
CREATE TABLE _pg_eddy.edges () USING pg_eddy_edge;

CREATE VIEW nodes AS SELECT * FROM _pg_eddy.nodes;
CREATE VIEW edges AS SELECT * FROM _pg_eddy.edges;

CREATE TABLE _pg_eddy.ivm_nodes (
    node_id     BIGINT PRIMARY KEY,
    labels      TEXT[] NOT NULL,
    properties  JSONB NOT NULL
);

CREATE INDEX ivm_nodes_labels_idx
    ON _pg_eddy.ivm_nodes USING gin (labels);
CREATE INDEX ivm_nodes_properties_idx
    ON _pg_eddy.ivm_nodes USING gin (properties jsonb_path_ops);

CREATE TABLE _pg_eddy.ivm_edges (
    rel_id          BIGINT PRIMARY KEY,
    rel_type        TEXT NOT NULL,
    source_node_id  BIGINT NOT NULL,
    target_node_id  BIGINT NOT NULL,
    properties      JSONB NOT NULL
);

CREATE INDEX ivm_edges_type_source_idx
    ON _pg_eddy.ivm_edges (rel_type, source_node_id);
CREATE INDEX ivm_edges_type_target_idx
    ON _pg_eddy.ivm_edges (rel_type, target_node_id);
CREATE INDEX ivm_edges_source_idx
    ON _pg_eddy.ivm_edges (source_node_id);
CREATE INDEX ivm_edges_target_idx
    ON _pg_eddy.ivm_edges (target_node_id);
CREATE INDEX ivm_edges_properties_idx
    ON _pg_eddy.ivm_edges USING gin (properties jsonb_path_ops);

ALTER TABLE _pg_eddy.ivm_nodes REPLICA IDENTITY DEFAULT;
ALTER TABLE _pg_eddy.ivm_edges REPLICA IDENTITY DEFAULT;
REVOKE INSERT, UPDATE, DELETE, TRUNCATE
    ON _pg_eddy.ivm_nodes, _pg_eddy.ivm_edges FROM PUBLIC;

CREATE TABLE _pg_eddy.graph_views (
    view_name             TEXT PRIMARY KEY,
    cypher_text           TEXT NOT NULL,
    params                JSONB NOT NULL DEFAULT '{}'::jsonb,
    compiled_sql          TEXT NOT NULL,
    schedule              TEXT NOT NULL DEFAULT '1s',
    refresh_mode          TEXT NOT NULL DEFAULT 'AUTO'
        CHECK (refresh_mode IN ('AUTO', 'FULL', 'DIFFERENTIAL', 'IMMEDIATE')),
    constraint_view       BOOLEAN NOT NULL DEFAULT FALSE,
    decode                BOOLEAN NOT NULL DEFAULT FALSE,
    stream_table_name     TEXT NOT NULL UNIQUE,
    stream_table_oid      OID,
    pg_trickle_version    TEXT NOT NULL,
    pg_trickle_revision   TEXT NOT NULL,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE FUNCTION _pg_eddy.check_graph_view_constraint()
    RETURNS TRIGGER
    LANGUAGE plpgsql
    SECURITY DEFINER
    SET search_path = pg_catalog, _pg_eddy_views
AS $$
DECLARE
    violated BOOLEAN;
BEGIN
    EXECUTE format(
        'SELECT EXISTS (SELECT 1 FROM _pg_eddy_views.%I)',
        TG_ARGV[0]
    ) INTO violated;
    IF violated THEN
        RAISE EXCEPTION 'PE607: graph constraint view violation: %', TG_ARGV[0]
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;