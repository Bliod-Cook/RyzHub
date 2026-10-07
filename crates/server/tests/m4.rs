//! M4 acceptance uses isolated real PostgreSQL/PGroonga/pgvector; embedding upstream is a fixture.
use anyhow::{Context, ensure};
use axum::{Json, Router, routing::post};
use ryzhub_server::{app::App, config::Config, http::router, memory, search, store::Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

struct Harness {
    admin: PgPool,
    db: PgPool,
    worker: PgPool,
    name: String,
    app: App,
    base: String,
    task: tokio::task::JoinHandle<()>,
    embedding: tokio::task::JoinHandle<()>,
}
impl Harness {
    async fn new() -> anyhow::Result<Self> {
        let url = std::env::var("TEST_ADMIN_URL").unwrap_or_else(|_| {
            "postgres://postgres:local-development-only@127.0.0.1:55432/postgres".into()
        });
        let admin = PgPool::connect(&url).await?;
        let root = url.rsplit_once('/').context("test URL")?.0;
        let name = format!("ryzhub_m4_test_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await?;
        let db = PgPool::connect(&format!("{root}/{name}")).await?;
        sqlx::migrate!("../../migrations").run(&db).await?;
        let address = root.split('@').nth(1).context("test URL")?;
        let worker = PgPool::connect(&format!(
            "postgres://ryzhub_worker:local-worker-only@{address}/{name}"
        ))
        .await?;
        let store = Store::connect(&format!(
            "postgres://ryzhub_app:local-app-only@{address}/{name}"
        ))
        .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mut config = Config::default();
        config.search.embedding_url = format!("http://{}", upstream.local_addr()?);
        config.search.embedding_model = "fixture-embedding-v1".into();
        config.search.embedding_key_env = "RYZHUB_TEST_EMBEDDING_KEY".into();
        let embedding = tokio::spawn(async move {
            let routes = Router::new().route(
                "/v1/embeddings",
                post(|Json(body): Json<Value>| async move {
                    let input = body["input"].as_array().unwrap();
                    if input
                        .iter()
                        .any(|v| v.as_str().unwrap().contains("embedding-unavailable"))
                    {
                        return (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"error":"fixture unavailable"})),
                        );
                    }
                    let data: Vec<Value> = input
                        .iter()
                        .enumerate()
                        .map(|(n, t)| {
                            let mut v = vec![0.0f32; 1024];
                            v[if t.as_str().unwrap().contains("苹果") {
                                0
                            } else {
                                1
                            }] = 1.0;
                            json!({"index":n,"embedding":v})
                        })
                        .collect();
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({"data":data,"usage":{"prompt_tokens":7,"total_tokens":7}})),
                    )
                }),
            );
            axum::serve(upstream, routes).await.unwrap();
        });
        let app = App::new(
            store,
            async_nats::connect("nats://127.0.0.1:54222").await?,
            config,
        )?;
        let routes = router(app.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        Ok(Self {
            admin,
            db,
            worker,
            name,
            app,
            base,
            task,
            embedding,
        })
    }
    async fn request(
        &self,
        user: &Value,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(user["token"].as_str().unwrap_or(""));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        ensure!(
            status.is_success(),
            "{path}: {status} {}",
            String::from_utf8_lossy(&bytes)
        );
        if bytes.is_empty() {
            Ok(json!({}))
        } else {
            Ok(serde_json::from_slice(&bytes)?)
        }
    }
    async fn user(&self) -> anyhow::Result<Value> {
        self.request(&json!({}),reqwest::Method::POST,"/auth/register",Some(json!({"email":format!("{}@example.test",Uuid::now_v7()),"password":"correct-test-password"}))).await
    }
    async fn conv(
        &self,
        user: &Value,
        project: Option<Uuid>,
        temporary: bool,
    ) -> anyhow::Result<Uuid> {
        let v = self
            .request(
                user,
                reqwest::Method::POST,
                &format!("/orgs/{}/conversations", user["org_id"].as_str().unwrap()),
                Some(json!({"title":"搜索测试","project_id":project,"temporary":temporary})),
            )
            .await?;
        ensure!(v["temporary"] == temporary, "temporary flag lost");
        Ok(id(&v, "id"))
    }
    async fn item(&self, org: Uuid, conv: Uuid, text: &str) -> anyhow::Result<Uuid> {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO items(id,org_id,conversation_id,revision,kind,state,text,first_seq,last_seq) VALUES($1,$2,$3,1,'user','completed',$4,1,1)")
            .bind(id).bind(org).bind(conv).bind(text).execute(&self.db).await?;
        Ok(id)
    }
    async fn drain(&self, org: Uuid) -> anyhow::Result<()> {
        for _ in 0..100 {
            let Some(job) = search::claim(&self.app, org).await? else {
                return Ok(());
            };
            search::process(&self.app, &job).await?;
        }
        anyhow::bail!("queue did not drain")
    }
    async fn hits(&self, user: &Value, query: &str) -> anyhow::Result<Vec<Value>> {
        let result = self
            .request(
                user,
                reqwest::Method::GET,
                &format!(
                    "/orgs/{}/search?q={}",
                    user["org_id"].as_str().unwrap(),
                    query
                ),
                None,
            )
            .await?;
        Ok(result["matches"].as_array().unwrap().clone())
    }
    async fn close(self) -> anyhow::Result<()> {
        self.task.abort();
        self.embedding.abort();
        self.app.store.pool.close().await;
        self.db.close().await;
        self.worker.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await?;
        Ok(())
    }
}
fn id(v: &Value, key: &str) -> Uuid {
    Uuid::parse_str(v[key].as_str().unwrap()).unwrap()
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-m4.sh"]
async fn m4_search_memory_acceptance() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").try_init();
    let h = Harness::new().await?;
    let result = acceptance(&h).await;
    h.close().await?;
    result
}
async fn acceptance(h: &Harness) -> anyhow::Result<()> {
    for secret in [
        "api_key=secret-value",
        "token:secret-value",
        "\"password\":\"secret-value\"",
        "{\"access_token\":\"secret-value\"}",
        "Authorization: Bearer secret-value",
        "refresh_token = secret-value",
    ] {
        ensure!(
            !memory::redact(secret).contains("secret-value"),
            "secret-shaped assignment not redacted: {secret}"
        );
    }
    let user = h.user().await?;
    let other = h.user().await?;
    let org = id(&user, "org_id");
    let owner = id(&user, "user_id");
    let conv = h.conv(&user, None, false).await?;
    let item = h.item(org, conv, "我喜欢苹果，中文搜索测试").await?;
    h.drain(org).await?;
    let hits = h.hits(&user, "苹果").await?;
    ensure!(
        hits.iter().any(|v| v["item_id"] == json!(item)
            && v["seq"] == 1
            && v["snippet_html"]
                .as_str()
                .is_some_and(|s| s.contains("keyword"))),
        "Chinese FTS/snippets/source navigation failed: {hits:?}"
    );
    // Vector and RRF queries use actual halfvec/HNSW, a separately tagged model and live ACL SQL.
    for mode in ["vector", "hybrid"] {
        let v = h
            .request(
                &user,
                reqwest::Method::GET,
                &format!("/orgs/{org}/search?q=苹果&mode={mode}"),
                None,
            )
            .await?;
        ensure!(
            v["matches"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["item_id"] == json!(item)),
            "{mode} search failed"
        );
    }
    let usage:i64=sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE org_id=$1 AND model='fixture-embedding-v1' AND status='completed'").bind(org).fetch_one(&h.db).await?;
    ensure!(usage >= 3, "embedding usage missing");
    let failed_embedding = h
        .item(org, conv, "embedding-unavailable 仍可全文搜索")
        .await?;
    let job = search::claim(&h.app, org)
        .await?
        .context("outage index job")?;
    search::process(&h.app, &job).await?;
    let pending:bool=sqlx::query_scalar("SELECT state='queued' AND error IS NOT NULL AND lease_until IS NULL FROM search_jobs WHERE org_id=$1 AND source_id=$2 AND source_version=$3")
        .bind(org).bind(failed_embedding).bind(job.source_version).fetch_one(&h.db).await?;
    ensure!(
        pending,
        "fulltext publication lost durable vector retry state"
    );
    ensure!(
        h.hits(&user, "仍可全文")
            .await?
            .iter()
            .any(|v| v["item_id"] == json!(failed_embedding)),
        "embedding failure hid fulltext"
    );
    let hybrid = h
        .request(
            &user,
            reqwest::Method::GET,
            &format!("/orgs/{org}/search?q=embedding-unavailable&mode=hybrid"),
            None,
        )
        .await?;
    ensure!(
        !hybrid["matches"].as_array().unwrap().is_empty(),
        "hybrid outage fallback failed"
    );
    for query in ["%28", "%22", "OR"] {
        h.request(
            &user,
            reqwest::Method::GET,
            &format!("/orgs/{org}/search?q={query}"),
            None,
        )
        .await?;
    }
    sqlx::query("DELETE FROM items WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(failed_embedding)
        .execute(&h.db)
        .await?;
    h.drain(org).await?;
    ensure!(
        h.hits(&other, "苹果").await?.is_empty(),
        "cross tenant leak"
    );
    // Creator changes take effect without indexing, even if copied index ACLs still say old owner.
    sqlx::query("UPDATE conversations SET creator_id=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(conv)
        .bind(id(&other, "user_id"))
        .execute(&h.db)
        .await?;
    ensure!(
        h.hits(&user, "苹果").await?.is_empty(),
        "stale owner ACL granted access"
    );
    sqlx::query("UPDATE conversations SET creator_id=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(conv)
        .bind(owner)
        .execute(&h.db)
        .await?;
    h.drain(org).await?;
    let backfill = h.item(org, conv, "低优先级回填").await?;
    sqlx::query("UPDATE search_jobs SET priority=-10 WHERE org_id=$1 AND source_id=$2")
        .bind(org)
        .bind(backfill)
        .execute(&h.db)
        .await?;
    let realtime = h.item(org, conv, "实时优先").await?;
    let next = search::claim(&h.app, org)
        .await?
        .context("foreground priority")?;
    ensure!(
        next.source_id == realtime,
        "backfill blocked foreground indexing"
    );
    search::process(&h.app, &next).await?;
    h.drain(org).await?;
    // Prepare v2, publish v3 first, then let the older embedding return.
    sqlx::query("UPDATE items SET text='旧苹果内容',revision=2 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(item)
        .execute(&h.db)
        .await?;
    let old = search::claim(&h.app, org).await?.context("old job")?;
    let mut tx = h.app.store.tx(org).await?;
    let prepared = search::source(&mut tx, &old).await?;
    tx.commit().await?;
    sqlx::query("UPDATE items SET text='新的梨内容',revision=3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(item)
        .execute(&h.db)
        .await?;
    h.drain(org).await?;
    ensure!(
        !search::publish(&h.app, &old, prepared, vec![]).await?,
        "old worker overwrote v3"
    );
    ensure!(
        h.hits(&user, "旧苹果").await?.is_empty(),
        "stale content searchable"
    );
    ensure!(
        !h.hits(&user, "梨").await?.is_empty(),
        "new version not indexed"
    );
    // A live-source deletion hides hits before the delete job runs and publishes a tombstone.
    sqlx::query("DELETE FROM items WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(item)
        .execute(&h.db)
        .await?;
    ensure!(
        h.hits(&user, "梨").await?.is_empty(),
        "deleted source leaked before indexing"
    );
    h.drain(org).await?;
    ensure!(
        sqlx::query_scalar::<_, bool>("SELECT deleted FROM search_docs WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(item)
            .fetch_one(&h.db)
            .await?,
        "missing tombstone"
    );
    // Reclaim fencing forbids a previous claimant from publishing the same source version.
    let fenced = h.item(org, conv, "租约苹果").await?;
    let job = search::claim(&h.app, org).await?.context("claimed")?;
    sqlx::query("UPDATE search_jobs SET lease_until=now()-interval '1 second' WHERE org_id=$1 AND source_id=$2").bind(org).bind(fenced).execute(&h.db).await?;
    let replacement = search::claim(&h.app, org).await?.context("reclaimed")?;
    let mut tx = h.app.store.tx(org).await?;
    let source = search::source(&mut tx, &job).await?;
    tx.commit().await?;
    ensure!(
        !search::publish(&h.app, &job, source, vec![]).await?,
        "expired claim published"
    );
    search::process(&h.app, &replacement).await?;
    // Temporary conversations are excluded from all sources and from memory reads/writes.
    let temporary = h.conv(&user, None, true).await?;
    h.item(org, temporary, "临时秘密测试").await?;
    h.drain(org).await?;
    ensure!(
        h.hits(&user, "临时秘密").await?.is_empty(),
        "temporary chat indexed"
    );
    let mut tx = h.app.store.tx(org).await?;
    ensure!(
        memory::snapshot(&mut tx, &h.app.config, org, temporary, owner)
            .await?
            .is_empty(),
        "temporary context injected"
    );
    tx.commit().await?;
    let make_project = |name: &str| json!({"name":name,"memory_mode":"project_only"});
    let p = h
        .request(
            &user,
            reqwest::Method::POST,
            &format!("/orgs/{org}/projects"),
            Some(make_project("隔离项目")),
        )
        .await?;
    let project = id(&p, "id");
    let inside = h.conv(&user, Some(project), false).await?;
    h.item(org, inside, "仅项目苹果").await?;
    let saved = h
        .request(
            &user,
            reqwest::Method::POST,
            &format!("/orgs/{org}/memories"),
            Some(json!({"content":"个人偏好简洁 sk-secret-token","source_conversation_id":conv})),
        )
        .await?;
    let scoped=h.request(&user,reqwest::Method::POST,&format!("/orgs/{org}/memories"),Some(json!({"content":"项目使用 Rust","project_id":project,"source_conversation_id":inside}))).await?;
    ensure!(
        !saved["content"].as_str().unwrap().contains("sk-secret"),
        "secret persisted"
    );
    let mut tx = h.app.store.tx(org).await?;
    let inside_snapshot = memory::snapshot(&mut tx, &h.app.config, org, inside, owner).await?;
    ensure!(
        inside_snapshot.contains("Rust") && !inside_snapshot.contains("个人偏好"),
        "project memory escaped scope"
    );
    let global = memory::snapshot(&mut tx, &h.app.config, org, conv, owner).await?;
    ensure!(
        global.contains("个人偏好") && !global.contains("Rust"),
        "global scope imported project memory"
    );
    tx.commit().await?;
    h.drain(org).await?;
    let q = search::SearchQuery {
        q: "苹果".into(),
        ..Default::default()
    };
    let global = search::retrieve(
        &h.app,
        org,
        owner,
        &q,
        search::Boundary {
            agent: true,
            project_only: None,
        },
        "history",
    )
    .await?;
    ensure!(
        !global.iter().any(|v| v["conversation_id"] == json!(inside)),
        "default agent found project-only history"
    );
    let restricted = search::retrieve(
        &h.app,
        org,
        owner,
        &q,
        search::Boundary {
            agent: true,
            project_only: Some(project),
        },
        "history",
    )
    .await?;
    ensure!(
        restricted.iter().all(|v| v["project_id"] == json!(project)),
        "project-only search escaped boundary"
    );
    // Writes are idempotent, and the same request key cannot be reused for different arguments.
    let input = json!({"content":"幂等记忆","request_key":"m4-idempotent"});
    let first = h
        .request(
            &user,
            reqwest::Method::POST,
            &format!("/orgs/{org}/memories"),
            Some(input.clone()),
        )
        .await?;
    let retry = h
        .request(
            &user,
            reqwest::Method::POST,
            &format!("/orgs/{org}/memories"),
            Some(input),
        )
        .await?;
    ensure!(
        first["id"] == retry["id"] && first["event_id"] == retry["event_id"],
        "memory retry wrote twice"
    );
    let mismatch = reqwest::Client::new()
        .post(format!("{}/orgs/{org}/memories", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"content":"changed args","request_key":"m4-idempotent"}))
        .send()
        .await?;
    ensure!(
        mismatch.status() == reqwest::StatusCode::CONFLICT,
        "request key reuse not rejected"
    );
    h.request(
        &user,
        reqwest::Method::DELETE,
        &format!("/orgs/{org}/memories/{}", first["id"].as_str().unwrap()),
        None,
    )
    .await?;
    // Snapshot stays frozen until a new conversation, with explicit deletion taking effect.
    let later = h
        .request(
            &user,
            reqwest::Method::POST,
            &format!("/orgs/{org}/memories"),
            Some(json!({"content":"后来加入的偏好"})),
        )
        .await?;
    let mut tx = h.app.store.tx(org).await?;
    ensure!(
        !memory::snapshot(&mut tx, &h.app.config, org, conv, owner)
            .await?
            .contains("后来加入"),
        "snapshot changed mid-conversation"
    );
    tx.commit().await?;
    let edited = h
        .request(
            &user,
            reqwest::Method::POST,
            &format!("/orgs/{org}/memories"),
            Some(json!({"content":"个人偏好详细","replace":saved["id"]})),
        )
        .await?;
    h.request(
        &user,
        reqwest::Method::POST,
        &format!(
            "/orgs/{org}/memory-events/{}/undo",
            edited["event_id"].as_str().unwrap()
        ),
        Some(json!({})),
    )
    .await?;
    let deleted = h
        .request(
            &user,
            reqwest::Method::DELETE,
            &format!("/orgs/{org}/memories/{}", later["id"].as_str().unwrap()),
            None,
        )
        .await?;
    h.request(
        &user,
        reqwest::Method::POST,
        &format!(
            "/orgs/{org}/memory-events/{}/undo",
            deleted["event_id"].as_str().unwrap()
        ),
        Some(json!({})),
    )
    .await?;
    let list = h
        .request(
            &user,
            reqwest::Method::GET,
            &format!("/orgs/{org}/memories"),
            None,
        )
        .await?;
    ensure!(
        list["items"].as_array().unwrap().len() == 2,
        "edit/delete undo failed"
    );
    // Digests use only user points and stay in scope; deleted projects disappear immediately.
    ensure!(
        memory::digest(&h.app, org, conv, owner).await?,
        "digest missing"
    );
    h.request(
        &user,
        reqwest::Method::PATCH,
        &format!("/orgs/{org}/memories/preferences"),
        Some(json!({"enabled":true,"reference_history":false})),
    )
    .await?;
    let without_history = h.conv(&user, None, false).await?;
    let mut tx = h.app.store.tx(org).await?;
    let context = memory::context(&mut tx, &h.app.config, org, without_history, owner).await?;
    ensure!(
        context.text.contains("个人偏好") && !context.text.contains("<recent>"),
        "history switch disabled saved memory or still injected history"
    );
    tx.commit().await?;
    let frozen: Value =
        sqlx::query_scalar("SELECT memory_snapshot FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(without_history)
            .fetch_one(&h.db)
            .await?;
    ensure!(
        frozen["recent"] == json!([]),
        "disabled history was copied into a persistent snapshot"
    );
    h.request(
        &user,
        reqwest::Method::PATCH,
        &format!("/orgs/{org}/memories/preferences"),
        Some(json!({"enabled":false,"reference_history":false})),
    )
    .await?;
    let mut tx = h.app.store.tx(org).await?;
    ensure!(
        memory::snapshot(&mut tx, &h.app.config, org, conv, owner)
            .await?
            .is_empty(),
        "disabled memory still injected"
    );
    tx.commit().await?;
    let frozen_job:search::Job=sqlx::query_as("UPDATE search_jobs SET state='running',claim_token=$3,lease_until=now()+interval '5 minutes' WHERE org_id=$1 AND source_id=$2 AND source_version=(SELECT version FROM memory_items WHERE org_id=$1 AND id=$2) RETURNING org_id,source_id,source_kind,source_version,op,claim_token")
        .bind(org).bind(id(&later,"id")).bind(Uuid::now_v7()).fetch_one(&h.db).await?;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE org_id=$1")
        .bind(org)
        .fetch_one(&h.db)
        .await?;
    search::process(&h.app, &frozen_job).await?;
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE org_id=$1")
        .bind(org)
        .fetch_one(&h.db)
        .await?;
    ensure!(
        before == after,
        "disabled memory was sent to embedding upstream"
    );
    h.request(
        &user,
        reqwest::Method::DELETE,
        &format!("/orgs/{org}/projects/{project}"),
        None,
    )
    .await?;
    ensure!(
        h.hits(&user, "仅项目苹果").await?.is_empty(),
        "deleted project still searchable"
    );
    h.request(
        &user,
        reqwest::Method::DELETE,
        &format!("/orgs/{org}/conversations/{conv}?delete_memories=true"),
        None,
    )
    .await?;
    ensure!(
        h.hits(&user, "租约苹果").await?.is_empty(),
        "deleted chat still searchable"
    );
    let referencing = h.conv(&user, None, false).await?;
    let unrelated = h.conv(&user, None, false).await?;
    sqlx::query("UPDATE conversations SET memory_snapshot=$3 WHERE org_id=$1 AND id=$2")
        .bind(org).bind(referencing).bind(json!({"saved":[],"recent":[{"id":conv,"title":"copied deleted title","points":"copied deleted points"}]})).execute(&h.db).await?;
    sqlx::query("UPDATE conversations SET memory_snapshot=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(unrelated)
        .bind(json!({"saved":[],"recent":[]}))
        .execute(&h.db)
        .await?;
    let mut lock = h.db.begin().await?;
    sqlx::query("SELECT id FROM conversations WHERE org_id=$1 AND id=$2 FOR UPDATE")
        .bind(org)
        .bind(unrelated)
        .execute(&mut *lock)
        .await?;
    let mut cleanup = h.app.store.tx(org).await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        memory::collect_history_references(&mut cleanup, org),
    )
    .await
    .context("snapshot cleanup locked an unrelated conversation")??;
    cleanup.commit().await?;
    lock.rollback().await?;
    let recent: Value = sqlx::query_scalar(
        "SELECT memory_snapshot->'recent' FROM conversations WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(referencing)
    .fetch_one(&h.db)
    .await?;
    ensure!(
        recent == json!([]),
        "deleted history snapshot content survived collection"
    );
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM memory_items WHERE org_id=$1 AND source_conversation_id=$2 AND status='active'").bind(org).bind(conv).fetch_one(&h.db).await?;
    ensure!(
        count == 0,
        "associated memories survived requested deletion"
    );
    ensure!(
        memory::collect_conversation(&h.app, org, conv).await?,
        "deleted conversation payload collection failed"
    );
    let payloads: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items WHERE org_id=$1 AND conversation_id=$2 AND text<>''",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    ensure!(payloads == 0, "deleted payloads retained");
    // Exercise the actual read-only worker role and both supervised loops, not just helper calls.
    let fresh = h.user().await?;
    let fresh_org = id(&fresh, "org_id");
    let fresh_conv = h.conv(&fresh, None, false).await?;
    h.item(fresh_org, fresh_conv, "后台自动检索摘要").await?;
    let digest_worker = tokio::spawn(memory::collector(h.app.clone(), h.worker.clone()));
    let index_worker = tokio::spawn(search::worker(h.app.clone(), h.worker.clone()));
    let observed=tokio::time::timeout(std::time::Duration::from_secs(10),async{
        loop{
            let found:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversation_digests WHERE org_id=$1 AND conversation_id=$2)").bind(fresh_org).bind(fresh_conv).fetch_one(&h.db).await?;
            let indexed= !h.hits(&fresh,"后台自动").await?.is_empty();
            if found && indexed{return Ok::<_,anyhow::Error>(());}
            if digest_worker.is_finished() || index_worker.is_finished(){anyhow::bail!("background worker stopped");}
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }).await;
    digest_worker.abort();
    index_worker.abort();
    observed??;
    // Native Work callers reach built-in memory tools through the actual MCP HTTP gateway.
    let work_conv = h.conv(&fresh, None, false).await?;
    let lease = h
        .app
        .store
        .claim(fresh_org, work_conv, Uuid::now_v7(), 300_000)
        .await?
        .context("MCP lease")?;
    let mut tx = h.app.store.fenced(&lease).await?;
    let command = ryzhub_proto::CommandEnvelope {
        cid: Uuid::now_v7().to_string(),
        ..Default::default()
    };
    let run = ryzhub_server::runs::create(
        &mut tx,
        &lease,
        &command,
        ryzhub_server::runs::Input::text("M4 MCP memory"),
        ryzhub_server::runs::RunKind::Main,
        ryzhub_server::owner::Engine::Mock,
        &h.app.config,
    )
    .await?;
    let sandbox = Uuid::now_v7();
    sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent,node_lease_until) VALUES($1,$2,$3,$4,1,$5,'running','{}',true,now()+interval '5 minutes')")
        .bind(sandbox).bind(fresh_org).bind(work_conv).bind(run).bind(lease.epoch).execute(&mut *tx).await?;
    tx.commit().await?;
    let key = ryzhub_server::llm::keys::issue_sandbox_key(
        &h.app.store,
        ryzhub_server::llm::keys::SandboxKeyRequest {
            org_id: fresh_org,
            user_id: id(&fresh, "user_id"),
            conversation_id: work_conv,
            run_id: run,
            sandbox_id: sandbox,
            lease_epoch: 1,
            models: vec!["mock".into()],
            credential_id: None,
        },
    )
    .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let gateway = format!("http://{}/mcp", listener.local_addr()?);
    let routes = ryzhub_server::llm::gateway::router(h.app.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, routes).await.unwrap();
    });
    let input = json!({"jsonrpc":"2.0","id":"m4-mcp-save","method":"tools/call","params":{"name":"memory_save","arguments":{"content":"通过 MCP 保存的显式记忆"}}});
    for _ in 0..2 {
        let v: Value = reqwest::Client::new()
            .post(&gateway)
            .bearer_auth(&key.token)
            .json(&input)
            .send()
            .await?
            .json()
            .await?;
        ensure!(v.get("error").is_none(), "MCP memory call failed: {v}");
    }
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_items WHERE org_id=$1 AND content='通过 MCP 保存的显式记忆'",
    )
    .bind(fresh_org)
    .fetch_one(&h.db)
    .await?;
    ensure!(n == 1, "MCP retries duplicated memory writes");
    let mut tx = h.app.store.fenced(&lease).await?;
    memory::project(&mut tx, &lease).await?;
    let instructions =
        ryzhub_server::projects::instructions(&mut tx, &lease, run, &h.app.config).await?;
    ensure!(
        instructions.contains("通过 MCP 保存"),
        "Work memory injection missing"
    );
    tx.commit().await?;
    let notices:i64=sqlx::query_scalar("SELECT count(*) FROM items WHERE org_id=$1 AND conversation_id=$2 AND kind IN ('memory_notice','memory_context')").bind(fresh_org).bind(work_conv).fetch_one(&h.db).await?;
    ensure!(notices == 2, "memory write/use timeline hints missing");
    h.drain(fresh_org).await?;
    let context_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM items WHERE org_id=$1 AND conversation_id=$2 AND kind='memory_context'",
    )
    .bind(fresh_org)
    .bind(work_conv)
    .fetch_one(&h.db)
    .await?;
    let context_path =
        format!("/orgs/{fresh_org}/conversations/{work_conv}/memory-context/{context_id}");
    let details = h
        .request(&fresh, reqwest::Method::GET, &context_path, None)
        .await?;
    let memory_entry = details["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["content"] == "通过 MCP 保存的显式记忆")
        .context("timeline memory details missing")?;
    let memory_id = id(memory_entry, "id");
    let persisted: String = sqlx::query_scalar("SELECT text FROM items WHERE org_id=$1 AND id=$2")
        .bind(fresh_org)
        .bind(context_id)
        .fetch_one(&h.db)
        .await?;
    ensure!(
        !persisted.contains("显式记忆"),
        "timeline duplicated memory content"
    );
    h.request(
        &fresh,
        reqwest::Method::DELETE,
        &format!("/orgs/{fresh_org}/memories/{memory_id}"),
        None,
    )
    .await?;
    let details = h
        .request(&fresh, reqwest::Method::GET, &context_path, None)
        .await?;
    ensure!(
        !details["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == json!(memory_id)),
        "deleted memory visible in timeline"
    );
    sqlx::query(
        "UPDATE memory_items SET invalid_at=now()-interval '31 days' WHERE org_id=$1 AND id=$2",
    )
    .bind(fresh_org)
    .bind(memory_id)
    .execute(&h.db)
    .await?;
    let mut tx = h.app.store.tx(fresh_org).await?;
    memory::collect_stale_memories(&mut tx, fresh_org).await?;
    tx.commit().await?;
    let retained:bool=sqlx::query_scalar("SELECT content<>'' OR EXISTS(SELECT 1 FROM memory_events WHERE org_id=$1 AND memory_id=$2) OR EXISTS(SELECT 1 FROM search_docs WHERE org_id=$1 AND id=$2) FROM memory_items WHERE org_id=$1 AND id=$2").bind(fresh_org).bind(memory_id).fetch_one(&h.db).await?;
    ensure!(!retained, "memory survived undo retention window");
    sqlx::query("UPDATE conversations SET temporary=true WHERE org_id=$1 AND id=$2")
        .bind(fresh_org)
        .bind(work_conv)
        .execute(&h.db)
        .await?;
    let mut denied = input;
    denied["id"] = json!("m4-mcp-denied");
    let v: Value = reqwest::Client::new()
        .post(&gateway)
        .bearer_auth(&key.token)
        .json(&denied)
        .send()
        .await?
        .json()
        .await?;
    ensure!(
        v["error"]["message"] == "POLICY_DENIED",
        "temporary Work memory write allowed: {v}"
    );
    server.abort();
    let _ = scoped;
    Ok(())
}
