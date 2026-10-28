//! Search and memory against isolated real PostgreSQL/PGroonga/pgvector. Digests are summarized
//! by the fixture model; vector search uses a real embedding provider when one is configured.
mod common;
use anyhow::{Context, ensure};
use common::{Harness, id};
use reqwest::Method;
use ryzhub_proto::{CommandEnvelope, UndoMemoryUpdate, command_envelope::Command, frame::Body};
use ryzhub_server::{
    memory, search,
    user_commands::{self, Intake},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

impl Harness {
    /// Fulltext search only.
    async fn new() -> anyhow::Result<Self> {
        Self::embedding("", "", "").await
    }
    /// Search with embeddings of `model` from `url`, under the key in `key_env`.
    async fn embedding(url: &str, model: &str, key_env: &str) -> anyhow::Result<Self> {
        let (url, model, key_env) = (url.to_owned(), model.to_owned(), key_env.to_owned());
        Self::start(move |config| {
            config.llm.enabled = true;
            config.search.embedding_url = url;
            config.search.embedding_model = model;
            config.search.embedding_key_env = key_env;
        })
        .await
    }
    async fn request(
        &self,
        user: &Value,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        let mut request = reqwest::Client::new()
            .request(
                method,
                format!("{}/orgs/{}{path}", self.base, id(user, "org_id")),
            )
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
    /// Save a memory through the API.
    async fn remember(&self, user: &Value, body: Value) -> anyhow::Result<Value> {
        self.request(user, Method::POST, "/memories", Some(body))
            .await
    }
    async fn conv(
        &self,
        user: &Value,
        project: Option<Uuid>,
        temporary: bool,
    ) -> anyhow::Result<Uuid> {
        let ctx = self.context(user).await?;
        let settings = json!({"temporary": temporary});
        let created = self.create(&ctx, "搜索测试", "", settings, project).await?;
        ensure!(created.temporary == temporary, "temporary flag lost");
        Ok(created.id)
    }
    /// Undo a memory change as clients do: by command on the user channel.
    async fn undo(&self, user: &Value, event: &Value) -> anyhow::Result<()> {
        let (org, member) = (id(user, "org_id"), id(user, "user_id"));
        let undo = UndoMemoryUpdate {
            event_id: event.as_str().unwrap().into(),
        };
        let envelope = CommandEnvelope {
            cid: Uuid::now_v7().to_string(),
            command: Some(Command::UndoMemoryUpdate(undo)),
        };
        let channel = format!("user:{org}:{member}");
        let intake =
            user_commands::accept(&self.app, (org, member), &channel, "test", &envelope).await?;
        let Intake::Done(result) = intake else {
            anyhow::bail!("undo queued")
        };
        ensure!(
            matches!(result.body, Some(Body::CommandAck(_))),
            "undo refused: {result:?}"
        );
        Ok(())
    }
    /// A completed user message in `conv`.
    async fn item(&self, org: Uuid, conv: Uuid, text: &str) -> anyhow::Result<Uuid> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO items(id,org_id,conversation_id,revision,kind,state,text,first_seq,
               last_seq)
             VALUES($1,$2,$3,1,'user','completed',$4,1,1)",
        )
        .bind(id)
        .bind(org)
        .bind(conv)
        .bind(text)
        .execute(&self.db)
        .await?;
        Ok(id)
    }
    /// Index everything the organization has queued.
    async fn drain(&self, org: Uuid) -> anyhow::Result<()> {
        for _ in 0..100 {
            let Some(job) = search::claim(&self.app, org).await? else {
                return Ok(());
            };
            search::process(&self.app, &job).await?;
        }
        anyhow::bail!("queue did not drain")
    }
    async fn search(&self, user: &Value, query: &str, mode: &str) -> anyhow::Result<Vec<Value>> {
        let path = format!("/search?q={query}&mode={mode}");
        let result = self.request(user, Method::GET, &path, None).await?;
        Ok(result["matches"].as_array().unwrap().clone())
    }
    /// Fulltext hits.
    async fn hits(&self, user: &Value, query: &str) -> anyhow::Result<Vec<Value>> {
        self.search(user, query, "fulltext").await
    }
    /// The memory a run in `conv` is given.
    async fn memory(&self, user: &Value, conv: Uuid) -> anyhow::Result<String> {
        let (org, owner) = (id(user, "org_id"), id(user, "user_id"));
        let mut tx = self.app.store.tx(org).await?;
        let context = memory::context(&mut tx, &self.app.config(), org, conv, owner).await?;
        tx.commit().await?;
        Ok(context.text)
    }
}

fn found(hits: &[Value], item: Uuid) -> bool {
    hits.iter().any(|v| v["item_id"] == json!(item))
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn chinese_fulltext_highlights_and_links_its_source() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let conv = h.conv(&user, None, false).await?;
    let item = h.item(org, conv, "我喜欢苹果，中文搜索测试").await?;
    h.drain(org).await?;
    let hits = h.hits(&user, "苹果").await?;
    ensure!(
        hits.iter().any(|v| v["item_id"] == json!(item)
            && v["seq"] == 1
            && v["snippet"] == "我喜欢苹果，中文搜索测试"
            && v["highlights"] == json!([[3, 5]])),
        "{hits:?}"
    );
    // Query syntax is searched for, never parsed.
    for query in ["%28", "%22", "OR"] {
        h.hits(&user, query).await?;
    }
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL, NATS and an embedding provider; \
            scripts/test-search.sh"]
async fn vector_search_finds_messages_and_memories_by_meaning() -> anyhow::Result<()> {
    let provider = ["RYZHUB_TEST_EMBEDDING_URL", "RYZHUB_TEST_EMBEDDING_MODEL"]
        .map(|name| std::env::var(name).unwrap_or_default());
    if provider.iter().any(String::is_empty)
        || std::env::var_os("RYZHUB_TEST_PROVIDER_KEY").is_none()
    {
        println!(
            "vector search skipped: RYZHUB_TEST_EMBEDDING_URL, RYZHUB_TEST_EMBEDDING_MODEL or \
             RYZHUB_TEST_PROVIDER_KEY is not set"
        );
        return Ok(());
    }
    let [url, model] = provider;
    let h = Harness::embedding(&url, &model, "RYZHUB_TEST_PROVIDER_KEY").await?;
    let user = h.user().await?;
    let (org, owner) = (id(&user, "org_id"), id(&user, "user_id"));
    let conv = h.conv(&user, None, false).await?;
    let fruit = h.item(org, conv, "我每天早上吃一根香蕉和一个苹果").await?;
    h.item(org, conv, "数据库迁移脚本在周五凌晨执行").await?;
    h.remember(&user, json!({"content":"我最喜欢的运动是游泳"}))
        .await?;
    h.remember(&user, json!({"content":"我习惯用 Vim 写代码"}))
        .await?;
    h.drain(org).await?;
    // No word of the query is in what it finds.
    ensure!(h.hits(&user, "水果").await?.is_empty());
    for mode in ["vector", "hybrid"] {
        let hits = h.search(&user, "水果", mode).await?;
        ensure!(hits[0]["item_id"] == json!(fruit), "{mode}: {hits:?}");
    }
    let q = search::SearchQuery {
        q: "体育爱好".into(),
        mode: "vector".into(),
        ..Default::default()
    };
    let boundary = search::Boundary::default();
    let memories = search::retrieve(&h.app, org, owner, &q, boundary, "memory").await?;
    ensure!(
        memories[0]["text"].as_str().unwrap().contains("游泳"),
        "{memories:?}"
    );
    // Every call is metered under the provider's model.
    let (calls, completed): (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER (WHERE status='completed') FROM usage_records
         WHERE org_id=$1 AND model=$2",
    )
    .bind(org)
    .bind(&model)
    .fetch_one(&h.db)
    .await?;
    ensure!(calls > 0 && completed == calls, "{completed}/{calls}");
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn embedding_outages_keep_fulltext_and_memory_turned_off_is_never_embedded()
-> anyhow::Result<()> {
    // Nothing listens there.
    let h = Harness::embedding(
        "http://127.0.0.1:1",
        "dead-embedding",
        "RYZHUB_TEST_UPSTREAM_KEY",
    )
    .await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let conv = h.conv(&user, None, false).await?;
    h.drain(org).await?;
    let item = h.item(org, conv, "向量服务中断时仍可全文搜索").await?;
    let job = search::claim(&h.app, org).await?.context("index job")?;
    search::process(&h.app, &job).await?;
    let retrying: bool = sqlx::query_scalar(
        "SELECT state='queued' AND error IS NOT NULL AND lease_until IS NULL FROM search_jobs
         WHERE org_id=$1 AND source_id=$2 AND source_version=$3",
    )
    .bind(org)
    .bind(item)
    .bind(job.source_version)
    .fetch_one(&h.db)
    .await?;
    ensure!(retrying, "fulltext publication lost the vectors' retry");
    ensure!(found(&h.hits(&user, "全文搜索").await?, item));
    ensure!(found(&h.search(&user, "全文搜索", "hybrid").await?, item));

    // Indexing memory the member turned off makes no embedding call, so no usage record.
    let other = h.user().await?;
    let other_org = id(&other, "org_id");
    let saved = h
        .remember(&other, json!({"content":"不应发送的记忆"}))
        .await?;
    let preferences = json!({"enabled":false,"reference_history":false});
    h.request(
        &other,
        Method::PATCH,
        "/memories/preferences",
        Some(preferences),
    )
    .await?;
    h.drain(other_org).await?;
    let (state, calls): (String, i64) = sqlx::query_as(
        "SELECT state,(SELECT count(*) FROM usage_records WHERE org_id=$1) FROM search_jobs
         WHERE org_id=$1 AND source_id=$2",
    )
    .bind(other_org)
    .bind(id(&saved, "id"))
    .fetch_one(&h.db)
    .await?;
    ensure!(
        (state.as_str(), calls) == ("completed", 0),
        "{state}, {calls}"
    );
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn hits_follow_the_tenant_and_the_current_creator_without_reindexing() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let (user, other) = (h.user().await?, h.user().await?);
    let org = id(&user, "org_id");
    let conv = h.conv(&user, None, false).await?;
    let item = h.item(org, conv, "我喜欢苹果").await?;
    h.drain(org).await?;
    ensure!(found(&h.hits(&user, "苹果").await?, item));
    ensure!(
        h.hits(&other, "苹果").await?.is_empty(),
        "cross tenant leak"
    );
    // The index still names the old creator.
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
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn indexing_prefers_live_work_and_never_publishes_stale_versions() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let conv = h.conv(&user, None, false).await?;
    h.drain(org).await?;
    // A backfill waits for what is being written now.
    let backfill = h.item(org, conv, "低优先级回填").await?;
    sqlx::query("UPDATE search_jobs SET priority=-10 WHERE org_id=$1 AND source_id=$2")
        .bind(org)
        .bind(backfill)
        .execute(&h.db)
        .await?;
    let live = h.item(org, conv, "实时优先").await?;
    let next = search::claim(&h.app, org).await?.context("live job")?;
    ensure!(next.source_id == live, "backfill blocked live indexing");
    search::process(&h.app, &next).await?;
    h.drain(org).await?;

    // Version 2 is prepared, version 3 published first; version 2 then cannot overwrite it.
    let item = h.item(org, conv, "原始内容").await?;
    h.drain(org).await?;
    let edit = |text: &str, revision: i64| {
        sqlx::query("UPDATE items SET text=$3,revision=$4 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(item)
            .bind(text.to_owned())
            .bind(revision)
            .execute(&h.db)
    };
    edit("旧苹果内容", 2).await?;
    let old = search::claim(&h.app, org).await?.context("old job")?;
    let mut tx = h.app.store.tx(org).await?;
    let prepared = search::source(&mut tx, &old).await?;
    tx.commit().await?;
    edit("新的梨内容", 3).await?;
    h.drain(org).await?;
    ensure!(
        !search::publish(&h.app, &old, prepared, vec![], None).await?,
        "old worker overwrote v3"
    );
    ensure!(h.hits(&user, "旧苹果").await?.is_empty(), "stale content");
    ensure!(found(&h.hits(&user, "梨").await?, item), "v3 missing");

    // A deleted source is hidden before its delete job runs, which leaves a tombstone.
    sqlx::query("DELETE FROM items WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(item)
        .execute(&h.db)
        .await?;
    ensure!(
        h.hits(&user, "梨").await?.is_empty(),
        "deleted source shown"
    );
    h.drain(org).await?;
    let tombstone: bool =
        sqlx::query_scalar("SELECT deleted FROM search_docs WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(item)
            .fetch_one(&h.db)
            .await?;
    ensure!(tombstone, "missing tombstone");

    // A claim that expired cannot publish once it is claimed again.
    let fenced = h.item(org, conv, "租约内容").await?;
    let job = search::claim(&h.app, org).await?.context("claimed")?;
    sqlx::query(
        "UPDATE search_jobs SET lease_until=now()-interval '1 second'
         WHERE org_id=$1 AND source_id=$2",
    )
    .bind(org)
    .bind(fenced)
    .execute(&h.db)
    .await?;
    let replacement = search::claim(&h.app, org).await?.context("reclaimed")?;
    let mut tx = h.app.store.tx(org).await?;
    let source = search::source(&mut tx, &job).await?;
    tx.commit().await?;
    ensure!(
        !search::publish(&h.app, &job, source, vec![], None).await?,
        "expired claim published"
    );
    ensure!(search::process(&h.app, &replacement).await?);
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn temporary_conversations_stay_out_of_search_and_memory() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    h.remember(&user, json!({"content":"个人偏好简洁"})).await?;
    let temporary = h.conv(&user, None, true).await?;
    h.item(org, temporary, "临时秘密测试").await?;
    h.drain(org).await?;
    ensure!(h.hits(&user, "临时秘密").await?.is_empty(), "indexed");
    ensure!(h.memory(&user, temporary).await?.is_empty(), "memory given");
    let usual = h.conv(&user, None, false).await?;
    ensure!(h.memory(&user, usual).await?.contains("个人偏好"));
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn project_only_memory_and_history_stay_in_their_project() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let (org, owner) = (id(&user, "org_id"), id(&user, "user_id"));
    let project = json!({"name":"隔离项目","memory_mode":"project_only"});
    let project = id(
        &h.request(&user, Method::POST, "/projects", Some(project))
            .await?,
        "id",
    );
    let (inside, outside) = (
        h.conv(&user, Some(project), false).await?,
        h.conv(&user, None, false).await?,
    );
    h.item(org, inside, "仅项目苹果").await?;
    h.item(org, outside, "全局苹果").await?;
    let personal = json!({"content":"个人偏好简洁 sk-secret-token",
        "source_conversation_id":outside});
    let saved = h.remember(&user, personal).await?;
    ensure!(
        !saved["content"].as_str().unwrap().contains("sk-secret"),
        "secret persisted"
    );
    h.remember(
        &user,
        json!({"content":"项目使用 Rust","project_id":project,"source_conversation_id":inside}),
    )
    .await?;
    let memory = h.memory(&user, inside).await?;
    ensure!(
        memory.contains("Rust") && !memory.contains("个人偏好"),
        "{memory}"
    );
    let memory = h.memory(&user, outside).await?;
    ensure!(
        memory.contains("个人偏好") && !memory.contains("Rust"),
        "{memory}"
    );
    h.drain(org).await?;
    let q = search::SearchQuery {
        q: "苹果".into(),
        ..Default::default()
    };
    let agent = |project_only| {
        let boundary = search::Boundary {
            agent: true,
            project_only,
        };
        search::retrieve(&h.app, org, owner, &q, boundary, "history")
    };
    let conversation =
        |hits: &[Value]| -> Vec<Uuid> { hits.iter().map(|v| id(v, "conversation_id")).collect() };
    ensure!(conversation(&agent(None).await?) == [outside]);
    ensure!(conversation(&agent(Some(project)).await?) == [inside]);
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn memory_writes_are_idempotent_undoable_and_frozen_per_conversation() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    // The same request key writes once, and only for the same arguments.
    let input = json!({"content":"幂等记忆","request_key":"search-idempotent"});
    let first = h.remember(&user, input.clone()).await?;
    let retry = h.remember(&user, input).await?;
    ensure!(
        first["id"] == retry["id"] && first["event_id"] == retry["event_id"],
        "memory retry wrote twice"
    );
    let mismatch = reqwest::Client::new()
        .post(format!("{}/orgs/{}/memories", h.base, id(&user, "org_id")))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"content":"changed args","request_key":"search-idempotent"}))
        .send()
        .await?;
    ensure!(mismatch.status() == reqwest::StatusCode::CONFLICT);
    let path = |memory: &Value| format!("/memories/{}", memory["id"].as_str().unwrap());
    h.request(&user, Method::DELETE, &path(&first), None)
        .await?;
    // A conversation keeps the memory it started with.
    let saved = h.remember(&user, json!({"content":"个人偏好简洁"})).await?;
    let conv = h.conv(&user, None, false).await?;
    ensure!(h.memory(&user, conv).await?.contains("个人偏好"));
    let later = h
        .remember(&user, json!({"content":"后来加入的偏好"}))
        .await?;
    ensure!(
        !h.memory(&user, conv).await?.contains("后来加入"),
        "snapshot changed mid-conversation"
    );
    let next = h.conv(&user, None, false).await?;
    ensure!(h.memory(&user, next).await?.contains("后来加入"));
    // Edits and deletions are undone.
    let edited = h
        .remember(
            &user,
            json!({"content":"个人偏好详细","replace":saved["id"]}),
        )
        .await?;
    h.undo(&user, &edited["event_id"]).await?;
    let deleted = h
        .request(&user, Method::DELETE, &path(&later), None)
        .await?;
    h.undo(&user, &deleted["event_id"]).await?;
    let list = h.request(&user, Method::GET, "/memories", None).await?;
    let mut contents: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    contents.sort();
    ensure!(contents == ["个人偏好简洁", "后来加入的偏好"], "{list}");
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn digests_go_through_the_gateway_and_members_can_turn_history_and_memory_off()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let (org, owner) = (id(&user, "org_id"), id(&user, "user_id"));
    let conv = h.conv(&user, None, false).await?;
    h.item(org, conv, "我喜欢苹果").await?;
    // Digests are summarized from the member's messages through the model gateway.
    ensure!(memory::digest(&h.app, org, conv, owner).await?, "no digest");
    let gateway: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM usage_records WHERE org_id=$1 AND model='fixture-model'
           AND run_id IS NULL AND upstream_status=200)
         AND EXISTS(SELECT 1 FROM model_rate_limits WHERE org_id=$1 AND scope='org')",
    )
    .bind(org)
    .fetch_one(&h.db)
    .await?;
    ensure!(gateway, "digest summary bypassed the model gateway");
    h.remember(&user, json!({"content":"个人偏好简洁"})).await?;
    let with_history = h.conv(&user, None, false).await?;
    ensure!(h.memory(&user, with_history).await?.contains("<recent>"));
    let preferences = |enabled: bool| {
        let body = json!({"enabled":enabled,"reference_history":false});
        h.request(&user, Method::PATCH, "/memories/preferences", Some(body))
    };
    preferences(true).await?;
    let without_history = h.conv(&user, None, false).await?;
    let memory = h.memory(&user, without_history).await?;
    ensure!(
        memory.contains("个人偏好") && !memory.contains("<recent>"),
        "{memory}"
    );
    let frozen: Value =
        sqlx::query_scalar("SELECT memory_snapshot FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(without_history)
            .fetch_one(&h.db)
            .await?;
    ensure!(
        frozen["recent"] == json!([]),
        "history kept in the snapshot"
    );
    // Memory turned off empties even a conversation's frozen memory.
    preferences(false).await?;
    ensure!(h.memory(&user, with_history).await?.is_empty());
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn deleted_projects_and_conversations_leave_search_memory_and_snapshots() -> anyhow::Result<()>
{
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let project = json!({"name":"删除项目"});
    let project = id(
        &h.request(&user, Method::POST, "/projects", Some(project))
            .await?,
        "id",
    );
    let inside = h.conv(&user, Some(project), false).await?;
    let filed = h.item(org, inside, "仅项目苹果").await?;
    let conv = h.conv(&user, None, false).await?;
    let said = h.item(org, conv, "将被删除的苹果").await?;
    h.remember(
        &user,
        json!({"content":"来自该对话的记忆","source_conversation_id":conv}),
    )
    .await?;
    h.drain(org).await?;
    ensure!(found(&h.hits(&user, "仅项目苹果").await?, filed));
    let path = format!("/projects/{project}");
    h.request(&user, Method::DELETE, &path, None).await?;
    ensure!(
        h.hits(&user, "仅项目苹果").await?.is_empty(),
        "deleted project still searchable"
    );

    // Other conversations' snapshots may have copied its history.
    let (referencing, unrelated) = (
        h.conv(&user, None, false).await?,
        h.conv(&user, None, false).await?,
    );
    let copied = json!({"saved":[],"recent":[{"id":conv,"title":"copied deleted title",
        "points":"copied deleted points"}]});
    for (target, snapshot) in [
        (referencing, copied),
        (unrelated, json!({"saved":[],"recent":[]})),
    ] {
        sqlx::query("UPDATE conversations SET memory_snapshot=$3 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(target)
            .bind(snapshot)
            .execute(&h.db)
            .await?;
    }
    ensure!(found(&h.hits(&user, "将被删除").await?, said));
    let path = format!("/conversations/{conv}?delete_memories=true");
    h.request(&user, Method::DELETE, &path, None).await?;
    ensure!(
        h.hits(&user, "将被删除").await?.is_empty(),
        "deleted chat still searchable"
    );
    // Collecting copied history passes over conversations held by someone else.
    let mut lock = h.db.begin().await?;
    sqlx::query("SELECT id FROM conversations WHERE org_id=$1 AND id=$2 FOR UPDATE")
        .bind(org)
        .bind(unrelated)
        .execute(&mut *lock)
        .await?;
    let mut cleanup = h.app.store.tx(org).await?;
    tokio::time::timeout(
        Duration::from_secs(2),
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
    ensure!(recent == json!([]), "copied history survived collection");
    let memories: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_items
         WHERE org_id=$1 AND source_conversation_id=$2 AND status='active'",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    ensure!(
        memories == 0,
        "associated memories survived requested deletion"
    );
    ensure!(ryzhub_server::conversation_commands::collect(&h.app, org, conv).await?);
    let payloads: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items WHERE org_id=$1 AND conversation_id=$2 AND text<>''",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    ensure!(payloads == 0, "deleted payloads retained");
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn background_workers_index_and_digest_with_the_worker_role() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let conv = h.conv(&user, None, false).await?;
    h.item(org, conv, "后台自动检索摘要").await?;
    let worker = PgPool::connect(&h.worker_url).await?;
    let digests = tokio::spawn(memory::collector(h.app.clone(), worker.clone()));
    let index = tokio::spawn(search::worker(h.app.clone(), worker.clone()));
    let observed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let digested: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM conversation_digests
                   WHERE org_id=$1 AND conversation_id=$2)",
            )
            .bind(org)
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
            if digested && !h.hits(&user, "后台自动").await?.is_empty() {
                return anyhow::Ok(());
            }
            ensure!(
                !digests.is_finished() && !index.is_finished(),
                "a background worker stopped"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    digests.abort();
    index.abort();
    worker.close().await;
    observed??;
    h.close().await
}

#[tokio::test]
#[ignore = "requires search-enabled PostgreSQL and NATS; scripts/test-search.sh"]
async fn work_saves_and_uses_memory_through_the_mcp_gateway() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let conv = h.conv(&user, None, false).await?;
    let lease = h
        .app
        .store
        .claim(org, conv, Uuid::now_v7(), 300_000)
        .await?
        .context("lease")?;
    let mut tx = h.app.store.fenced(&lease).await?;
    let run = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO runs(id,org_id,conversation_id,harness,model,exec_epoch,state,response_text,
           assistant_item_id,cause_cid)
         VALUES($1,$2,$3,'codex','fixture-model',$4,'running','',$5,$6)",
    )
    .bind(run)
    .bind(org)
    .bind(conv)
    .bind(lease.epoch)
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(&mut *tx)
    .await?;
    let sandbox = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,
           handle,persistent,node_lease_until)
         VALUES($1,$2,$3,$4,1,$5,'running','{}',true,now()+interval '5 minutes')",
    )
    .bind(sandbox)
    .bind(org)
    .bind(conv)
    .bind(run)
    .bind(lease.epoch)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let key = ryzhub_server::llm::keys::issue_sandbox_key(
        &h.app.store,
        ryzhub_server::llm::keys::SandboxKeyRequest {
            org_id: org,
            user_id: id(&user, "user_id"),
            conversation_id: conv,
            run_id: run,
            sandbox_id: sandbox,
            lease_epoch: 1,
            models: vec!["fixture-model".into()],
        },
    )
    .await?;
    let call = |body: &Value| {
        let request = reqwest::Client::new()
            .post(format!("{}/mcp", h.gateway))
            .bearer_auth(&key.token)
            .json(body);
        async move { anyhow::Ok(request.send().await?.json::<Value>().await?) }
    };
    let content = "通过 MCP 保存的显式记忆";
    let mut save = json!({"jsonrpc":"2.0","id":"mcp-save","method":"tools/call",
        "params":{"name":"memory_save","arguments":{"content":content}}});
    // A retry of the same request saves nothing more.
    for _ in 0..2 {
        let v = call(&save).await?;
        ensure!(v.get("error").is_none(), "MCP memory call failed: {v}");
    }
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_items WHERE org_id=$1 AND content=$2")
            .bind(org)
            .bind(content)
            .fetch_one(&h.db)
            .await?;
    ensure!(n == 1, "MCP retries duplicated memory writes");
    let mut tx = h.app.store.fenced(&lease).await?;
    memory::project(&mut tx, &lease).await?;
    let instructions =
        ryzhub_server::projects::instructions(&mut tx, &lease, run, &h.app.config()).await?;
    ensure!(
        instructions.contains(content),
        "Work memory injection missing"
    );
    tx.commit().await?;
    // The timeline says what was saved and used, and links to it rather than copying it.
    let notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items WHERE org_id=$1 AND conversation_id=$2
           AND kind IN ('memory_notice','memory_context')",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    ensure!(notices == 2, "memory write/use timeline hints missing");
    h.drain(org).await?;
    let (context_id, persisted): (Uuid, String) = sqlx::query_as(
        "SELECT id,text FROM items WHERE org_id=$1
         AND conversation_id=$2 AND kind='memory_context'",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    ensure!(
        !persisted.contains("显式记忆"),
        "timeline copied memory content"
    );
    let context_path = format!("/conversations/{conv}/memory-context/{context_id}");
    let details = h.request(&user, Method::GET, &context_path, None).await?;
    let memory_id = id(
        details["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["content"] == content)
            .context("timeline memory details missing")?,
        "id",
    );
    let path = format!("/memories/{memory_id}");
    h.request(&user, Method::DELETE, &path, None).await?;
    let details = h.request(&user, Method::GET, &context_path, None).await?;
    ensure!(
        !details["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == json!(memory_id)),
        "deleted memory visible in timeline"
    );
    // Past the undo window, nothing of it is kept.
    sqlx::query(
        "UPDATE memory_items SET invalid_at=now()-interval '31 days' WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(memory_id)
    .execute(&h.db)
    .await?;
    let mut tx = h.app.store.tx(org).await?;
    memory::collect_stale_memories(&mut tx, org).await?;
    tx.commit().await?;
    let retained: bool = sqlx::query_scalar(
        "SELECT content<>''
           OR EXISTS(SELECT 1 FROM memory_events WHERE org_id=$1 AND memory_id=$2)
           OR EXISTS(SELECT 1 FROM search_docs WHERE org_id=$1 AND id=$2)
         FROM memory_items WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(memory_id)
    .fetch_one(&h.db)
    .await?;
    ensure!(!retained, "memory survived the undo window");
    // A temporary Work conversation saves no memory.
    sqlx::query("UPDATE conversations SET temporary=true WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(conv)
        .execute(&h.db)
        .await?;
    save["id"] = json!("mcp-denied");
    let v = call(&save).await?;
    ensure!(
        v["error"]["message"] == "POLICY_DENIED",
        "temporary Work memory write allowed: {v}"
    );
    h.close().await
}
