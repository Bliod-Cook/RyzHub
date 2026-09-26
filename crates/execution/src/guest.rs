use crate::{journal, *};
use anyhow::{Context, ensure};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio, sync::Arc};
use tokio::{
    io::AsyncRead,
    net::UnixListener,
    process::ChildStdin,
    sync::{Mutex, mpsc},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Operation {
    fingerprint: Vec<u8>,
    result: Value,
    #[serde(default)]
    native: bool,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    sandbox_id: Option<Uuid>,
    epoch: u64,
    acked: u64,
    head: u64,
    operations: BTreeMap<String, Operation>,
    events: Vec<GuestEvent>,
}
struct Job {
    stdin: Option<ChildStdin>,
    kill: mpsc::Sender<()>,
    pid: u32,
}
pub struct Guest {
    id: Uuid,
    path: PathBuf,
    state: Mutex<State>,
    jobs: Mutex<BTreeMap<String, Job>>,
    native: Mutex<Option<Arc<crate::native::Native>>>,
    folds: Mutex<BTreeMap<String, Value>>,
    pub(crate) key: tokio::sync::RwLock<Option<(String, String)>>,
    _lock: std::fs::File,
}
impl Guest {
    pub fn open(id: Uuid, directory: PathBuf) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&directory)?;
        let process_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join("guest.lock"))?;
        process_lock.try_lock()?;
        let path = directory.join("journal.json");
        let mut state: State = journal::load(&path)?;
        ensure!(
            state.sandbox_id.is_none_or(|saved| saved == id),
            "SANDBOX_ID_MISMATCH"
        );
        state.sandbox_id = Some(id);
        // A guest crash is different from a host/Owner restart: never rerun unknown side effects.
        let interrupted: Vec<_> = state
            .operations
            .iter()
            .filter(|(_, v)| v.result["state"] == "running")
            .map(|(k, _)| k.clone())
            .collect();
        for op in interrupted {
            let result = json!({"state":"outcome_unknown","error":"guest restarted while process was running"});
            state.operations.get_mut(&op).unwrap().result = result.clone();
            state.head += 1;
            state.events.push(GuestEvent {
                gseq: state.head,
                op_id: op,
                result,
            });
        }
        journal::save(&path, &state)?;
        Ok(Arc::new(Self {
            id,
            path,
            state: Mutex::new(state),
            jobs: Mutex::new(BTreeMap::new()),
            native: Mutex::new(None),
            folds: Mutex::new(BTreeMap::new()),
            key: tokio::sync::RwLock::new(None),
            _lock: process_lock,
        }))
    }
    pub async fn handle(self: &Arc<Self>, request: GuestRequest) -> anyhow::Result<Value> {
        ensure!(request.sandbox_id == self.id, "SANDBOX_ID_MISMATCH");
        let mut state = self.state.lock().await;
        ensure!(
            request.exec_epoch >= state.epoch && request.exec_epoch > 0,
            "FENCED"
        );
        if matches!(request.command, GuestCommand::Fence) {
            state.epoch = request.exec_epoch;
            journal::save(&self.path, &*state)?;
            return Ok(json!({"epoch":state.epoch}));
        }
        ensure!(request.exec_epoch == state.epoch, "FENCE_REQUIRED");
        match request.command {
            GuestCommand::ProjectInstructions { text } => {
                ensure!(text.len() <= 70000, "INVALID_PROJECT_INSTRUCTIONS");
                let dir = std::path::Path::new("/workspace/.platform");
                std::fs::create_dir_all(dir)?;
                journal::atomic_bytes(&dir.join("project.md"), text.as_bytes())?;
                for (name, import) in [
                    (
                        "AGENTS.md",
                        "\n\n<!-- RyzHub project -->\nRead .platform/project.md for the current project instructions. Knowledge files are read-only in /project.\n",
                    ),
                    (
                        "CLAUDE.md",
                        "\n\n<!-- RyzHub project -->\n@.platform/project.md\n",
                    ),
                ] {
                    let path = std::path::Path::new("/workspace").join(name);
                    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
                    if !content.contains("<!-- RyzHub project -->") {
                        content.push_str(import);
                        journal::atomic_bytes(&path, content.as_bytes())?;
                    }
                }
                Ok(json!({"installed":true}))
            }
            GuestCommand::NativeStart {
                model,
                mcp,
                harness,
            } => {
                let mut native = self.native.lock().await;
                if native.is_none() {
                    *native = Some(
                        crate::native::Native::start(self.clone(), &model, mcp, &harness).await?,
                    );
                }
                Ok(json!({"running":true,"capabilities":crate::native::capabilities(&harness)}))
            }
            GuestCommand::NativeRequest {
                op_id,
                method,
                params,
            } => {
                ensure!(
                    matches!(
                        method.as_str(),
                        "thread/start"
                            | "thread/resume"
                            | "thread/read"
                            | "turn/start"
                            | "turn/steer"
                            | "turn/interrupt"
                            | "model/list"
                            | "thread/compact/start"
                            | "thread/fork"
                            | "skills/list"
                            | "collaborationMode/list"
                            | "session/configure"
                    ),
                    "NATIVE_METHOD_UNSUPPORTED"
                );
                if let Some(result) = self.reserve_control(
                    &mut state,
                    &op_id,
                    json!({"method":method,"params":params}),
                )? {
                    return Ok(result);
                }
                self.native
                    .lock()
                    .await
                    .as_ref()
                    .context("NATIVE_NOT_STARTED")?
                    .send(json!({"id":op_id,"method":method,"params":params}))
                    .await?;
                Ok(json!({"state":"running"}))
            }
            GuestCommand::NativeAnswer { op_id, id, result } => {
                if let Some(result) =
                    self.reserve_control(&mut state, &op_id, json!({"id":id,"result":result}))?
                {
                    return Ok(result);
                }
                self.native
                    .lock()
                    .await
                    .as_ref()
                    .context("NATIVE_NOT_STARTED")?
                    .send(json!({"id":id,"result":result}))
                    .await?;
                self.complete_control(&mut state, &op_id, json!({"state":"completed"}))
            }
            GuestCommand::RotateKey { token, socket_name } => {
                ensure!(token.len() <= 8192, "INVALID_KEY");
                ensure!(
                    socket_name.starts_with("llm")
                        && socket_name.ends_with(".sock")
                        && !socket_name.contains('/')
                        && !socket_name.contains(".."),
                    "INVALID_GATEWAY_SOCKET"
                );
                *self.key.write().await = Some((token, socket_name));
                Ok(json!({"rotated":true}))
            }
            GuestCommand::Exec { op_id, spec } => {
                ensure!(
                    !op_id.is_empty()
                        && op_id.len() <= 512
                        && !spec.cmd.is_empty()
                        && spec.cmd.len() <= 65536
                        && spec.env.len() <= 64
                        && spec.timeout_ms > 0
                        && spec.timeout_ms <= 600_000,
                    "INVALID_EXEC"
                );
                ensure!(!spec.pty, "PTY_UNSUPPORTED");
                let fingerprint = Sha256::digest(serde_json::to_vec(&spec)?).to_vec();
                if let Some(old) = state.operations.get(&op_id) {
                    ensure!(old.fingerprint == fingerprint, "OP_ID_REUSED");
                    return Ok(old.result.clone());
                }
                let pending = json!({"state":"running","handle":op_id});
                state.operations.insert(
                    op_id.clone(),
                    Operation {
                        fingerprint,
                        result: pending.clone(),
                        native: false,
                    },
                );
                journal::save(&self.path, &*state)?;
                // The journal remains locked through dispatch, so Fence cannot overtake an old command.
                self.spawn(op_id, spec).await?;
                Ok(pending)
            }
            GuestCommand::Poll { op_id } => Ok(state
                .operations
                .get(&op_id)
                .context("UNKNOWN_OPERATION")?
                .result
                .clone()),
            GuestCommand::Write {
                op_id,
                request_id,
                data,
            } => {
                ensure!(data.len() <= 65536, "STDIN_TOO_LARGE");
                if let Some(result) = self.reserve_control(
                    &mut state,
                    &request_id,
                    json!({"kind":"write","op_id":op_id,"data":data}),
                )? {
                    return Ok(result);
                }
                let mut jobs = self.jobs.lock().await;
                let result=async {
                  let job = jobs.get_mut(&op_id).context("JOB_NOT_RUNNING")?;
                  let stdin = job.stdin.as_mut().context("STDIN_CLOSED")?;
                  tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    stdin.write_all(data.as_bytes()),
                )
                  .await??;
                  Ok::<_,anyhow::Error>(json!({"state":"completed","written":data.len()}))
                }.await.unwrap_or_else(|_|json!({"state":"outcome_unknown","error":"stdin outcome unknown; do not automatically retry"}));
                self.complete_control(&mut state, &request_id, result)
            }
            GuestCommand::Kill { op_id, request_id } => {
                if let Some(result) = self.reserve_control(
                    &mut state,
                    &request_id,
                    json!({"kind":"kill","op_id":op_id}),
                )? {
                    return Ok(result);
                }
                if let Some(job) = self.jobs.lock().await.get(&op_id) {
                    let _ = job.kill.try_send(());
                }
                self.complete_control(
                    &mut state,
                    &request_id,
                    json!({"state":"completed","kill_requested":true}),
                )
            }
            GuestCommand::Replay { after } => {
                ensure!(after >= state.acked && after <= state.head, "INVALID_GSEQ");
                Ok(
                    json!({"events":state.events.iter().filter(|e|e.gseq>after).take(64).collect::<Vec<_>>(),"head":state.head,"folds":self.folds.lock().await.clone()}),
                )
            }
            GuestCommand::Ack { through } => {
                ensure!(through <= state.head, "INVALID_GSEQ");
                state.acked = state.acked.max(through);
                let acked = state.acked;
                state.events.retain(|e| e.gseq > acked);
                journal::save(&self.path, &*state)?;
                Ok(json!({"acked":acked}))
            }
            GuestCommand::Fence => unreachable!(),
        }
    }
    fn reserve_control(
        &self,
        state: &mut State,
        id: &str,
        value: Value,
    ) -> anyhow::Result<Option<Value>> {
        ensure!(!id.is_empty() && id.len() <= 512, "INVALID_OPERATION_ID");
        let fingerprint = Sha256::digest(serde_json::to_vec(&value)?).to_vec();
        if let Some(old) = state.operations.get(id) {
            ensure!(old.fingerprint == fingerprint, "OP_ID_REUSED");
            return Ok(Some(old.result.clone()));
        }
        state.operations.insert(
            id.into(),
            Operation {
                fingerprint,
                result: json!({"state":"running"}),
                native: value.get("method").is_some(),
            },
        );
        journal::save(&self.path, state)?;
        Ok(None)
    }
    fn complete_control(
        &self,
        state: &mut State,
        id: &str,
        result: Value,
    ) -> anyhow::Result<Value> {
        state.operations.get_mut(id).unwrap().result = result.clone();
        state.head += 1;
        state.events.push(GuestEvent {
            gseq: state.head,
            op_id: id.into(),
            result: result.clone(),
        });
        journal::save(&self.path, state)?;
        Ok(result)
    }
    async fn spawn(self: &Arc<Self>, op: String, spec: ExecSpec) -> anyhow::Result<()> {
        use std::os::unix::process::CommandExt;
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-lc")
            .arg(&spec.cmd)
            .current_dir(&spec.cwd)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", "/home/agent")
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let child = command.spawn();
        let this = self.clone();
        let mut child = match child {
            Ok(child) => child,
            Err(_) => {
                tokio::spawn(async move {
                    let _=this.complete(&op,json!({"state":"completed","exit_code":127,"error":"process spawn failed"})).await;
                });
                return Ok(());
            }
        };
        let pid = child.id().context("MISSING_PID")?;
        let (kill, mut killed) = mpsc::channel(1);
        self.jobs.lock().await.insert(
            op.clone(),
            Job {
                stdin: child.stdin.take(),
                kill,
                pid,
            },
        );
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let directory = self.path.parent().unwrap().join("outputs");
        std::fs::create_dir_all(&directory)?;
        let filename = format!("{:x}", Sha256::digest(op.as_bytes()));
        let stdout_path = directory.join(format!("{filename}.stdout"));
        let stderr_path = directory.join(format!("{filename}.stderr"));
        tokio::spawn(async move {
            let out = tokio::spawn(capture(stdout, stdout_path.clone()));
            let err = tokio::spawn(capture(stderr, stderr_path.clone()));
            let mut timed_out = false;
            let mut cancelled = false;
            let status = tokio::select! {
                result=child.wait()=>result,
                _=tokio::time::sleep(std::time::Duration::from_millis(spec.timeout_ms))=>{timed_out=true;let _=killpg(Pid::from_raw(pid as i32),Signal::SIGKILL);child.wait().await},
                _=killed.recv()=>{cancelled=true;let _=killpg(Pid::from_raw(pid as i32),Signal::SIGKILL);child.wait().await},
            };
            // Kill descendants holding pipes after the foreground shell exits.
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
            let output = out.await.ok().and_then(Result::ok).unwrap_or_default();
            let errors = err.await.ok().and_then(Result::ok).unwrap_or_default();
            let value = json!({"state":"completed","exit_code":status.ok().and_then(|s|s.code()),"timeout":timed_out,"cancelled":cancelled,"stdout_head":output.0,"stdout_tail":output.1,"stderr_head":errors.0,"stderr_tail":errors.1,"truncated":output.2||errors.2,"full_output_path":stdout_path,"stderr_path":stderr_path});
            let _ = this.complete(&op, value).await;
        });
        Ok(())
    }
    async fn complete(&self, op: &str, result: Value) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        state
            .operations
            .get_mut(op)
            .context("UNKNOWN_OPERATION")?
            .result = result.clone();
        state.head += 1;
        let gseq = state.head;
        state.events.push(GuestEvent {
            gseq,
            op_id: op.into(),
            result,
        });
        journal::save(&self.path, &*state)?;
        self.jobs.lock().await.remove(op);
        Ok(())
    }
    pub async fn serve(self: Arc<Self>, socket: PathBuf) -> anyhow::Result<()> {
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:8090").await?;
        let app = crate::proxy::router(self.clone());
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app).await;
        });
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        loop {
            let (stream, _) = listener.accept().await?;
            let this = self.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                let mut read = BufReader::new(read).take(4 * 1024 * 1024 + 1);
                let result = async {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        read.read_line(&mut line),
                    )
                    .await??;
                    ensure!(line.len() <= 4 * 1024 * 1024, "REQUEST_TOO_LARGE");
                    this.handle(serde_json::from_str(&line)?).await
                }
                .await;
                let reply = match result {
                    Ok(value) => Reply {
                        ok: true,
                        value,
                        error: None,
                    },
                    Err(e) => Reply {
                        ok: false,
                        value: Value::Null,
                        error: Some(e.to_string()),
                    },
                };
                if let Ok(mut bytes) = serde_json::to_vec(&reply) {
                    bytes.push(b'\n');
                    let _ = write.write_all(&bytes).await;
                }
            });
        }
    }
    pub async fn native_event(&self, value: Value) -> anyhow::Result<()> {
        if let Some(method) = value["method"].as_str() {
            if method.ends_with("/delta") || method.ends_with("Delta") {
                let params = &value["params"];
                if let Some(id) = params["itemId"].as_str() {
                    let mut folds = self.folds.lock().await;
                    let fold = folds.entry(id.into()).or_insert_with(
                        || json!({"text":"","method":method,"turnId":params["turnId"]}),
                    );
                    let mut text = fold["text"].as_str().unwrap_or("").to_owned();
                    text.push_str(params["delta"].as_str().unwrap_or(""));
                    if text.len() <= 4 * 1024 * 1024 {
                        fold["text"] = text.into();
                    }
                }
                return Ok(());
            }
            if method == "item/completed"
                && let Some(id) = value["params"]["item"]["id"].as_str()
            {
                self.folds.lock().await.remove(id);
            }
        }
        let mut state = self.state.lock().await;
        if value["method"] == "harness/exited" {
            for op in state.operations.values_mut() {
                if op.native && op.result["state"] == "running" {
                    op.result = json!({"state":"outcome_unknown","error":"NATIVE_PROCESS_EXITED"});
                }
            }
        }
        if value.get("method").is_none()
            && let Some(id) = value["id"].as_str()
            && let Some(op) = state.operations.get_mut(id)
        {
            op.result = json!({"state":"completed","response":value});
        }
        state.head += 1;
        let gseq = state.head;
        state.events.push(GuestEvent {
            gseq,
            op_id: "native".into(),
            result: value,
        });
        journal::save(&self.path, &*state)?;
        Ok(())
    }
    pub async fn stop_jobs(&self) {
        for job in self.jobs.lock().await.values() {
            let _ = killpg(Pid::from_raw(job.pid as i32), Signal::SIGKILL);
        }
    }
}
async fn capture(
    mut reader: impl AsyncRead + Unpin,
    path: PathBuf,
) -> anyhow::Result<(String, String, bool)> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut head = Vec::new();
    let mut tail = Vec::new();
    let mut total = 0;
    let mut buffer = [0; 8192];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        if total < 16 * 1024 * 1024 {
            file.write_all(&buffer[..n]).await?;
        }
        total += n;
        let take = (8192 - head.len()).min(n);
        head.extend_from_slice(&buffer[..take]);
        tail.extend_from_slice(&buffer[..n]);
        if tail.len() > 8192 {
            tail.drain(..tail.len() - 8192);
        }
    }
    file.sync_all().await?;
    Ok((
        String::from_utf8_lossy(&head).into(),
        if total > 8192 {
            String::from_utf8_lossy(&tail).into()
        } else {
            String::new()
        },
        total > 8192,
    ))
}

#[cfg(test)]
mod native_exit_tests {
    use super::*;

    #[tokio::test]
    async fn native_exit_resolves_pending_rpc_without_replaying_exec() {
        let directory = tempfile::tempdir().unwrap();
        let guest = Guest::open(Uuid::now_v7(), directory.path().into()).unwrap();
        {
            let mut state = guest.state.lock().await;
            guest
                .reserve_control(&mut state, "models", json!({"method":"model/list"}))
                .unwrap();
            guest
                .reserve_control(&mut state, "exec", json!({"cmd":"independent process"}))
                .unwrap();
        }
        guest
            .native_event(json!({"method":"harness/exited","params":{"exit_code":101}}))
            .await
            .unwrap();
        let durable: State = journal::load(&directory.path().join("journal.json")).unwrap();
        assert_eq!(
            durable.operations["models"].result["state"],
            "outcome_unknown"
        );
        assert_eq!(durable.operations["exec"].result["state"], "running");
        assert_eq!(
            durable.events.last().unwrap().result["params"]["exit_code"],
            101
        );
    }
}
