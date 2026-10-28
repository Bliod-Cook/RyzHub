//! The instance admin API. Requires the local PostgreSQL 18 + NATS services:
//! scripts/test-integration.sh.
mod common;
use common::{Harness, id};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

/// `method path` as `user`, with its status and JSON body.
async fn call(
    h: &Harness,
    user: &Value,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", h.base))
        .bearer_auth(user["token"].as_str().unwrap());
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
    let status = response.status();
    Ok((status, response.json().await.unwrap_or(Value::Null)))
}

/// `PATCH /admin/users/{target}` as `actor`.
async fn set_status(
    h: &Harness,
    actor: &Value,
    target: &Value,
    status: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let path = format!("/admin/users/{}", id(target, "user_id"));
    call(
        h,
        actor,
        Method::PATCH,
        &path,
        Some(json!({"status": status})),
    )
    .await
}

/// `PUT /admin/config/{section}` as `admin`, on the file's `version`.
async fn put_config(
    h: &Harness,
    admin: &Value,
    section: &str,
    values: Value,
    version: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let body = json!({"version": version, "values": values});
    let path = format!("/admin/config/{section}");
    call(h, admin, Method::PUT, &path, Some(body)).await
}

/// A new user the database makes an instance admin.
async fn admin(h: &Harness) -> anyhow::Result<Value> {
    let user = h.user().await?;
    sqlx::query("UPDATE users SET is_instance_admin=true WHERE id=$1")
        .bind(id(&user, "user_id"))
        .execute(&h.db)
        .await?;
    Ok(user)
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn the_admin_api_serves_instance_admins_only() -> anyhow::Result<()> {
    let h = Harness::start(|_| {}).await?;
    let member = h.user().await?;
    let admin = admin(&h).await?;
    for path in ["/admin/config", "/admin/plugin-blocks"] {
        let (status, _) = call(&h, &member, Method::GET, path, None).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        let (status, _) = call(&h, &admin, Method::GET, path, None).await?;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    // Changes are audited in the admin's personal organization.
    let body = json!({"name": "acme", "reason": "test"});
    let (_, block) = call(&h, &admin, Method::POST, "/admin/plugin-blocks", Some(body)).await?;
    let audited: (uuid::Uuid, String, Value) = sqlx::query_as(
        "SELECT org_id,actor_kind,detail FROM audit_logs WHERE action='admin.plugin.block'",
    )
    .fetch_one(&h.db)
    .await?;
    assert_eq!(audited.0, id(&admin, "org_id"));
    assert_eq!(audited.1, "instance_admin");
    assert_eq!(audited.2["name"], "acme");
    assert!(block["id"].is_string());
    // The static page that took a pasted token is gone.
    let page = reqwest::get(format!("{}/admin", h.base)).await?;
    assert_eq!(page.status(), StatusCode::NOT_FOUND);
    h.close().await
}

/// A usage record of `user` in `org`, `ago` before now.
async fn usage(
    h: &Harness,
    user: &Value,
    model: &str,
    tokens: (i64, i64),
    status: &str,
    estimated: bool,
    ago: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO usage_records(id,org_id,user_id,model,input_tokens,output_tokens,status,estimated,created_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,now()-$9::interval)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(id(user, "org_id"))
    .bind(id(user, "user_id"))
    .bind(model)
    .bind(tokens.0)
    .bind(tokens.1)
    .bind(status)
    .bind(estimated)
    .bind(ago)
    .execute(&h.db)
    .await?;
    Ok(())
}

/// A run in a new conversation of `user`'s, in `state`.
async fn run(
    h: &Harness,
    user: &Value,
    mode: &str,
    harness: &str,
    state: &str,
) -> anyhow::Result<()> {
    let ctx = h.context(user).await?;
    let conv = h.create(&ctx, "stats", "chat", json!({}), None).await?.id;
    sqlx::query("UPDATE conversations SET mode=$2 WHERE id=$1")
        .bind(conv)
        .bind(mode)
        .execute(&h.db)
        .await?;
    sqlx::query(
        "INSERT INTO runs(id,org_id,conversation_id,harness,model,exec_epoch,state,response_text,
                          assistant_item_id,cause_cid)
         VALUES($1,$2,$3,$4,'m',1,$5,'',$1,$1)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(ctx.org_id)
    .bind(conv)
    .bind(harness)
    .bind(state)
    .execute(&h.db)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn the_overview_aggregates_the_whole_instance_by_the_viewers_days() -> anyhow::Result<()> {
    let h = Harness::start(|_| {}).await?;
    let admin = admin(&h).await?;
    let (heavy, light, idle) = (h.user().await?, h.user().await?, h.user().await?);
    sqlx::query("UPDATE users SET status='disabled' WHERE id=$1")
        .bind(id(&idle, "user_id"))
        .execute(&h.db)
        .await?;
    usage(&h, &heavy, "big", (1000, 500), "completed", false, "1 hour").await?;
    usage(&h, &heavy, "big", (100, 0), "interrupted", true, "2 days").await?;
    usage(
        &h,
        &light,
        "small",
        (10, 5),
        "upstream_error",
        false,
        "3 days",
    )
    .await?;
    // Outside the week, inside the quarter.
    usage(&h, &light, "small", (7, 7), "completed", false, "40 days").await?;
    run(&h, &heavy, "chat", "chat", "completed").await?;
    run(&h, &heavy, "work", "codex", "running").await?;
    run(&h, &light, "work", "claude", "failed").await?;

    let get = |days: u32| {
        let (h, admin) = (&h, &admin);
        async move {
            let path = format!("/admin/stats?days={days}&tz_offset=480");
            let (status, body) = call(h, admin, Method::GET, &path, None).await?;
            assert_eq!(status, StatusCode::OK, "{body}");
            anyhow::Ok(body)
        }
    };
    let week = get(7).await?;
    assert_eq!(
        week["users"],
        json!({"total": 4, "disabled": 1, "deletion_pending": 0})
    );
    assert_eq!(week["model_users"], json!({"week": 2, "month": 2}));
    assert_eq!(week["conversations"], json!({"chat": 1, "work": 2}));
    assert_eq!(week["runs"], json!({"running": 1, "requires_action": 0}));
    assert_eq!(
        week["run_outcomes"],
        json!({"completed": 1, "failed": 1, "cancelled": 0})
    );
    assert_eq!(
        week["request_statuses"],
        json!({"completed": 1, "interrupted": 1, "upstream_error": 1})
    );
    assert_eq!(week["models"][0]["model"], "big");
    assert_eq!(week["models"][0]["input_tokens"], 1100);
    assert_eq!(week["harnesses"].as_array().unwrap().len(), 3);
    assert_eq!(week["top_users"][0]["email"], heavy["email"]);
    assert_eq!(week["top_users"].as_array().unwrap().len(), 2);
    assert_eq!(
        (
            week["estimated_tokens"].clone(),
            week["total_tokens"].clone()
        ),
        (json!(100), json!(1615))
    );
    // One entry per day, which together hold the whole period.
    let daily = week["daily"].as_array().unwrap();
    assert_eq!(daily.len(), 7);
    let sum = |key: &str| daily.iter().map(|d| d[key].as_i64().unwrap()).sum::<i64>();
    assert_eq!((sum("input_tokens"), sum("output_tokens")), (1110, 505));
    assert_eq!(
        (sum("chat_runs"), sum("work_runs"), sum("new_users")),
        (1, 2, 4)
    );
    assert_eq!(daily[0]["day"], week["first_day"]);
    // Days are the viewer's: UTC+8 here.
    let day = (chrono::Utc::now() - chrono::Duration::hours(1) + chrono::Duration::hours(8))
        .date_naive()
        .to_string();
    let hour_ago = daily.iter().find(|d| d["day"] == day.as_str()).unwrap();
    assert!(hour_ago["input_tokens"].as_i64().unwrap() >= 1000);
    // The migrated default node takes any number of sandboxes.
    assert_eq!(week["nodes"], json!({"active": 1}));
    assert_eq!(week["node_capacity"], Value::Null);
    assert_eq!(week["retention"]["audit_days"], 400);

    let quarter = get(90).await?;
    assert_eq!(quarter["daily"].as_array().unwrap().len(), 90);
    assert_eq!(quarter["total_tokens"], 1629);
    let (status, _) = call(
        &h,
        &admin,
        Method::GET,
        "/admin/stats?days=8&tz_offset=0",
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    h.close().await
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn users_are_listed_newest_first_by_page_search_and_status() -> anyhow::Result<()> {
    let h = Harness::start(|_| {}).await?;
    let admin = admin(&h).await?;
    let member = h.user().await?;
    sqlx::query(
        "INSERT INTO users(id,email,display_name,password_hash)
         SELECT uuidv7(),'bulk'||n||'@example.test','Bulk '||n,'!' FROM generate_series(1,60) n",
    )
    .execute(&h.db)
    .await?;
    sqlx::query("UPDATE users SET status='disabled' WHERE email='bulk7@example.test'")
        .execute(&h.db)
        .await?;
    let list = |query: &str| {
        let path = format!("/admin/users?{query}");
        let (h, admin) = (&h, &admin);
        async move {
            let (status, body) = call(h, admin, Method::GET, &path, None).await?;
            assert_eq!(status, StatusCode::OK, "{body}");
            anyhow::Ok(body)
        }
    };
    let first = list("").await?;
    let page = first["users"].as_array().unwrap();
    assert_eq!(page.len(), 50);
    assert_eq!(page[0]["email"], "bulk60@example.test");
    let second = list(&format!("before={}", first["next"].as_str().unwrap())).await?;
    assert_eq!(second["users"].as_array().unwrap().len(), 12);
    assert_eq!(second["next"], Value::Null);
    let found = list("q=BULK7").await?;
    assert_eq!(found["users"].as_array().unwrap().len(), 1);
    let disabled = list("status=disabled").await?;
    assert_eq!(disabled["users"][0]["email"], "bulk7@example.test");
    // One account in detail: how it signs in, and what it has.
    let path = format!("/admin/users/{}", id(&member, "user_id"));
    let (_, detail) = call(&h, &admin, Method::GET, &path, None).await?;
    assert_eq!(detail["email"], member["email"]);
    assert_eq!(detail["status"], "active");
    assert_eq!(detail["password"], true);
    assert_eq!(detail["providers"], json!([]));
    assert!(detail["created_at_ms"].as_i64().unwrap() > 0);
    assert!(detail["last_sign_in_ms"].as_i64().unwrap() > 0);
    assert_eq!(detail["devices"], 1);
    assert_eq!(detail["conversations"], 0);
    assert_eq!(detail["tokens_30d"], json!({"input": 0, "output": 0}));
    let path = format!("/admin/users/{}", uuid::Uuid::now_v7());
    let (status, _) = call(&h, &admin, Method::GET, &path, None).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    h.close().await
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn admins_never_change_themselves_and_two_disabling_each_other_leave_one()
-> anyhow::Result<()> {
    let h = Harness::start(|_| {}).await?;
    let (first, second) = (admin(&h).await?, admin(&h).await?);
    let (status, body) = set_status(&h, &first, &first, "disabled").await?;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("CANNOT_CHANGE_SELF"))
    );
    let (one, other) = tokio::join!(
        set_status(&h, &first, &second, "disabled"),
        set_status(&h, &second, &first, "disabled")
    );
    let succeeded = [one?.0, other?.0]
        .iter()
        .filter(|status| status.is_success())
        .count();
    assert_eq!(succeeded, 1);
    let admins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE is_instance_admin AND status='active'",
    )
    .fetch_one(&h.db)
    .await?;
    assert_eq!(admins, 1);
    h.close().await
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn a_sign_in_racing_a_disable_never_outlives_it() -> anyhow::Result<()> {
    let h = Harness::start(|_| {}).await?;
    let admin = admin(&h).await?;
    let http = reqwest::Client::new();
    for _ in 0..5 {
        let member = h.user().await?;
        let user = id(&member, "user_id");
        let logins = (0..4).map(|_| {
            http.post(format!("{}/auth/login", h.base))
                .json(&json!({"email": member["email"], "password": "test-password"}))
                .send()
        });
        let disable = set_status(&h, &admin, &member, "disabled");
        let (_, disabled) = tokio::join!(futures_util::future::join_all(logins), disable);
        assert!(disabled?.0.is_success());
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auth_sessions WHERE user_id=$1 AND revoked_at IS NULL",
        )
        .bind(user)
        .fetch_one(&h.db)
        .await?;
        assert_eq!(live, 0);
    }
    h.close().await
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn a_deleted_account_restores_and_once_due_is_purged_to_tombstones() -> anyhow::Result<()> {
    use ryzhub_server::{accounts, conversation_commands, projects};
    let h = Harness::start(|config| config.llm.enabled = true).await?;
    let worker = sqlx::PgPool::connect(&h.worker_url).await?;
    let admin = admin(&h).await?;
    let member = h.user().await?;
    let (user, org) = (id(&member, "user_id"), id(&member, "org_id"));
    // Something of everything the account keeps.
    let ctx = h.context(&member).await?;
    let conv = h
        .create(&ctx, "secret plans", "chat", json!({}), None)
        .await?
        .id;
    let mut owner = h.owner()?;
    h.send(&member, conv, common::message("hello")).await?;
    h.wait_state(conv, "completed").await?;
    owner.kill().await?;
    owner.wait().await?;
    let base = format!("/orgs/{org}");
    let body = json!({"name": "secret project"});
    let (_, project) = call(
        &h,
        &member,
        Method::POST,
        &format!("{base}/projects"),
        Some(body),
    )
    .await?;
    let project = uuid::Uuid::parse_str(project["id"].as_str().unwrap())?;
    let body = json!({"content": "secret memory"});
    let (status, _) = call(
        &h,
        &member,
        Method::POST,
        &format!("{base}/memories"),
        Some(body),
    )
    .await?;
    assert!(status.is_success());
    let file = reqwest::Client::new()
        .post(format!("{}/orgs/{org}/attachments?name=secret.txt", h.base))
        .bearer_auth(member["token"].as_str().unwrap())
        .body("secret file")
        .send()
        .await?;
    assert!(file.status().is_success());
    let secret = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO secrets(id,org_id,ciphertext,wrapped_dek,kek_id) VALUES($1,$2,'x','x',0)",
    )
    .bind(secret)
    .bind(org)
    .execute(&h.db)
    .await?;
    sqlx::query(
        "INSERT INTO model_credentials(id,org_id,owner_user_id,name,provider,secret_id)
         VALUES($1,$2,$3,'secret key','openai',$4)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(org)
    .bind(user)
    .bind(secret)
    .execute(&h.db)
    .await?;

    // Deleted, the account is disabled and can be restored, disabled.
    let path = format!("/admin/users/{user}");
    let (_, deleted) = call(&h, &admin, Method::DELETE, &path, None).await?;
    assert_eq!(deleted["status"], "deletion_pending");
    let days = (deleted["delete_after_ms"].as_i64().unwrap()
        - chrono::Utc::now().timestamp_millis())
        / 86_400_000;
    assert_eq!(days, 29);
    let login = json!({"email": member["email"], "password": "test-password"});
    let refused: Value = reqwest::Client::new()
        .post(format!("{}/auth/login", h.base))
        .json(&login)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(refused["error"], "ACCOUNT_DISABLED");
    let restore = format!("{path}/restore");
    let (_, restored) = call(&h, &admin, Method::POST, &restore, None).await?;
    assert_eq!(
        (
            restored["status"].clone(),
            restored["delete_after_ms"].clone()
        ),
        (json!("disabled"), Value::Null)
    );
    call(&h, &admin, Method::DELETE, &path, None).await?;
    sqlx::query("UPDATE users SET delete_after=now()-interval '1 second' WHERE id=$1")
        .bind(user)
        .execute(&h.db)
        .await?;

    // Due, it is anonymized at once and its organization emptied over rounds.
    accounts::expire(&h.app, &worker, user).await?;
    let (_, gone) = call(&h, &admin, Method::GET, &path, None).await?;
    assert_eq!(gone["email"], format!("deleted+{user}@invalid"));
    assert_eq!(
        (gone["status"].clone(), gone["purging"].clone()),
        (json!("deleted"), json!(true))
    );
    assert!(
        !accounts::purge(&h.app, org).await?,
        "conversations wait for their collector"
    );
    assert!(conversation_commands::collect(&h.app, org, conv).await?);
    assert!(projects::collect(&h.app, org, project).await?);
    // A failed round leaves the organization to the next one.
    let files = std::path::Path::new(&h.app.config().work.storage_root)
        .join(org.to_string())
        .join("attachments");
    let mode =
        |bits| std::fs::set_permissions(&files, std::os::unix::fs::PermissionsExt::from_mode(bits));
    mode(0o500)?;
    let failed = accounts::purge(&h.app, org).await;
    mode(0o700)?;
    assert!(failed.is_err());
    assert!(accounts::purge(&h.app, org).await?);

    let state: String = sqlx::query_scalar("SELECT state FROM organizations WHERE id=$1")
        .bind(org)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(state, "deleted");
    assert!(!files.parent().unwrap().exists(), "stored objects remain");
    let left: Vec<(String, i64)> = sqlx::query_as(
        "SELECT 'conversation',count(*) FROM conversations WHERE org_id=$1 AND (state<>'deleted' OR title<>'')
         UNION ALL SELECT 'item',count(*) FROM items WHERE org_id=$1 AND text<>''
         UNION ALL SELECT 'project',count(*) FROM projects WHERE org_id=$1 AND (state<>'deleted' OR name<>'')
         UNION ALL SELECT 'memory',count(*) FROM memory_items WHERE org_id=$1 AND content<>''
         UNION ALL SELECT 'attachment',count(*) FROM attachments WHERE org_id=$1
         UNION ALL SELECT 'credential',count(*) FROM model_credentials
             WHERE org_id=$1 AND (revoked_at IS NULL OR name<>'' OR secret_id IS NOT NULL)
         UNION ALL SELECT 'secret',count(*) FROM secrets WHERE org_id=$1
         UNION ALL SELECT 'search',count(*) FROM search_docs WHERE org_id=$1
         UNION ALL SELECT 'event',count(*) FROM events WHERE org_id=$1
         UNION ALL SELECT 'identity',count(*) FROM user_identities WHERE user_id=$2",
    )
    .bind(org)
    .bind(user)
    .fetch_all(&h.db)
    .await?;
    assert!(left.iter().all(|(_, n)| *n == 0), "{left:?}");
    // Usage keeps the id; the email is free again.
    let usage: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE user_id=$1")
        .bind(user)
        .fetch_one(&h.db)
        .await?;
    assert!(usage > 0);
    let again = reqwest::Client::new()
        .post(format!("{}/auth/register", h.base))
        .json(&login)
        .send()
        .await?;
    assert!(again.status().is_success());
    worker.close().await;
    h.close().await
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn a_running_gateway_follows_its_edited_file_within_seconds() -> anyhow::Result<()> {
    let h = Harness::start(|_| {}).await?;
    let reserve = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = reserve.local_addr()?;
    drop(reserve);
    let file = tempfile::NamedTempFile::new()?;
    let write = |origin: &str| {
        std::fs::write(
            file.path(),
            format!("listen = \"{addr}\"\nallowed_origins = [\"{origin}\"]\n"),
        )
    };
    write("https://old.test")?;
    let mut gateway = tokio::process::Command::new(env!("CARGO_BIN_EXE_ryzhub-server"))
        .arg("gateway")
        .env("DATABASE_URL", &h.app_url)
        .env("NATS_URL", "nats://127.0.0.1:54222")
        .env("RYZHUB_CONFIG", file.path())
        .kill_on_drop(true)
        .spawn()?;
    let base = format!("http://{addr}");
    let allows = |origin: &'static str| {
        reqwest::Client::new()
            .request(Method::OPTIONS, format!("{base}/me"))
            .header("origin", origin)
            .header("access-control-request-method", "GET")
            .send()
    };
    let mut started = false;
    for _ in 0..200 {
        started = reqwest::get(format!("{base}/healthz")).await.is_ok();
        if started {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(started, "gateway startup timeout");
    let allowed = |response: reqwest::Response| {
        response
            .headers()
            .contains_key("access-control-allow-origin")
    };
    assert!(allowed(allows("https://old.test").await?));
    assert!(!allowed(allows("https://new.test").await?));
    write("https://new.test")?;
    let edited = std::time::Instant::now();
    while !allowed(allows("https://new.test").await?) {
        assert!(edited.elapsed().as_secs() < 6, "the edit did not apply");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!allowed(allows("https://old.test").await?));
    gateway.kill().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn the_panel_edits_its_file_in_place_one_version_at_a_time() -> anyhow::Result<()> {
    let h = Harness::start(|_| {}).await?;
    let admin = admin(&h).await?;
    let path = h.app.running.path.clone().unwrap();
    let text = std::fs::read_to_string(&path)?;
    std::fs::write(&path, format!("# Kept by every edit.\n{text}"))?;
    let (_, config) = call(&h, &admin, Method::GET, "/admin/config", None).await?;
    assert_eq!(config["editable"], true);
    assert_eq!(
        config["fields"]["llm.model"],
        json!({"section": "models", "apply": "new", "editable": true})
    );
    assert_eq!(config["fields"]["listen"]["editable"], false);
    assert_eq!(config["env"]["OPENAI_API_KEY"], false);
    let version = config["version"].as_str().unwrap().to_owned();
    let refused = |result: (StatusCode, Value)| result.1["error"].as_str().map(str::to_owned);
    // Only what the panel may change, in its own section, with keys from known variables.
    let locked = Some("CONFIG_FIELD_LOCKED".to_owned());
    assert_eq!(
        refused(
            put_config(
                &h,
                &admin,
                "general",
                json!({"listen": "0.0.0.0:1"}),
                &version
            )
            .await?
        ),
        locked
    );
    assert_eq!(
        refused(put_config(&h, &admin, "general", json!({"llm.model": "x"}), &version).await?),
        locked
    );
    assert_eq!(
        refused(put_config(&h, &admin, "models", json!({"llm.models.0": {}}), &version).await?),
        locked
    );
    let foreign = json!({"llm.platform_key_env": "RYZHUB_TEST_VAULT_KEY"});
    assert_eq!(
        refused(put_config(&h, &admin, "models", foreign, &version).await?).as_deref(),
        Some("ENV_NOT_ALLOWED")
    );
    let invalid = put_config(
        &h,
        &admin,
        "general",
        json!({"approval_ttl_secs": 1}),
        &version,
    )
    .await?;
    assert_eq!(invalid.1["error"], "INVALID_CONFIG");
    assert!(
        invalid.1["message"]
            .as_str()
            .unwrap()
            .contains("approval_ttl_secs")
    );
    // Two edits of one version: the second finds the file changed.
    let (first, second) = tokio::join!(
        put_config(
            &h,
            &admin,
            "general",
            json!({"approval_ttl_secs": 120}),
            &version
        ),
        put_config(
            &h,
            &admin,
            "general",
            json!({"approval_ttl_secs": 180}),
            &version
        )
    );
    let (first, second) = (first?, second?);
    let conflicts = [&first, &second]
        .iter()
        .filter(|(_, body)| body["error"] == "CONFIG_CHANGED")
        .count();
    assert_eq!(conflicts, 1, "{first:?} {second:?}");
    let edited = std::fs::read_to_string(&path)?;
    assert!(edited.starts_with("# Kept by every edit.\n"));
    assert!(path.with_extension("toml.bak").exists());
    // Applied at once, or listed until a restart.
    assert!([120, 180].contains(&h.app.config().approval_ttl_secs));
    let (_, config) = call(&h, &admin, Method::GET, "/admin/config", None).await?;
    let version = config["version"].as_str().unwrap().to_owned();
    let (status, config) = put_config(
        &h,
        &admin,
        "plugins",
        json!({"plugins.enabled": false}),
        &version,
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["pending_restart"], json!(["plugins.enabled"]));
    assert!(h.app.config().plugins.enabled);
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_logs WHERE action='admin.config.update'")
            .fetch_one(&h.db)
            .await?;
    assert_eq!(audited, 2);
    // An admin cannot shut out the page they edit from.
    let version = config["version"].as_str().unwrap();
    let body = json!({"version": version, "values": {"allowed_origins": ["https://other.test"]}});
    let response = reqwest::Client::new()
        .put(format!("{}/admin/config/general", h.base))
        .bearer_auth(admin["token"].as_str().unwrap())
        .header("origin", "http://localhost:3000")
        .json(&body)
        .send()
        .await?;
    let refused: Value = response.json().await?;
    assert_eq!(refused["error"], "ORIGIN_REQUIRED");
    h.close().await
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn a_process_restarts_by_exiting_for_its_supervisor_to_start_it_again() -> anyhow::Result<()>
{
    let h = Harness::start(|_| {}).await?;
    let admin = admin(&h).await?;
    let reserve = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = reserve.local_addr()?;
    drop(reserve);
    let file = tempfile::NamedTempFile::new()?;
    let text = format!("listen = \"{addr}\"\n[plugins]\nenabled = false\n");
    std::fs::write(file.path(), text)?;
    let mut gateway = tokio::process::Command::new(env!("CARGO_BIN_EXE_ryzhub-server"))
        .arg("gateway")
        .env("DATABASE_URL", &h.app_url)
        .env("NATS_URL", "nats://127.0.0.1:54222")
        .env("RYZHUB_CONFIG", file.path())
        .kill_on_drop(true)
        .spawn()?;
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();
    let mut started = false;
    for _ in 0..200 {
        started = reqwest::get(format!("{base}/healthz")).await.is_ok();
        if started {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(started, "gateway startup timeout");
    let token = admin["token"].as_str().unwrap();
    let components: Value = http
        .get(format!("{base}/admin/components"))
        .bearer_auth(token)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(components["process"]["role"], "gateway");
    assert_eq!(components["process"]["pid"], gateway.id().unwrap());
    let names: Vec<&str> = components["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["api", "config-reload"]);
    assert_eq!(components["nodes"][0]["name"], "default");
    let refused = http
        .post(format!("{base}/admin/components/api/restart"))
        .bearer_auth(token)
        .send()
        .await?;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let restarted = http
        .post(format!("{base}/admin/components/config-reload/restart"))
        .bearer_auth(token)
        .send()
        .await?;
    assert_eq!(restarted.status(), StatusCode::ACCEPTED);
    let restart = http
        .post(format!("{base}/admin/process/restart"))
        .bearer_auth(token)
        .send()
        .await?;
    assert_eq!(restart.status(), StatusCode::ACCEPTED);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), gateway.wait()).await??;
    assert_eq!(status.code(), Some(75));
    h.close().await
}
