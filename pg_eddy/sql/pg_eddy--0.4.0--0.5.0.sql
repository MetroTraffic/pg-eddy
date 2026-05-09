-- pg_eddy--0.4.0--0.5.0.sql
-- Migration from v0.4.0 to v0.5.0.
-- Adds the label_index table and its indexes.

CREATE TABLE _pg_eddy.label_index (
    label_id  INT    NOT NULL,
    node_id   BIGINT NOT NULL
);

CREATE INDEX ON _pg_eddy.label_index USING btree (label_id, node_id);
CREATE INDEX ON _pg_eddy.label_index USING btree (node_id);
