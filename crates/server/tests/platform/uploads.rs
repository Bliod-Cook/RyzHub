//! Library files sent in parts: out of order, resumed, replaced, checked, completed once.
use super::*;
use sha2::{Digest, Sha256};

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS"]
async fn large_files_upload_in_parts_that_resume_and_complete_once() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let token = user["token"].as_str().unwrap().to_owned();
    let client = reqwest::Client::new();
    let base = format!("{}/orgs/{org}/uploads", h.base);
    let capabilities: Value = client
        .get(format!("{}/capabilities", h.base))
        .send()
        .await?
        .json()
        .await?;
    let part = capabilities["attachments"]["part_bytes"].as_u64().unwrap() as usize;
    let file: Vec<u8> = (0..2 * part + 1234).map(|i| (i * 7 % 251) as u8).collect();
    let digest = format!("{:x}", Sha256::digest(&file));

    let started: Value = client
        .post(&base)
        .bearer_auth(&token)
        .json(&json!({"name":"big.bin","size":file.len()}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        (started["parts"].as_u64(), started["direct"].as_bool()),
        (Some(3), Some(false))
    );
    let upload = id(&started, "id");
    let put = |n: usize, body: Vec<u8>| {
        client
            .put(format!("{base}/{upload}/parts/{n}"))
            .bearer_auth(&token)
            .body(body)
            .send()
    };
    let chunk = |n: usize| file[(n - 1) * part..(n * part).min(file.len())].to_vec();

    // Out of order, a wrong size refused, a part sent again replaced.
    put(2, chunk(2)).await?.error_for_status()?;
    let short = put(3, b"short".to_vec()).await?;
    assert_eq!(short.status(), 400);
    put(1, vec![0; part]).await?.error_for_status()?;
    put(1, chunk(1)).await?.error_for_status()?;
    let early = client
        .post(format!("{base}/{upload}/complete"))
        .bearer_auth(&token)
        .send()
        .await?;
    assert_eq!(early.status(), 409);
    assert_eq!(early.json::<Value>().await?["error"], "UPLOAD_INCOMPLETE");

    // Another member does not see it; its owner resumes from what arrived.
    let other = h.user().await?;
    let foreign = client
        .get(format!(
            "{}/orgs/{}/uploads/{upload}",
            h.base,
            id(&other, "org_id")
        ))
        .bearer_auth(other["token"].as_str().unwrap())
        .send()
        .await?;
    assert!(!foreign.status().is_success());
    let resumed: Value = client
        .get(format!("{base}/{upload}"))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(resumed["received"], json!([1, 2]));
    put(3, chunk(3)).await?.error_for_status()?;
    let completed: Value = client
        .post(format!("{base}/{upload}/complete"))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(completed["sha256"], digest);
    assert_eq!(completed["size"].as_u64(), Some(file.len() as u64));
    let again: Value = client
        .post(format!("{base}/{upload}/complete"))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        again["id"], completed["id"],
        "completing again returns the same file"
    );
    let content = client
        .get(format!(
            "{}/orgs/{org}/attachments/{}/content",
            h.base,
            completed["id"].as_str().unwrap()
        ))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    assert_eq!(content.as_ref(), file.as_slice());
    let parts =
        ryzhub_server::workspace::blob::BlobStore::configured(&h.app.config().work.storage_root)?
            .scoped(org)
            .within("attachments")
            .within("uploads")
            .within(&upload.to_string());
    let first = format!("{:x}", Sha256::digest(chunk(1)));
    assert!(parts.get(&first).await.is_err(), "parts discarded");
    let late = put(3, chunk(3)).await?;
    assert_eq!(late.status(), 409, "closed once completed");

    // An abandoned upload is discarded, and one past its expiry collected.
    let abandoned: Value = client
        .post(&base)
        .bearer_auth(&token)
        .json(&json!({"name":"gone.bin","size":file.len()}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let gone = id(&abandoned, "id");
    client
        .put(format!("{base}/{gone}/parts/1"))
        .bearer_auth(&token)
        .body(chunk(1))
        .send()
        .await?
        .error_for_status()?;
    let parts =
        ryzhub_server::workspace::blob::BlobStore::configured(&h.app.config().work.storage_root)?
            .scoped(org)
            .within("attachments")
            .within("uploads")
            .within(&gone.to_string());
    assert!(parts.get(&first).await.is_ok());
    sqlx::query("UPDATE uploads SET expires_at=now()-interval '1 minute' WHERE id=$1")
        .bind(gone)
        .execute(&h.db)
        .await?;
    ryzhub_server::attachments::gc::collect_org(&h.app, org).await?;
    assert!(parts.get(&first).await.is_err());
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM uploads WHERE id=$1")
        .bind(gone)
        .fetch_optional(&h.db)
        .await?;
    assert_eq!(state, None, "forgotten once expired and discarded");
    let too_large = client
        .post(&base)
        .bearer_auth(&token)
        .json(&json!({"name":"huge.bin","size":1u64 << 40}))
        .send()
        .await?;
    assert_eq!(too_large.status(), 413);
    h.close().await
}
