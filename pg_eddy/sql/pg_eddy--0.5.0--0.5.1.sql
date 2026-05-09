-- pg_eddy--0.5.0--0.5.1.sql
-- Migration from v0.5.0 to v0.5.1.
-- Adds rel-type catalog index tables for fast edge lookup by type and endpoint.

-- Edge type → source node index.
-- Maintained by create_edge (INSERT) and delete_edge (DELETE) via SPI.
-- Allows O(degree_of_type) lookups for "all edges of type T from node N".
CREATE TABLE _pg_eddy.edge_type_src (
    type_id      INT    NOT NULL,
    src_node_id  BIGINT NOT NULL,
    edge_id      BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.edge_type_src
    USING btree (type_id, src_node_id);

CREATE INDEX ON _pg_eddy.edge_type_src
    USING btree (edge_id);

-- Edge type → destination node index.
CREATE TABLE _pg_eddy.edge_type_dst (
    type_id      INT    NOT NULL,
    dst_node_id  BIGINT NOT NULL,
    edge_id      BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.edge_type_dst
    USING btree (type_id, dst_node_id);

CREATE INDEX ON _pg_eddy.edge_type_dst
    USING btree (edge_id);
