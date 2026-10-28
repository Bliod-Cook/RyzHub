-- Instance admins disable and delete accounts. A deleted account can be restored, disabled,
-- until `delete_after`; then it is anonymized and its personal organization purged:
-- `deleting` while its content goes, `deleted` once nothing of it is left but tombstones.
ALTER TABLE users
  ADD CONSTRAINT users_status_check CHECK (status IN ('active','disabled','deletion_pending','deleted')),
  ADD COLUMN delete_after timestamptz,
  ADD CONSTRAINT users_delete_after_check CHECK ((status = 'deletion_pending') = (delete_after IS NOT NULL));
CREATE INDEX users_deletion_due ON users (delete_after) WHERE status = 'deletion_pending';
ALTER TABLE organizations
  ADD CONSTRAINT organizations_state_check CHECK (state IN ('active','deleting','deleted'));
-- The personal organization of `member`, whatever the state of either.
CREATE FUNCTION personal_org(member uuid) RETURNS uuid
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog,public AS $$
    SELECT o.id FROM public.organizations o JOIN public.memberships m ON m.org_id = o.id
    WHERE m.user_id = member AND m.role = 'owner' AND o.kind = 'personal'
    ORDER BY o.id LIMIT 1
$$;
REVOKE ALL ON FUNCTION personal_org(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION personal_org(uuid) TO ryzhub_app;
