-- pg_eddy--0.11.0--0.12.0.sql
-- Typed logical mirrors and graph-view metadata for trigger-based IVM.

CREATE SCHEMA IF NOT EXISTS _pg_eddy_views;
GRANT USAGE ON SCHEMA _pg_eddy_views TO PUBLIC;

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

CREATE FUNCTION rebuild_ivm_sources()
    RETURNS BIGINT
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'rebuild_ivm_sources_wrapper';

CREATE FUNCTION create_graph_view(
    name TEXT,
    cypher TEXT,
    params JSONB DEFAULT '{}'::jsonb,
    schedule TEXT DEFAULT '1s',
    refresh_mode TEXT DEFAULT 'AUTO',
    decode BOOLEAN DEFAULT FALSE,
    "constraint" BOOLEAN DEFAULT FALSE
)
    RETURNS VOID
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'create_graph_view_wrapper';

CREATE FUNCTION drop_graph_view(name TEXT)
    RETURNS VOID
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'drop_graph_view_wrapper';

CREATE FUNCTION refresh_graph_view(name TEXT)
    RETURNS VOID
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'refresh_graph_view_wrapper';

CREATE FUNCTION graph_view_dependency_info()
    RETURNS JSONB
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'graph_view_dependency_info_wrapper';

CREATE FUNCTION list_graph_views()
    RETURNS TABLE (
        name TEXT,
        cypher TEXT,
        params JSONB,
        schedule TEXT,
        refresh_mode TEXT,
        constraint_view BOOLEAN,
        decode BOOLEAN,
        stream_table_name TEXT,
        stream_table_oid BIGINT,
        status TEXT,
        is_populated BOOLEAN,
        created_at TIMESTAMPTZ
    )
    LANGUAGE C STRICT
    AS 'MODULE_PATHNAME', 'list_graph_views_wrapper';

SELECT rebuild_ivm_sources();