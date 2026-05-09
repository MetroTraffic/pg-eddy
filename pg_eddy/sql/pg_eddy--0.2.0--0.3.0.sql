-- pg_eddy--0.2.0--0.3.0.sql
-- Migration from v0.2.0 to v0.3.0 (Phase 2: Edge Storage).
--
-- Adds the edge_id_seq sequence used by create_edge().
-- The rel_type_registry and property_key_registry tables already exist from
-- v0.2.0 and require no schema changes.

-- ---------------------------------------------------------------------------
-- New: edge id sequence
-- ---------------------------------------------------------------------------
CREATE SEQUENCE _pg_eddy.edge_id_seq
    START WITH 1
    INCREMENT BY 1
    NO CYCLE;
