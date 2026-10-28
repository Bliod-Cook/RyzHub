-- Instance-wide figures for the admin overview, across organizations: aggregates only. The
-- period is the last `days` days in the viewer's time zone, `tz` east of UTC; daily series
-- are counted in that zone.
CREATE FUNCTION instance_stats(days integer, tz interval) RETURNS jsonb
LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog,public
-- Compiling this many expressions costs more than it saves.
SET jit=off AS $$
WITH period AS (
    SELECT first_day, (first_day::timestamp - tz) AT TIME ZONE 'UTC' AS since
    FROM (SELECT (now() AT TIME ZONE 'UTC' + tz)::date - (days - 1) AS first_day) f
),
calendar AS (
    SELECT (first_day + n) AS day FROM period, generate_series(0, days - 1) n
),
usage_days AS (
    SELECT (u.created_at AT TIME ZONE 'UTC' + tz)::date AS day, u.user_id,
           sum(u.input_tokens) AS input_tokens, sum(u.output_tokens) AS output_tokens
    FROM public.usage_records u WHERE u.created_at >= (SELECT since FROM period)
    GROUP BY 1, 2
),
usage_kinds AS (
    SELECT u.model, u.status, u.estimated, count(*) AS requests,
           sum(u.input_tokens) AS input_tokens, sum(u.output_tokens) AS output_tokens
    FROM public.usage_records u WHERE u.created_at >= (SELECT since FROM period)
    GROUP BY 1, 2, 3
),
started AS (
    SELECT (r.started_at AT TIME ZONE 'UTC' + tz)::date AS day, r.harness, r.state, count(*) AS n
    FROM public.runs r WHERE r.started_at >= (SELECT since FROM period)
    GROUP BY 1, 2, 3
),
joined AS (
    SELECT (uuid_extract_timestamp(u.id) AT TIME ZONE 'UTC' + tz)::date AS day, count(*) AS n
    FROM public.users u WHERE uuid_extract_timestamp(u.id) >= (SELECT since FROM period)
    GROUP BY 1
)
SELECT jsonb_build_object(
    'first_day', (SELECT first_day FROM period),
    'users', (SELECT jsonb_build_object(
        'total', count(*) FILTER (WHERE status <> 'deleted'),
        'disabled', count(*) FILTER (WHERE status = 'disabled'),
        'deletion_pending', count(*) FILTER (WHERE status = 'deletion_pending'))
        FROM public.users),
    'model_users', (SELECT jsonb_build_object(
        'week', count(*) FILTER (WHERE last > now() - interval '7 days'), 'month', count(*))
        FROM (SELECT max(created_at) AS last FROM public.usage_records
              WHERE created_at > now() - interval '30 days' GROUP BY user_id) m),
    'conversations', (SELECT jsonb_build_object(
        'chat', count(*) FILTER (WHERE mode = 'chat'),
        'work', count(*) FILTER (WHERE mode = 'work'))
        FROM public.conversations WHERE state IN ('active', 'archived')),
    'runs', (SELECT jsonb_build_object(
        'running', count(*) FILTER (WHERE state = 'running'),
        'requires_action', count(*) FILTER (WHERE state = 'requires_action'))
        FROM public.runs WHERE state IN ('running', 'requires_action')),
    'sandboxes_running', (SELECT count(*) FROM public.sandboxes WHERE state = 'running'),
    -- Null when a node takes any number (capacity 0).
    'node_capacity', (SELECT CASE WHEN bool_or(capacity = 0) THEN NULL
        ELSE coalesce(sum(capacity), 0) END FROM public.execution_nodes WHERE state = 'active'),
    'nodes', (SELECT coalesce(jsonb_object_agg(state, n), '{}') FROM
        (SELECT state, count(*) AS n FROM public.execution_nodes GROUP BY state) s),
    -- Stored content once per organization and hash, as storage keeps it.
    'checkpoint_bytes', (SELECT coalesce(sum(size), 0) FROM
        (SELECT DISTINCT org_id, manifest->>'sha256', (manifest->>'size')::bigint AS size
         FROM public.checkpoints WHERE state IN ('committed', 'tombstoned')) c),
    'attachment_bytes', (SELECT coalesce(sum(size), 0) FROM
        (SELECT DISTINCT org_id, sha256, size FROM public.attachments WHERE state = 'ready') a),
    'daily', (SELECT jsonb_agg(jsonb_build_object(
        'day', c.day,
        'input_tokens', coalesce(u.input_tokens, 0),
        'output_tokens', coalesce(u.output_tokens, 0),
        'model_users', coalesce(u.users, 0),
        'chat_runs', coalesce(r.chat, 0),
        'work_runs', coalesce(r.work, 0),
        'new_users', coalesce(j.n, 0)) ORDER BY c.day)
        FROM calendar c
        LEFT JOIN (SELECT day, sum(input_tokens) AS input_tokens, sum(output_tokens) AS output_tokens,
                          count(*) AS users FROM usage_days GROUP BY day) u USING (day)
        LEFT JOIN (SELECT day, sum(n) FILTER (WHERE harness = 'chat') AS chat,
                          sum(n) FILTER (WHERE harness <> 'chat') AS work FROM started GROUP BY day) r
            USING (day)
        LEFT JOIN joined j USING (day)),
    'models', (SELECT coalesce(jsonb_agg(jsonb_build_object('model', model, 'input_tokens', i,
        'output_tokens', o) ORDER BY i + o DESC, model), '[]') FROM
        (SELECT model, sum(input_tokens) AS i, sum(output_tokens) AS o FROM usage_kinds
         GROUP BY model) m),
    'harnesses', (SELECT coalesce(jsonb_agg(jsonb_build_object('harness', harness, 'runs', n)
        ORDER BY n DESC, harness), '[]') FROM
        (SELECT harness, sum(n) AS n FROM started GROUP BY harness) h),
    'run_outcomes', (SELECT jsonb_build_object(
        'completed', coalesce(sum(n) FILTER (WHERE state = 'completed'), 0),
        'failed', coalesce(sum(n) FILTER (WHERE state = 'failed'), 0),
        'cancelled', coalesce(sum(n) FILTER (WHERE state = 'cancelled'), 0)) FROM started),
    'request_statuses', (SELECT jsonb_build_object(
        'completed', coalesce(sum(requests) FILTER (WHERE status = 'completed'), 0),
        'interrupted', coalesce(sum(requests) FILTER (WHERE status = 'interrupted'), 0),
        'upstream_error', coalesce(sum(requests) FILTER (WHERE status = 'upstream_error'), 0))
        FROM usage_kinds),
    'top_users', (SELECT coalesce(jsonb_agg(jsonb_build_object('user_id', t.user_id,
        'email', u.email, 'input_tokens', t.i, 'output_tokens', t.o) ORDER BY t.i + t.o DESC), '[]')
        FROM (SELECT user_id, sum(input_tokens) AS i, sum(output_tokens) AS o FROM usage_days
              GROUP BY user_id ORDER BY sum(input_tokens + output_tokens) DESC LIMIT 10) t
        JOIN public.users u ON u.id = t.user_id),
    'estimated_tokens', (SELECT coalesce(sum(input_tokens + output_tokens) FILTER (WHERE estimated), 0)
        FROM usage_kinds),
    'total_tokens', (SELECT coalesce(sum(input_tokens + output_tokens), 0) FROM usage_kinds)
)
$$;
-- Runs in a period, newest first, out of all of an instance's runs.
CREATE INDEX runs_started ON runs (started_at);
REVOKE ALL ON FUNCTION instance_stats(integer, interval) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION instance_stats(integer, interval) TO ryzhub_app;
