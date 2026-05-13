-- pg_eddy--0.10.0--0.11.0.sql
-- v0.25.0: OPT-1 — node-ID location index.
--
-- Adds _pg_eddy.node_location to enable O(log N) → O(1) node-by-ID lookups
-- instead of the O(N) sequential scan previously used by find_node_by_id and
-- find_node_location.
--
-- After the table is created, all existing nodes are backfilled by scanning
-- the node heap once.  New nodes are written to this table at insert time.

-- ---------------------------------------------------------------------------
-- Shadow location catalog
-- ---------------------------------------------------------------------------
CREATE TABLE _pg_eddy.node_location (
    node_id    BIGINT NOT NULL PRIMARY KEY,
    page_num   INT4   NOT NULL,
    offset_num INT4   NOT NULL
);

-- ---------------------------------------------------------------------------
-- Backfill: scan existing nodes and populate the table.
-- rebuild_node_location_index() is a new Rust SQL function added in v0.25.0.
-- ---------------------------------------------------------------------------
SELECT pg_eddy.rebuild_node_location_index();
