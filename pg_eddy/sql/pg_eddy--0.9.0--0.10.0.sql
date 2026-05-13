-- pg_eddy upgrade: 0.9.0 → 0.10.0
-- v0.23.0: Property indexes + Schema DDL + CALL db.* procedures
--
-- New catalog tables:
--   _pg_eddy.prop_index_catalog   — tracks (label, prop) pairs with a B-tree index
--   _pg_eddy.prop_value_index     — the index data: (label_id, key_id, value_text) → node_id

-- ---------------------------------------------------------------------------
-- Property index catalog — one row per CREATE INDEX ON :Label(prop)
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.prop_index_catalog (
    index_id    INT  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label_name  TEXT NOT NULL,
    prop_name   TEXT NOT NULL,
    UNIQUE (label_name, prop_name)
);

-- ---------------------------------------------------------------------------
-- Property value index — the actual index rows
--   label_id   : from label_registry
--   key_id     : from property_key_registry
--   value_text : JSON-serialised property value (e.g. "42", "\"Alice\"", "true")
--   node_id    : the node that has this (label, prop=value) combination
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.prop_value_index (
    label_id    INT    NOT NULL,
    key_id      INT    NOT NULL,
    value_text  TEXT   NOT NULL,
    node_id     BIGINT NOT NULL
);

-- Primary lookup: given (label, prop, value) → set of node_ids
CREATE INDEX ON _pg_eddy.prop_value_index USING btree (label_id, key_id, value_text);

-- Maintenance lookup: given node_id → all its index entries (for DELETE / UPDATE)
CREATE INDEX ON _pg_eddy.prop_value_index USING btree (node_id);
