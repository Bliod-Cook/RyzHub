-- Identity-scoped discovery is the only cross-org lookup available to runtime.
-- These narrowly scoped read functions return no conversation content or secrets.
CREATE FUNCTION resolve_conversation_org(requester uuid, target uuid) RETURNS uuid
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog,public AS $$
    SELECT c.org_id FROM public.conversations c
    JOIN public.memberships m ON m.org_id=c.org_id AND m.user_id=requester
    JOIN public.organizations o ON o.id=c.org_id AND o.state='active'
    JOIN public.users u ON u.id=requester AND u.status='active'
    WHERE c.id=target AND c.creator_id=requester AND c.state IN ('active','archived')
$$;
CREATE FUNCTION list_user_orgs(requester uuid) RETURNS TABLE(id uuid,name text)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog,public AS $$
    SELECT o.id,o.name FROM public.organizations o
    JOIN public.memberships m ON m.org_id=o.id AND m.user_id=requester
    JOIN public.users u ON u.id=requester AND u.status='active'
    WHERE o.state='active' ORDER BY o.id
$$;
REVOKE ALL ON FUNCTION resolve_conversation_org(uuid,uuid),list_user_orgs(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION resolve_conversation_org(uuid,uuid),list_user_orgs(uuid) TO ryzhub_app;
