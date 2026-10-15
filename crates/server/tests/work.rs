//! Real PostgreSQL, NATS and containerd; only the paid model upstream is a protocol fixture.
mod common;
use anyhow::{Context, ensure};
use common::{Harness, guest_exec, id, message};
use ryzhub_proto::{command_envelope::Command, *};
use ryzhub_server::{llm::keys, owner};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{io::Write, sync::Arc, time::Duration};
use tokio::process::Command as Process;
use uuid::Uuid;

impl Harness {
    async fn new() -> anyhow::Result<Self> {
        Self::start(|config| {
            common::work(config);
            config.work.mcp_tools = vec![ryzhub_server::mcp::Tool {
                name: "write_note".into(),
                description: "Local test tool".into(),
                input_schema: json!({"type":"object","required":["text"],
                    "properties":{"text":{"type":"string"}}}),
                url: format!("{}/mcp", config.llm.upstream_url),
                remote_name: "write_note".into(),
            }];
        })
        .await
    }
    async fn conversation(&self, user: &Value) -> anyhow::Result<Uuid> {
        let ctx = self.context(user).await?;
        Ok(self
            .app
            .store
            .create_conversation(&ctx, "Work", "work")
            .await?
            .id)
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS/containerd and actual Codex binary"]
async fn actual_codex_native_turn_and_checkpoint_restore() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    reqwest::Client::new().post(format!("{}/orgs/{org}/credentials",h.base)).bearer_auth(user["token"].as_str().unwrap()).json(&json!({"name":"Anthropic isolation fixture","provider":"anthropic","api_key":"must-not-reach-openai"})).send().await?.error_for_status()?;
    h.send(&user, conv, message("hello codex")).await?;
    let run = h.wait_state(conv, "completed").await?;
    let text:String=sqlx::query_scalar("SELECT text FROM items WHERE run_id=$1 AND kind='assistant' ORDER BY last_seq DESC LIMIT 1").bind(run).fetch_one(&h.db).await?;
    assert!(text.contains("Codex"), "{text}");
    let usage:(String,i64,i64,bool)=sqlx::query_as("SELECT provider,input_tokens,output_tokens,estimated FROM usage_records WHERE run_id=$1 ORDER BY created_at DESC LIMIT 1").bind(run).fetch_one(&h.db).await?;
    assert_eq!(usage, ("openai".into(), 31, 9, false));
    let key = h
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|(b, _)| b.get("model").is_some())
        .context("NO_MODEL_REQUEST")?
        .1["authorization"]
        .to_str()?
        .to_owned();
    assert_eq!(key, "Bearer local-fixture-key");
    let sandbox: Uuid =
        sqlx::query_scalar("SELECT id FROM sandboxes WHERE run_id=$1 AND persistent")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    let epoch: i64 = sqlx::query_scalar("SELECT exec_epoch FROM sandboxes WHERE id=$1")
        .bind(sandbox)
        .fetch_one(&h.db)
        .await?;
    wait_workspace_idle(&h, conv).await?;
    let host = std::path::Path::new(&h.app.config.sandbox.host_socket);
    use ryzhub_execution::{ExecSpec, GuestCommand, GuestRequest, HostCommand};
    let exec = |cmd: &str, op: &str| HostCommand::Guest {
        request: GuestRequest {
            sandbox_id: sandbox,
            exec_epoch: epoch as u64,
            command: GuestCommand::Exec {
                op_id: op.into(),
                spec: ExecSpec {
                    cmd: cmd.into(),
                    cwd: "/workspace".into(),
                    env: Default::default(),
                    timeout_ms: 10000,
                    background: false,
                },
            },
        },
    };
    ryzhub_execution::call(host,&exec("printf checkpoint > /workspace/untracked; chmod 751 /workspace/untracked; mv /etc/periodic /etc/renamed-periodic; chmod 700 /etc/renamed-periodic", "prepare-checkpoint")).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    async fn job(
        h: &Harness,
        user: &Value,
        org: Uuid,
        conv: Uuid,
        kind: &str,
    ) -> anyhow::Result<()> {
        let mut accepted = None;
        for _ in 0..300 {
            let response = reqwest::Client::new()
                .post(format!(
                    "{}/orgs/{org}/conversations/{conv}/workspace",
                    h.base
                ))
                .bearer_auth(user["token"].as_str().unwrap())
                .json(&json!({"kind":kind}))
                .send()
                .await?;
            let status = response.status();
            let value: Value = response.json().await?;
            // Automatic post-turn checkpointing can win the admission race.
            if status == 409 && value["error"] == "WORKSPACE_BUSY" {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            ensure!(status.is_success(), "workspace API {status}: {value}");
            accepted = Some(value);
            break;
        }
        let v = accepted.context("workspace stayed busy")?;
        let id = Uuid::parse_str(v["id"].as_str().context("JOB_ID")?)?;
        for _ in 0..600 {
            let (state, result): (String, Option<Value>) =
                sqlx::query_as("SELECT state,result FROM workspace_jobs WHERE id=$1")
                    .bind(id)
                    .fetch_one(&h.db)
                    .await?;
            if state == "completed" {
                return Ok(());
            }
            ensure!(state != "failed", "job failed: {result:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("job timeout")
    }
    job(&h, &user, org, conv, "checkpoint").await?;
    ryzhub_execution::call(
        host,
        &exec("printf lost > /workspace/untracked", "post-watermark"),
    )
    .await?;
    // Corrupt the primary object; restore must verify and repair it from secondary storage.
    let manifest:Value=sqlx::query_scalar("SELECT c.manifest FROM checkpoints c JOIN workspace_heads w ON w.checkpoint_id=c.id WHERE w.conversation_id=$1").bind(conv).fetch_one(&h.db).await?;
    let key = manifest["sha256"].as_str().unwrap();
    let blob_path = std::path::Path::new(&h.app.config.work.storage_root)
        .join(org.to_string())
        .join(key);
    let archive = std::fs::read(&blob_path)?;
    let decoded = zstd::decode_all(archive.as_slice())?;
    assert!(
        !decoded
            .windows(b"must-not-reach-openai".len())
            .any(|w| w == b"must-not-reach-openai")
    );
    std::fs::write(&blob_path, b"corrupted object")?;
    job(&h, &user, org, conv, "restore").await?;
    let tasks = Process::new("docker")
        .args([
            "exec",
            "ryzhub-execution",
            "ctr",
            "--address",
            "/run/ryzhub-containerd.sock",
            "--namespace",
            "ryzhub",
            "tasks",
            "list",
        ])
        .output()
        .await?;
    ensure!(
        tasks.status.success(),
        "cannot inspect stopped task reclamation"
    );
    ensure!(
        !String::from_utf8_lossy(&tasks.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some(sandbox.to_string().as_str())),
        "stopped source task must release its shim"
    );
    assert_eq!(std::fs::read(&blob_path)?, archive);
    reqwest::Client::new().post(format!("{}/orgs/{org}/credentials",h.base)).bearer_auth(user["token"].as_str().unwrap()).json(&json!({"name":"OpenAI BYOK fixture","provider":"openai","api_key":"openai-fixture-key"})).send().await?.error_for_status()?;
    let restored: Uuid = sqlx::query_scalar(
        "SELECT id FROM sandboxes WHERE conversation_id=$1 AND persistent AND state='running'",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    let restored_epoch: i64 = sqlx::query_scalar("SELECT exec_epoch FROM sandboxes WHERE id=$1")
        .bind(restored)
        .fetch_one(&h.db)
        .await?;
    let result=guest_exec(host,restored,restored_epoch,"verify-restored","cat /workspace/untracked; stat -c %a /workspace/untracked; test ! -d /etc/periodic; test -d /etc/renamed-periodic; stat -c %a /etc/renamed-periodic").await?;
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout_head"], "checkpoint751\n700\n");
    h.send(&user, conv, message("hello again")).await?;
    for _ in 0..200 {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM runs WHERE conversation_id=$1")
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
        if count == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let second = h.wait_state(conv, "completed").await?;
    assert_ne!(second, run);
    assert!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .any(|(body, _)| body.to_string().contains("平台恢复说明"))
    );
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(v, _)| v.get("model").is_some())
            .unwrap()
            .1["authorization"],
        "Bearer openai-fixture-key"
    );
    let threads: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT native_thread_id) FROM sandboxes WHERE conversation_id=$1",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(threads, 1);
    wait_workspace_idle(&h, conv).await?;
    job(&h, &user, org, conv, "restore").await?;
    let live = ryzhub_execution::call(host, &HostCommand::Status).await?;
    assert!(
        !live["running"]
            .as_array()
            .unwrap()
            .contains(&json!(restored)),
        "repeated restore must stop the current writer"
    );
    owner.kill().await?;
    h.close().await
}
async fn wait_workspace_idle(h: &Harness, conv: Uuid) -> anyhow::Result<()> {
    for _ in 0..600 {
        let ready:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_heads WHERE conversation_id=$1) AND NOT EXISTS(SELECT 1 FROM workspace_jobs WHERE conversation_id=$1 AND state IN ('queued','running'))").bind(conv).fetch_one(&h.db).await?;
        if ready {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("automatic turn checkpoint did not complete")
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS/containerd and actual Codex"]
async fn bypassing_harness_still_requires_one_use_mcp_approval() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    h.send(&user, conv, message("manual gateway test")).await?;
    let lease = h
        .app
        .store
        .claim(org, conv, Uuid::now_v7(), 60_000)
        .await?
        .unwrap();
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    let sandbox = ryzhub_server::work::ensure_work(&h.app, &lease, run).await?;
    // The same kind of key the guest holds, issued directly: the caller bypasses the harness.
    let epoch: i64 = sqlx::query_scalar("SELECT lease_epoch FROM sandboxes WHERE id=$1")
        .bind(sandbox)
        .fetch_one(&h.db)
        .await?;
    let token = keys::issue_sandbox_key(
        &h.app.store,
        keys::SandboxKeyRequest {
            org_id: org,
            user_id: id(&user, "user_id"),
            conversation_id: conv,
            run_id: run,
            sandbox_id: sandbox,
            lease_epoch: epoch,
            models: vec![h.app.config.work.model.clone()],
            credential_id: None,
        },
    )
    .await?
    .token;
    let request = json!({"jsonrpc":"2.0","id":"once","method":"tools/call","params":{"name":"write_note","arguments":{"text":"approved parameters"}}});
    let url = format!("{}/mcp", h.gateway);
    let client = reqwest::Client::new();
    let pending = tokio::spawn(client.post(&url).bearer_auth(&token).json(&request).send());
    for _ in 0..100 {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_calls WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
        if count == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(v, _)| v["method"] != "tools/call")
    );
    ryzhub_server::mcp::project(&h.app, &lease, run).await?;
    let (action, epoch) = h.action(run).await?;
    h.send(
        &user,
        conv,
        Command::DecideApproval(DecideApproval {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            decision: "allow".into(),
        }),
    )
    .await?;
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    let result: Value = pending.await??.json().await?;
    assert!(result.get("error").is_none(), "{result}");
    let again: Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&request)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(result, again);
    let mut changed = request;
    changed["params"]["arguments"]["text"] = json!("different");
    let denied: Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&changed)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(denied["error"]["message"], "REQUEST_ID_REUSED");
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(v, _)| v["method"] == "tools/call")
            .count(),
        1
    );
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS/containerd and actual Codex"]
async fn owner_restarts_do_not_restart_or_interrupt_native_model_stream() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut first = h.owner()?;
    h.send(&user, conv, message("slow native response")).await?;
    for _ in 0..200 {
        if h.requests
            .lock()
            .unwrap()
            .iter()
            .any(|(v, _)| v.get("model").is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    first.kill().await?;
    let mut second = h.owner()?;
    let run = h.wait_state(conv, "completed").await?;
    let epoch: i64 = sqlx::query_scalar("SELECT exec_epoch FROM runs WHERE id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert!(epoch >= 2);
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(v, _)| v.get("model").is_some())
            .count(),
        1
    );
    let usage: (String, bool) =
        sqlx::query_as("SELECT status,estimated FROM usage_records WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(usage, ("completed".into(), false));
    second.kill().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS"]
async fn restore_pin_blocks_expired_checkpoint_gc_and_secondary_repairs_primary()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    h.send(&user, conv, message("checkpoint metadata fixture"))
        .await?;
    let lease = h
        .app
        .store
        .claim(org, conv, Uuid::now_v7(), 60_000)
        .await?
        .unwrap();
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    let sandbox = Uuid::now_v7();
    sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent) VALUES($1,$2,$3,$4,1,1,'stopped','{}',true)").bind(sandbox).bind(org).bind(conv).bind(run).execute(&h.db).await?;
    use sha2::{Digest, Sha256};
    let bytes = b"immutable archive fixture";
    let key = format!("{:x}", Sha256::digest(bytes));
    let p = ryzhub_server::workspace::blob::BlobStore::configured(&h.app.config.work.storage_root)?
        .scoped(org);
    let s = ryzhub_server::workspace::blob::BlobStore::configured(
        h.app.config.work.secondary_root.as_deref().unwrap(),
    )?
    .scoped(org);
    p.put(&key, bytes).await?;
    s.put(&key, bytes).await?;
    let checkpoint = Uuid::now_v7();
    sqlx::query("INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq,state,delete_after) VALUES($1,$2,$3,$4,$5,0,0,'tombstoned',now()-interval '1 second')").bind(checkpoint).bind(org).bind(conv).bind(sandbox).bind(json!({"sha256":key})).execute(&h.db).await?;
    let pin = Uuid::now_v7();
    let mut restoring = h.app.store.tx(org).await?;
    sqlx::query("SELECT id FROM checkpoints WHERE org_id=$1 AND id=$2 FOR UPDATE")
        .bind(org)
        .bind(checkpoint)
        .fetch_one(&mut *restoring)
        .await?;
    let gc_app = h.app.clone();
    let mut collecting =
        tokio::spawn(async move { ryzhub_server::workspace::gc(&gc_app, org, checkpoint).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut collecting)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO restore_pins(id,org_id,checkpoint_id) VALUES($1,$2,$3)")
        .bind(pin)
        .bind(org)
        .bind(checkpoint)
        .execute(&mut *restoring)
        .await?;
    restoring.commit().await?;
    assert!(
        !collecting.await??,
        "GC must recheck a pin committed by its concurrent restore"
    );
    assert_eq!(p.get(&key).await?, bytes);
    p.delete(&key).await?;
    p.put(&key, &s.get(&key).await?).await?;
    assert_eq!(p.get(&key).await?, bytes);
    sqlx::query("DELETE FROM restore_pins WHERE id=$1")
        .bind(pin)
        .execute(&h.db)
        .await?;
    let head = Uuid::now_v7();
    let superseded = Uuid::now_v7();
    for id in [head, superseded] {
        sqlx::query("INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq,created_at) VALUES($1,$2,$3,$4,$5,0,0,now()-interval '40 days')")
            .bind(id).bind(org).bind(conv).bind(sandbox).bind(json!({"sha256":key})).execute(&h.db).await?;
    }
    sqlx::query(
        "INSERT INTO workspace_heads(org_id,conversation_id,checkpoint_id) VALUES($1,$2,$3)",
    )
    .bind(org)
    .bind(conv)
    .bind(head)
    .execute(&h.db)
    .await?;
    let mut app = h.app.clone();
    Arc::make_mut(&mut app.config)
        .work
        .checkpoint_retention_days = 30;
    Arc::make_mut(&mut app.config).work.checkpoint_gc_grace_secs = 60;
    ryzhub_server::workspace::collect_org(&app, org).await?;
    let states: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id,state FROM checkpoints WHERE org_id=$1")
            .bind(org)
            .fetch_all(&h.db)
            .await?;
    assert!(states.contains(&(checkpoint, "deleted".into())));
    assert!(states.contains(&(superseded, "tombstoned".into())));
    assert!(states.contains(&(head, "committed".into())));
    assert_eq!(
        p.get(&key).await?,
        bytes,
        "the shared blob and current head survive GC"
    );
    assert!(
        !ryzhub_server::workspace::gc(&app, org, superseded).await?,
        "grace period is recoverable"
    );
    sqlx::query("UPDATE checkpoints SET delete_after=now()-interval '1 second' WHERE id=$1")
        .bind(superseded)
        .execute(&h.db)
        .await?;
    let mut worker = h.owner()?;
    for _ in 0..100 {
        let state: String = sqlx::query_scalar("SELECT state FROM checkpoints WHERE id=$1")
            .bind(superseded)
            .fetch_one(&h.db)
            .await?;
        if state == "deleted" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    worker.kill().await?;
    let state: String = sqlx::query_scalar("SELECT state FROM checkpoints WHERE id=$1")
        .bind(superseded)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(
        state, "deleted",
        "the production collector must run in the Owner role"
    );
    let unrelated =
        ryzhub_server::workspace::blob::BlobStore::configured(&app.config.work.storage_root)?
            .scoped(Uuid::now_v7());
    unrelated.put(&key, bytes).await?;
    sqlx::query("DELETE FROM workspace_heads WHERE org_id=$1")
        .bind(org)
        .execute(&h.db)
        .await?;
    Arc::make_mut(&mut app.config).work.checkpoint_gc_grace_secs = 0;
    ryzhub_server::workspace::collect_org(&app, org).await?;
    assert!(p.get(&key).await.is_err());
    assert!(s.get(&key).await.is_err());
    assert_eq!(
        unrelated.get(&key).await?,
        bytes,
        "another tenant's identical blob survives"
    );
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS/containerd and actual Codex"]
async fn node_lease_expiry_restores_committed_rpo_and_invalidates_later_operations()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    h.send(&user, conv, message("durable prefix")).await?;
    let first = h.wait_state(conv, "completed").await?;
    let job: Value = reqwest::Client::new()
        .post(format!(
            "{}/orgs/{org}/conversations/{conv}/workspace",
            h.base
        ))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"kind":"checkpoint"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let checkpoint = id(&job, "id");
    for _ in 0..300 {
        let state: String = sqlx::query_scalar("SELECT state FROM workspace_jobs WHERE id=$1")
            .bind(checkpoint)
            .fetch_one(&h.db)
            .await?;
        if state == "completed" {
            break;
        }
        ensure!(state != "failed", "CHECKPOINT_FAILED");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (old, epoch): (Uuid, i64) = sqlx::query_as(
        "SELECT id,exec_epoch FROM sandboxes WHERE conversation_id=$1 AND state='running'",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    h.send(&user, conv, message("slow uncommitted operation"))
        .await?;
    for _ in 0..200 {
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=$1 AND id<>$2 AND native_turn_id IS NOT NULL)").bind(conv).bind(first).fetch_one(&h.db).await?;
        if active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ryzhub_execution::call(
        std::path::Path::new(&h.app.config.sandbox.host_socket),
        &ryzhub_execution::HostCommand::Stop {
            sandbox_id: old,
            exec_epoch: epoch as u64,
        },
    )
    .await?;
    let mut recovered = false;
    for _ in 0..500 {
        let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sandboxes WHERE conversation_id=$1 AND id<>$2 AND state='running')").bind(conv).bind(old).fetch_one(&h.db).await?;
        if live {
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(recovered, "AUTOMATIC_RECOVERY_TIMEOUT");
    let invalid: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items WHERE conversation_id=$1 AND state='invalidated'",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    assert!(invalid > 0);
    let notice:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items WHERE conversation_id=$1 AND kind='notice' AND text LIKE '%水位后的操作%')").bind(conv).fetch_one(&h.db).await?;
    assert!(notice);
    let revoked: bool = sqlx::query_scalar(
        "SELECT bool_and(revoked_at IS NOT NULL) FROM virtual_keys WHERE sandbox_id=$1",
    )
    .bind(old)
    .fetch_one(&h.db)
    .await?;
    assert!(revoked);
    owner.kill().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires local S3 wire fixture verified by AWS botocore"]
async fn s3_sigv4_storage_roundtrip_and_authentication() -> anyhow::Result<()> {
    let s3_url =
        std::env::var("RYZHUB_TEST_S3_URL").unwrap_or_else(|_| "http://127.0.0.1:59000".into());
    let mut config = tempfile::NamedTempFile::new()?;
    write!(
        config,
        "endpoint = \"{s3_url}\"\nbucket = \"ryzhub-test\"\nregion = \"us-east-1\"\naccess_env = \"RYZHUB_TEST_S3_ACCESS\"\nsecret_env = \"RYZHUB_TEST_S3_SECRET\"\n"
    )?;
    let store = ryzhub_server::workspace::blob::BlobStore::configured(&format!(
        "s3config:{}",
        config.path().display()
    ))?
    .scoped(Uuid::now_v7());
    use sha2::{Digest, Sha256};
    let body = format!("archive {}", Uuid::now_v7());
    let key = format!("{:x}", Sha256::digest(body.as_bytes()));
    store.put(&key, body.as_bytes()).await?;
    assert_eq!(store.get(&key).await?, body.as_bytes());
    store.delete(&key).await?;
    assert!(store.get(&key).await.is_err());
    // Objects larger than one part go up as a multipart upload.
    let large: Vec<u8> = (0..16 * 1024 * 1024 + 1).map(|i| (i % 251) as u8).collect();
    let large_key = format!("{:x}", Sha256::digest(&large));
    store.put(&large_key, &large).await?;
    assert_eq!(store.get(&large_key).await?, large);
    store.delete(&large_key).await?;
    // Clients send parts straight to the bucket over presigned URLs; a URL works only for the
    // part it was signed for.
    assert!(store.direct());
    let target = format!("{:x}", Sha256::digest(b"direct target"));
    let upload = store.start_direct(&target).await?;
    let (first, second) = (vec![7u8; 5 * 1024 * 1024], b"tail".to_vec());
    let mut etags = vec![];
    for (n, part) in [(1, &first), (2, &second)] {
        let url = store
            .presign_part(&target, &upload, n, Duration::from_secs(600))
            .await?;
        let tampered = url.replace(&format!("partNumber={n}"), "partNumber=9");
        let refused = reqwest::Client::new()
            .put(&tampered)
            .body(part.clone())
            .send()
            .await?;
        assert_eq!(refused.status(), 403, "signed for another part");
        let sent = reqwest::Client::new()
            .put(&url)
            .body(part.clone())
            .send()
            .await?
            .error_for_status()?;
        etags.push(sent.headers()["etag"].to_str()?.to_owned());
    }
    store.finish_direct(&target, &upload, etags).await?;
    let (size, mut reader) = store.open(&target).await?;
    let mut assembled = vec![];
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut assembled).await?;
    assert_eq!(size as usize, first.len() + second.len());
    assert_eq!(&assembled[..first.len()], first.as_slice());
    assert_eq!(&assembled[first.len()..], second.as_slice());
    store.delete(&target).await?;
    let abandoned = store.start_direct(&target).await?;
    store.abort_direct(&target, &abandoned).await?;
    config.as_file_mut().set_len(0)?;
    use std::io::{Seek, SeekFrom};
    config.as_file_mut().seek(SeekFrom::Start(0))?;
    write!(
        config,
        "endpoint = \"{s3_url}\"\nbucket = \"ryzhub-test\"\nregion = \"us-east-1\"\naccess_env = \"RYZHUB_TEST_S3_ACCESS\"\nsecret_env = \"RYZHUB_TEST_UPSTREAM_KEY\"\n"
    )?;
    let bad = ryzhub_server::workspace::blob::BlobStore::configured(&format!(
        "s3config:{}",
        config.path().display()
    ))?;
    assert!(bad.put(&key, body.as_bytes()).await.is_err());
    Ok(())
}
#[tokio::test]
#[ignore = "requires actual Codex, PostgreSQL/NATS/containerd"]
async fn native_command_approval_is_durable_and_executes_once() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    h.send(&user, conv, message("native-exec")).await?;
    let run = h.wait_state(conv, "requires_action").await?;
    let (action, epoch) = h.action(run).await?;
    let (sandbox, exec_epoch): (Uuid, i64) =
        sqlx::query_as("SELECT id,exec_epoch FROM sandboxes WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    let before = guest_exec(
        std::path::Path::new(&h.app.config.sandbox.host_socket),
        sandbox,
        exec_epoch,
        "before-approval",
        "test ! -f /workspace/approval-marker",
    )
    .await?;
    assert_eq!(before["exit_code"], 0);
    h.send(
        &user,
        conv,
        Command::DecideApproval(DecideApproval {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            decision: "allow".into(),
        }),
    )
    .await?;
    h.wait_state(conv, "completed").await?;
    let result = guest_exec(
        std::path::Path::new(&h.app.config.sandbox.host_socket),
        sandbox,
        exec_epoch,
        "after-approval",
        "cat /workspace/approval-marker",
    )
    .await?;
    assert_eq!(result["stdout_head"], "native-once");
    let tool:String=sqlx::query_scalar("SELECT text FROM items WHERE run_id=$1 AND kind='tool' AND state='completed' ORDER BY last_seq DESC LIMIT 1").bind(run).fetch_one(&h.db).await?;
    assert!(tool.contains("native-once"));
    owner.kill().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, containerd and Codex"]
async fn a_work_terminal_runs_commands_resumes_detached_and_is_cut_by_fencing() -> anyhow::Result<()>
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message as Ws, client::IntoClientRequest};
    let h = Harness::new().await?;
    let user = h.user().await?;
    let (org, conv) = (id(&user, "org_id"), h.conversation(&user).await?);
    let mut owner = h.owner()?;
    h.send(&user, conv, message("hello codex")).await?;
    h.wait_state(conv, "completed").await?;
    let token = user["token"].as_str().unwrap().to_owned();
    let connect = |since: u64| {
        let url = format!(
            "{}/orgs/{org}/conversations/{conv}/terminal?cols=100&rows=30&since={since}",
            h.base.replacen("http", "ws", 1)
        );
        let token = token.clone();
        async move {
            let mut request = url.into_client_request()?;
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {token}").parse()?);
            let (socket, _) = tokio_tungstenite::connect_async(request).await?;
            anyhow::Ok(socket)
        }
    };
    // Output until `needle` appears, and how many bytes that was.
    async fn until(
        socket: &mut (
                 impl futures_util::Stream<Item = Result<Ws, tokio_tungstenite::tungstenite::Error>>
                 + Unpin
             ),
        needle: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let mut seen = Vec::new();
        while !String::from_utf8_lossy(&seen).contains(needle) {
            match tokio::time::timeout(Duration::from_secs(15), socket.next()).await? {
                Some(Ok(Ws::Binary(bytes))) => seen.extend_from_slice(&bytes),
                Some(Ok(_)) => {}
                other => anyhow::bail!("terminal ended early: {other:?}"),
            }
        }
        Ok(seen)
    }

    let mut socket = connect(0).await?;
    let attached = socket.next().await.context("attached")??;
    assert_eq!(
        serde_json::from_str::<Value>(attached.to_text()?)?["attached"]["start"],
        0
    );
    socket
        .send(Ws::Binary(
            b"cd /workspace && echo term-$((6*7))\r".to_vec().into(),
        ))
        .await?;
    let first = until(&mut socket, "term-42").await?;
    socket
        .send(Ws::Text(
            json!({"resize": {"cols": 120, "rows": 40}})
                .to_string()
                .into(),
        ))
        .await?;
    socket
        .send(Ws::Binary(b"stty size\r".to_vec().into()))
        .await?;
    let sized = until(&mut socket, "40 120").await?;
    let offset = (first.len() + sized.len()) as u64;
    // Leaving detaches; reconnecting from the offset seen continues without replaying it.
    drop(socket);
    let mut socket = connect(offset).await?;
    socket.next().await.context("attached")??;
    socket
        .send(Ws::Binary(b"echo again-$((1+1))\r".to_vec().into()))
        .await?;
    let resumed = until(&mut socket, "again-2").await?;
    assert!(
        !String::from_utf8_lossy(&resumed).contains("term-42"),
        "replayed seen output"
    );
    // A newer execution epoch cuts the connection.
    let (sandbox, epoch): (Uuid, i64) = sqlx::query_as(
        "SELECT id,exec_epoch FROM sandboxes WHERE conversation_id=$1 AND state='running'",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    let fence = ryzhub_execution::HostCommand::Guest {
        request: ryzhub_execution::GuestRequest {
            sandbox_id: sandbox,
            exec_epoch: epoch as u64 + 1,
            command: ryzhub_execution::GuestCommand::Fence,
        },
    };
    ryzhub_server::sandbox::host(&h.app, org, sandbox, fence).await?;
    let cut = loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next()).await? {
            Some(Ok(Ws::Binary(_))) => continue,
            other => break other,
        }
    };
    assert!(
        matches!(cut, None | Some(Ok(Ws::Close(_))) | Some(Err(_))),
        "{cut:?}"
    );
    sqlx::query("UPDATE sandboxes SET exec_epoch=$2 WHERE id=$1")
        .bind(sandbox)
        .bind(epoch + 1)
        .execute(&h.db)
        .await?;
    // Under the new epoch the same session is still there; closing ends its shell.
    let mut socket = connect(0).await?;
    socket.next().await.context("attached")??;
    until(&mut socket, "again-2").await?;
    socket
        .send(Ws::Text(json!({"close": true}).to_string().into()))
        .await?;
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS/containerd and actual Codex binary"]
async fn idle_work_archives_through_its_job_and_a_purged_workspace_cannot_restore()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let worker = PgPool::connect(&h.worker_url).await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    h.send(&user, conv, message("hello codex")).await?;
    h.wait_state(conv, "completed").await?;
    let mut head: Option<Uuid> = None;
    for _ in 0..600 {
        head = sqlx::query_scalar(
            "SELECT checkpoint_id FROM workspace_heads WHERE conversation_id=$1 AND checkpoint_id IS NOT NULL",
        )
        .bind(conv)
        .fetch_optional(&h.db)
        .await?;
        if head.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    head.context("turn checkpoint")?;
    wait_workspace_idle(&h, conv).await?;
    sqlx::query(
        "UPDATE org_policies SET policy=policy||'{\"auto_archive_after_secs\":3600,\"archive_retention_secs\":3600}'
         WHERE org_id=$1",
    )
    .bind(org)
    .execute(&h.db)
    .await?;
    sqlx::query("UPDATE conversations SET created_at=now()-interval '2 hours' WHERE id=$1")
        .bind(conv)
        .execute(&h.db)
        .await?;
    sqlx::query(
        "UPDATE runs SET started_at=now()-interval '2 hours',ended_at=now()-interval '2 hours'
         WHERE conversation_id=$1",
    )
    .bind(conv)
    .execute(&h.db)
    .await?;
    ryzhub_server::lifecycle::sweep(&h.app, &worker).await?;
    let queued: String = sqlx::query_scalar(
        "SELECT kind FROM workspace_jobs WHERE conversation_id=$1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(
        queued, "archive",
        "a running workspace archives through its job"
    );
    let mut archived = false;
    for _ in 0..600 {
        archived = sqlx::query_scalar(
            "SELECT state='archived' AND archived_at IS NOT NULL FROM conversations WHERE id=$1",
        )
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
        if archived {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(archived, "archived after its checkpoint");
    let (stored, sandbox): (String, String) = sqlx::query_as(
        "SELECT h.storage_state,s.state FROM workspace_heads h JOIN sandboxes s
         ON s.conversation_id=h.conversation_id AND s.persistent
         WHERE h.conversation_id=$1 ORDER BY s.created_at DESC LIMIT 1",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    assert_eq!((stored.as_str(), sandbox.as_str()), ("cold", "archived"));

    sqlx::query("UPDATE conversations SET archived_at=now()-interval '2 hours' WHERE id=$1")
        .bind(conv)
        .execute(&h.db)
        .await?;
    ryzhub_server::lifecycle::sweep(&h.app, &worker).await?;
    let (stored, head): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT storage_state,checkpoint_id FROM workspace_heads WHERE conversation_id=$1",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    assert_eq!((stored.as_str(), head), ("deleted", None));
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM checkpoints WHERE conversation_id=$1 AND state<>'deleted'",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(left, 0);
    let refused = reqwest::Client::new()
        .post(format!(
            "{}/orgs/{org}/conversations/{conv}/workspace",
            h.base
        ))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"kind":"restore"}))
        .send()
        .await?;
    assert_eq!(refused.status(), 409);
    assert_eq!(refused.json::<Value>().await?["error"], "WORKSPACE_PURGED");
    worker.close().await;
    owner.kill().await?;
    h.close().await
}
