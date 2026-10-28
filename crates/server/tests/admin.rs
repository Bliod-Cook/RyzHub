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
    for path in ["/admin/settings", "/admin/plugin-blocks"] {
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
