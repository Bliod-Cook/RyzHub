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
