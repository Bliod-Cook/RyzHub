use ryzhub_execution::{ExecSpec, GuestCommand, GuestRequest, guest::Guest};
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn durable_exec_deduplication_fencing_replay_and_restart() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let id = Uuid::now_v7();
    let guest = Guest::open(id, directory.path().into())?;
    let request = |epoch, command| GuestRequest {
        sandbox_id: id,
        exec_epoch: epoch,
        command,
    };
    guest.handle(request(1, GuestCommand::Fence)).await?;
    let spec: ExecSpec = serde_json::from_value(
        json!({"cmd":"printf once >> marker; sleep 0.1; printf '你好'","cwd":workspace.path(),"timeout_ms":2000}),
    )?;
    let command = GuestCommand::Exec {
        op_id: "turn:call".into(),
        spec: spec.clone(),
    };
    let first = guest.handle(request(1, command.clone())).await?;
    assert_eq!(first["state"], "running");
    assert_eq!(guest.handle(request(1, command.clone())).await?, first);
    let mut changed = spec;
    changed.cmd = "printf different".into();
    assert!(
        guest
            .handle(request(
                1,
                GuestCommand::Exec {
                    op_id: "turn:call".into(),
                    spec: changed
                }
            ))
            .await
            .is_err()
    );
    guest.handle(request(2, GuestCommand::Fence)).await?;
    assert!(guest.handle(request(1, command.clone())).await.is_err());
    let mut final_result = Value::Null;
    for _ in 0..100 {
        final_result = guest
            .handle(request(
                2,
                GuestCommand::Poll {
                    op_id: "turn:call".into(),
                    wait_ms: 0,
                },
            ))
            .await?;
        if final_result["state"] == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(final_result["stdout_head"], "你好");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("marker"))?,
        "once"
    );
    let replay = guest
        .handle(request(
            2,
            GuestCommand::Replay {
                after: 0,
                wait_ms: 0,
                folds_seen: 0,
            },
        ))
        .await?;
    assert_eq!(replay["events"].as_array().unwrap().len(), 1);
    assert_eq!(replay["events"][0]["gseq"], 1);
    drop(guest);
    let restarted = Guest::open(id, directory.path().into())?;
    assert_eq!(restarted.handle(request(2, command)).await?, final_result);
    assert_eq!(
        restarted
            .handle(request(
                2,
                GuestCommand::Replay {
                    after: 0,
                    wait_ms: 0,
                    folds_seen: 0
                }
            ))
            .await?,
        replay
    );
    restarted
        .handle(request(2, GuestCommand::Ack { through: 1 }))
        .await?;
    // A replay from behind the acknowledged position (a concurrent Owner task acknowledged
    // first) returns only what is new; a replay from beyond the head is an error.
    for after in [0, 1] {
        let replay = restarted
            .handle(request(
                2,
                GuestCommand::Replay {
                    after,
                    wait_ms: 0,
                    folds_seen: 0,
                },
            ))
            .await?;
        assert!(replay["events"].as_array().unwrap().is_empty());
    }
    assert!(
        restarted
            .handle(request(
                2,
                GuestCommand::Replay {
                    after: 2,
                    wait_ms: 0,
                    folds_seen: 0
                }
            ))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn exec_timeout_and_output_are_bounded() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let id = Uuid::now_v7();
    let guest = Guest::open(id, directory.path().into())?;
    let call = |command| GuestRequest {
        sandbox_id: id,
        exec_epoch: 1,
        command,
    };
    guest.handle(call(GuestCommand::Fence)).await?;
    let spec = serde_json::from_value(
        json!({"cmd":"head -c 20000 /dev/zero; sleep 10","cwd":workspace.path(),"timeout_ms":100}),
    )?;
    guest
        .handle(call(GuestCommand::Exec {
            op_id: "timeout".into(),
            spec,
        }))
        .await?;
    for _ in 0..100 {
        let result = guest
            .handle(call(GuestCommand::Poll {
                op_id: "timeout".into(),
                wait_ms: 1000,
            }))
            .await?;
        if result["state"] == "completed" {
            assert_eq!(result["timeout"], true);
            assert_eq!(result["truncated"], true);
            assert!(result["stdout_head"].as_str().unwrap().len() <= 8192);
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    anyhow::bail!("job did not time out")
}

#[tokio::test]
async fn stdin_is_idempotent_and_virtual_key_never_enters_journal() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let id = Uuid::now_v7();
    let guest = Guest::open(id, directory.path().into())?;
    let call = |command| GuestRequest {
        sandbox_id: id,
        exec_epoch: 1,
        command,
    };
    guest.handle(call(GuestCommand::Fence)).await?;
    guest
        .handle(call(GuestCommand::RotateKey {
            token: "test-secret-do-not-persist".into(),
            socket_name: "llm.sock".into(),
        }))
        .await?;
    let spec = serde_json::from_value(
        json!({"cmd":"cat > marker","cwd":workspace.path(),"timeout_ms":2000}),
    )?;
    guest
        .handle(call(GuestCommand::Exec {
            op_id: "job".into(),
            spec,
        }))
        .await?;
    let write = GuestCommand::Write {
        op_id: "job".into(),
        request_id: "write".into(),
        data: "once\n".into(),
    };
    let result = guest.handle(call(write.clone())).await?;
    assert_eq!(guest.handle(call(write)).await?, result);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    guest
        .handle(call(GuestCommand::Kill {
            op_id: "job".into(),
            request_id: "kill".into(),
        }))
        .await?;
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("marker"))?,
        "once\n"
    );
    assert!(
        !std::fs::read_to_string(directory.path().join("journal.json"))?
            .contains("test-secret-do-not-persist")
    );
    guest.stop_jobs().await;
    Ok(())
}
