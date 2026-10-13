-- Execution nodes by the name the instance configuration gives them. Where a node listens is
-- configuration only; this table records scheduling state. Each sandbox names its node.
CREATE TABLE execution_nodes (
    name text PRIMARY KEY CHECK (name ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    capacity integer NOT NULL DEFAULT 0 CHECK (capacity >= 0),
    state text NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'draining', 'offline')),
    lease_until timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now()
);
GRANT SELECT, INSERT, UPDATE ON execution_nodes TO ryzhub_app;
GRANT SELECT ON execution_nodes TO ryzhub_worker;

-- Sandboxes so far ran on the one node a configuration without `sandbox.nodes` has.
INSERT INTO execution_nodes(name) VALUES ('default');
ALTER TABLE sandboxes ADD COLUMN node text NOT NULL DEFAULT 'default' REFERENCES execution_nodes(name);
CREATE INDEX sandboxes_node_live ON sandboxes(node) WHERE state IN ('running', 'provisioning');
