-- Digest-pinned images on execution nodes that nothing references, and since when; an image
-- unreferenced for long enough is removed from its node.
CREATE TABLE node_images (
    node text NOT NULL REFERENCES execution_nodes(name),
    reference text NOT NULL,
    unreferenced_since timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (node, reference)
);
GRANT SELECT, INSERT, UPDATE, DELETE ON node_images TO ryzhub_app;

-- Live sandboxes per node, across organizations, for placement.
CREATE FUNCTION node_load() RETURNS TABLE(node text, live bigint)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog,public AS $$
    SELECT s.node, count(*) FROM public.sandboxes s
    WHERE s.state IN ('running','provisioning') GROUP BY s.node
$$;
REVOKE ALL ON FUNCTION node_load() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION node_load() TO ryzhub_app;
