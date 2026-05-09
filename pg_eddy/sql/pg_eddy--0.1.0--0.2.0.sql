-- pg_eddy--0.1.0--0.2.0.sql
-- Migration from Phase 0 (v0.1.0) to Phase 1 (v0.2.0).
--
-- Changes:
--   1. Add node_id_seq sequence
--   2. Add property_key_registry (new in v0.2.0; v0.1.0 had it)
--   3. Rename public nodes/edges tables to _pg_eddy schema
--   4. Expose public views
--   5. Alter registry tables to use INT key_id / label_id (from BIGINT)

-- ---------------------------------------------------------------------------
-- 1. Node id sequence
-- ---------------------------------------------------------------------------
CREATE SEQUENCE IF NOT EXISTS _pg_eddy.node_id_seq
    START WITH 1
    INCREMENT BY 1
    NO CYCLE;

-- ---------------------------------------------------------------------------
-- 2. Ensure property_key_registry exists (idempotent)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS _pg_eddy.property_key_registry (
    key_id    INT  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE
);

-- ---------------------------------------------------------------------------
-- 3. Move storage tables from public schema to _pg_eddy schema
--    (DROP existing v0.1.0 tables and recreate under new schema)
-- ---------------------------------------------------------------------------
DROP TABLE IF EXISTS public.nodes;
DROP TABLE IF EXISTS public.edges;

CREATE TABLE IF NOT EXISTS _pg_eddy.nodes ()
    USING pg_eddy_node;

CREATE TABLE IF NOT EXISTS _pg_eddy.edges ()
    USING pg_eddy_edge;

-- ---------------------------------------------------------------------------
-- 4. Public views
-- ---------------------------------------------------------------------------
CREATE OR REPLACE VIEW nodes AS SELECT * FROM _pg_eddy.nodes;
CREATE OR REPLACE VIEW edges AS SELECT * FROM _pg_eddy.edges;
