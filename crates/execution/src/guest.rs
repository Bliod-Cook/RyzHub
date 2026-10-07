//! The guest agent inside each sandbox: executes commands, drives the native harness and keeps
//! an ordered, durable journal of results for the Owner.
//!
//! Every externally visible effect has an operation id. A repeated request with the same id
//! and payload returns the first result instead of running again, which makes every Owner
//! retry safe. Results reach the Owner as numbered guest events (`gseq`), replayed until
//! acknowledged; after that only a tombstone of the operation is kept, which still prevents a
//! second execution.
use anyhow::{Context, ensure};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::AsyncRead,
    net::UnixListener,
    process::ChildStdin,
    sync::{Mutex, Notify, mpsc},
};

use crate::{
    harness::{HarnessEvent, HarnessRequest, HarnessResult, ItemPhase},
    journal::{Log, Replay},
    native::{Incoming, Native},
    *,
};

/// Events per replay batch, and their approximate serialized size.
const REPLAY_EVENTS: usize = 64;
const REPLAY_BYTES: usize = 2 * 1024 * 1024;
/// Streaming text kept per in-progress item, and items reported per replay.
const FOLD_BYTES: usize = 256 * 1024;
const FOLD_ITEMS: usize = 16;
/// A replay woken only by streaming text waits this long to batch more of it.
const FOLD_COALESCE: Duration = Duration::from_millis(50);
/// A completed result stays available this long, and at least until acknowledged, so a late
/// poll or a retried request still gets it.
const RESULT_MS: u64 = 3600 * 1000;
/// After that, an operation is remembered by fingerprint alone for this long: a repeated
/// request is answered, never executed a second time.
const TOMBSTONE_MS: u64 = 7 * 24 * 3600 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Operation {
    fingerprint: Vec<u8>,
    result: Value,
    /// A harness request: its native reply is translated with this.
    #[serde(default)]
    request: Option<HarnessRequest>,
    /// The outcome is unknown if the harness exits before replying.
    #[serde(default)]
    native: bool,
    /// The guest event that reported completion, if any.
    #[serde(default)]
    done_gseq: Option<u64>,
    #[serde(default)]
    done_ms: Option<u64>,
}

impl Operation {
    fn running(fingerprint: Vec<u8>) -> Self {
        Self {
            fingerprint,
            result: json!({"state":"running"}),
            request: None,
            native: false,
            done_gseq: None,
            done_ms: None,
        }
    }

    fn is_running(&self) -> bool {
        self.result["state"] == "running"
    }

    /// Drop the result but keep the identity. A repeated request gets `acknowledged`.
    fn forget(&mut self) {
        self.result = json!({"state":"acknowledged"});
        self.request = None;
    }
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

#[derive(Serialize, Deserialize)]
enum Entry {
    Sandbox(Uuid),
    Epoch(u64),
    Reserve {
        id: String,
        operation: Box<Operation>,
    },
    /// Set an operation's result; with `event`, also report it as the next guest event.
    Complete {
        id: String,
        result: Value,
        event: bool,
        at_ms: u64,
    },
    /// A harness event not tied to an operation.
    Event {
        result: Value,
    },
    Ack {
        through: u64,
        at_ms: u64,
    },
}

impl Replay for State {
    type Entry = Entry;
    fn apply(&mut self, entry: &Entry) {
        match entry {
            Entry::Sandbox(id) => self.sandbox_id = Some(*id),
            Entry::Epoch(epoch) => self.epoch = *epoch,
            Entry::Reserve { id, operation } => {
                self.operations.insert(id.clone(), (**operation).clone());
            }
            Entry::Complete {
                id,
                result,
                event,
                at_ms,
            } => {
                let Some(operation) = self.operations.get_mut(id) else {
                    return;
                };
                operation.result = result.clone();
                operation.done_ms = Some(*at_ms);
                if *event {
                    self.head += 1;
                    operation.done_gseq = Some(self.head);
                    self.events.push(GuestEvent {
                        gseq: self.head,
                        op_id: id.clone(),
                        result: result.clone(),
                    });
                }
            }
            Entry::Event { result } => {
                self.head += 1;
                self.events.push(GuestEvent {
                    gseq: self.head,
                    op_id: "native".into(),
                    result: result.clone(),
                });
            }
            Entry::Ack { through, at_ms } => {
                self.acked = self.acked.max(*through);
                let acked = self.acked;
                self.events.retain(|event| event.gseq > acked);
                // Keep results an Owner might still ask for: running operations, completions it
                // has not acknowledged, and recent results. The rest become tombstones, so an
                // Owner that re-sends an old operation can never run it twice.
                self.operations.retain(|_, op| {
                    let needed = op.is_running()
                        || op.done_gseq.is_some_and(|gseq| gseq > acked)
                        || op.done_ms.is_none_or(|done| done + RESULT_MS > *at_ms);
                    if !needed {
                        op.forget();
                    }
                    needed || op.done_ms.is_some_and(|done| done + TOMBSTONE_MS > *at_ms)
                });
            }
        }
    }
}

struct Journal {
    log: Log<State>,
    state: State,
}

impl Journal {
    fn record(&mut self, entry: Entry) -> anyhow::Result<()> {
        self.log.append(&entry)?;
        self.state.apply(&entry);
        self.log.compact(&self.state)
    }

    fn complete(&mut self, id: &str, result: Value, event: bool) -> anyhow::Result<()> {
        self.record(Entry::Complete {
            id: id.into(),
            result,
            event,
            at_ms: now_ms(),
        })
    }

    /// Reserve `id` for a new operation, or return the result of the identical earlier one.
    fn reserve(
        &mut self,
        id: &str,
        payload: &impl Serialize,
        operation: impl FnOnce(Vec<u8>) -> Operation,
    ) -> anyhow::Result<Option<Value>> {
        ensure!(!id.is_empty() && id.len() <= 512, "invalid operation id");
        let fingerprint = Sha256::digest(serde_json::to_vec(payload)?).to_vec();
        if let Some(old) = self.state.operations.get(id) {
            ensure!(old.fingerprint == fingerprint, "OP_ID_REUSED");
            return Ok(Some(old.result.clone()));
        }
        self.record(Entry::Reserve {
            id: id.into(),
            operation: Box::new(operation(fingerprint)),
        })?;
        Ok(None)
    }
}

/// The journal head of a (frozen) guest, read from its files without touching them.
pub fn journal_head(directory: &std::path::Path) -> anyhow::Result<u64> {
    Ok(Log::<State>::read(directory, "journal")?.head)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Default)]
struct Folds {
    items: BTreeMap<String, String>,
    version: u64,
}

struct Job {
    stdin: Option<ChildStdin>,
    kill: mpsc::Sender<()>,
    pid: u32,
}

pub struct Guest {
    id: Uuid,
    /// The control directory: journal, lock and full command outputs.
    directory: PathBuf,
    journal: Mutex<Journal>,
    jobs: Mutex<BTreeMap<String, Job>>,
    native: Mutex<Option<Arc<Native>>>,
    folds: Mutex<Folds>,
    /// Signalled whenever an event, a result or streaming text arrives.
    changed: Notify,
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
        let (log, state) = Log::<State>::open(&directory, "journal")?;
        let mut journal = Journal { log, state };
        ensure!(
            journal.state.sandbox_id.is_none_or(|saved| saved == id),
            "SANDBOX_ID_MISMATCH"
        );
        if journal.state.sandbox_id.is_none() {
            journal.record(Entry::Sandbox(id))?;
        }
        // A guest crash is not an Owner restart: never rerun an effect of unknown outcome.
        let interrupted: Vec<String> = journal
            .state
            .operations
            .iter()
            .filter(|(_, op)| op.is_running())
            .map(|(id, _)| id.clone())
            .collect();
        for op in interrupted {
            let result = json!({"state":"outcome_unknown","error":"guest restarted while the operation was running"});
            journal.complete(&op, result, true)?;
        }
        Ok(Arc::new(Self {
            id,
            directory,
            journal: Mutex::new(journal),
            jobs: Mutex::new(BTreeMap::new()),
            native: Mutex::new(None),
            folds: Mutex::new(Folds::default()),
            changed: Notify::new(),
            key: tokio::sync::RwLock::new(None),
            _lock: process_lock,
        }))
    }

    pub async fn handle(self: &Arc<Self>, request: GuestRequest) -> anyhow::Result<Value> {
        ensure!(request.sandbox_id == self.id, "SANDBOX_ID_MISMATCH");
        let mut journal = self.journal.lock().await;
        ensure!(
            request.exec_epoch >= journal.state.epoch && request.exec_epoch > 0,
            "FENCED"
        );
        if matches!(request.command, GuestCommand::Fence) {
            if journal.state.epoch != request.exec_epoch {
                journal.record(Entry::Epoch(request.exec_epoch))?;
            }
            return Ok(json!({"epoch":journal.state.epoch}));
        }
        ensure!(request.exec_epoch == journal.state.epoch, "FENCE_REQUIRED");
        match request.command {
            GuestCommand::Fence => unreachable!(),
            GuestCommand::ProjectInstructions { text } => {
                drop(journal);
                install_project_instructions(&text)?;
                Ok(json!({"installed":true}))
            }
            GuestCommand::NativeStart {
                model,
                mcp,
                harness,
            } => {
                drop(journal);
                let mut native = self.native.lock().await;
                if native.is_none() {
                    *native = Some(Native::start(self.clone(), &model, mcp, &harness).await?);
                }
                Ok(json!({"running":true,"capabilities":crate::native::capabilities(&harness)}))
            }
            GuestCommand::Harness { op_id, request } => {
                let reserved = journal.reserve(&op_id, &request, |fingerprint| Operation {
                    request: Some(request.clone()),
                    native: true,
                    ..Operation::running(fingerprint)
                })?;
                if let Some(result) = reserved {
                    return Ok(result);
                }
                let native = self
                    .native
                    .lock()
                    .await
                    .clone()
                    .context("NATIVE_NOT_STARTED")?;
                match native.request(&op_id, &request) {
                    Some(message) => {
                        native.send(message).await?;
                        Ok(json!({"state":"running"}))
                    }
                    None => {
                        let result = json!({"state":"completed","result":HarnessResult::default()});
                        journal.complete(&op_id, result.clone(), false)?;
                        Ok(result)
                    }
                }
            }
            GuestCommand::Answer {
                op_id,
                request_id,
                answer,
            } => {
                let payload = (&request_id, &answer);
                if let Some(result) = journal.reserve(&op_id, &payload, Operation::running)? {
                    return Ok(result);
                }
                let native = self
                    .native
                    .lock()
                    .await
                    .clone()
                    .context("NATIVE_NOT_STARTED")?;
                native.send(Native::answer(&request_id, &answer)?).await?;
                let result = json!({"state":"completed"});
                journal.complete(&op_id, result.clone(), true)?;
                self.changed.notify_waiters();
                Ok(result)
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
                    !spec.cmd.is_empty()
                        && spec.cmd.len() <= 65536
                        && spec.env.len() <= 64
                        && spec.timeout_ms > 0
                        && spec.timeout_ms <= 600_000,
                    "INVALID_EXEC"
                );
                ensure!(!spec.pty, "PTY_UNSUPPORTED");
                let pending = json!({"state":"running","handle":op_id});
                let reserved = journal.reserve(&op_id, &spec, |fingerprint| Operation {
                    result: pending.clone(),
                    ..Operation::running(fingerprint)
                })?;
                if let Some(result) = reserved {
                    return Ok(result);
                }
                // The journal stays locked through dispatch, so a Fence cannot overtake it.
                self.spawn(op_id, spec).await?;
                Ok(pending)
            }
            GuestCommand::Poll { op_id, wait_ms } => {
                drop(journal);
                self.wait(wait_ms, |state, _| {
                    state
                        .operations
                        .get(&op_id)
                        .is_none_or(|op| !op.is_running())
                })
                .await;
                let journal = self.journal.lock().await;
                Ok(journal
                    .state
                    .operations
                    .get(&op_id)
                    .context("UNKNOWN_OPERATION")?
                    .result
                    .clone())
            }
            GuestCommand::Write {
                op_id,
                request_id,
                data,
            } => {
                ensure!(data.len() <= 65536, "STDIN_TOO_LARGE");
                let payload = json!({"kind":"write","op_id":op_id,"data":data});
                if let Some(result) = journal.reserve(&request_id, &payload, Operation::running)? {
                    return Ok(result);
                }
                let mut jobs = self.jobs.lock().await;
                let result = async {
                    let job = jobs.get_mut(&op_id).context("JOB_NOT_RUNNING")?;
                    let stdin = job.stdin.as_mut().context("STDIN_CLOSED")?;
                    tokio::time::timeout(Duration::from_secs(2), stdin.write_all(data.as_bytes()))
                        .await??;
                    Ok::<_, anyhow::Error>(json!({"state":"completed","written":data.len()}))
                }
                .await
                .unwrap_or_else(|_| json!({"state":"outcome_unknown","error":"stdin outcome unknown; do not automatically retry"}));
                journal.complete(&request_id, result.clone(), true)?;
                self.changed.notify_waiters();
                Ok(result)
            }
            GuestCommand::Kill { op_id, request_id } => {
                let payload = json!({"kind":"kill","op_id":op_id});
                if let Some(result) = journal.reserve(&request_id, &payload, Operation::running)? {
                    return Ok(result);
                }
                if let Some(job) = self.jobs.lock().await.get(&op_id) {
                    let _ = job.kill.try_send(());
                }
                let result = json!({"state":"completed","kill_requested":true});
                journal.complete(&request_id, result.clone(), true)?;
                self.changed.notify_waiters();
                Ok(result)
            }
            GuestCommand::ReadImage {
                path,
                max_edge,
                max_bytes,
            } => {
                drop(journal);
                ensure!(
                    !path.is_empty()
                        && path.len() <= 4096
                        && (64..=8000).contains(&max_edge)
                        && max_bytes <= 8 * 1024 * 1024,
                    "INVALID_IMAGE_REQUEST"
                );
                Ok(tokio::task::spawn_blocking(move || {
                    crate::picture::read(&path, max_edge, max_bytes)
                })
                .await?)
            }
            GuestCommand::Replay {
                after,
                wait_ms,
                folds_seen,
            } => {
                // Ahead of the journal means the journal was lost. Behind the acknowledged
                // position is fine: another Owner task projected (and committed) those events.
                ensure!(after <= journal.state.head, "INVALID_GSEQ");
                let after = after.max(journal.state.acked);
                drop(journal);
                self.replay(after, wait_ms, folds_seen).await
            }
            GuestCommand::Ack { through } => {
                ensure!(through <= journal.state.head, "INVALID_GSEQ");
                if through > journal.state.acked {
                    journal.record(Entry::Ack {
                        through,
                        at_ms: now_ms(),
                    })?;
                }
                Ok(json!({"acked":journal.state.acked}))
            }
        }
    }

    /// Wait up to `wait_ms` until `ready` holds. Never holds a lock while waiting.
    async fn wait(&self, wait_ms: u64, ready: impl Fn(&State, &Folds) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms.min(30_000));
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let journal = self.journal.lock().await;
                let folds = self.folds.lock().await;
                if ready(&journal.state, &folds) {
                    return;
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// Unacknowledged events after `after` (bounded by count and size) and the text of items
    /// still streaming. Waits for new events or text when there is nothing to report.
    async fn replay(&self, after: u64, wait_ms: u64, folds_seen: u64) -> anyhow::Result<Value> {
        self.wait(wait_ms, |state, folds| {
            state.head > after || folds.version != folds_seen
        })
        .await;
        let only_text = {
            let journal = self.journal.lock().await;
            journal.state.head == after
        };
        if only_text && folds_seen != self.folds.lock().await.version {
            tokio::time::sleep(FOLD_COALESCE).await;
        }
        let journal = self.journal.lock().await;
        let mut budget = REPLAY_BYTES;
        let mut events = vec![];
        for event in journal.state.events.iter().filter(|e| e.gseq > after) {
            let size = serde_json::to_vec(event).map_or(usize::MAX, |bytes| bytes.len());
            if events.len() == REPLAY_EVENTS || (!events.is_empty() && size > budget) {
                break;
            }
            budget = budget.saturating_sub(size);
            events.push(event.clone());
        }
        let folds = self.folds.lock().await;
        let texts: BTreeMap<&String, &String> = folds.items.iter().take(FOLD_ITEMS).collect();
        Ok(
            json!({"events":events,"head":journal.state.head,"folds":texts,"fold_version":folds.version}),
        )
    }

    /// One line of native harness output, already classified.
    pub(crate) async fn native_line(&self, incoming: Incoming) -> anyhow::Result<()> {
        match incoming {
            Incoming::Ignore | Incoming::AutoReply { .. } => return Ok(()),
            Incoming::Delta { item_id, text } => {
                let mut folds = self.folds.lock().await;
                let fold = folds.items.entry(item_id).or_default();
                if fold.len() + text.len() <= FOLD_BYTES {
                    fold.push_str(&text);
                }
                folds.version += 1;
            }
            Incoming::Response { id, response } => {
                let mut journal = self.journal.lock().await;
                let Some(operation) = journal.state.operations.get(&id) else {
                    return Ok(());
                };
                if !operation.is_running() {
                    return Ok(());
                }
                let result = match &operation.request {
                    Some(request) => match crate::native::result_of(request, &response) {
                        Ok(result) => json!({"state":"completed","result":result}),
                        Err(error) => json!({"state":"failed","error":error}),
                    },
                    None => json!({"state":"completed","response":response}),
                };
                journal.complete(&id, result, false)?;
            }
            Incoming::Event(event) => {
                let mut journal = self.journal.lock().await;
                if matches!(event, HarnessEvent::Exited { .. }) {
                    let pending: Vec<String> = journal
                        .state
                        .operations
                        .iter()
                        .filter(|(_, op)| op.native && op.is_running())
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in pending {
                        let result =
                            json!({"state":"outcome_unknown","error":"NATIVE_PROCESS_EXITED"});
                        journal.complete(&id, result, false)?;
                    }
                }
                if let HarnessEvent::Item {
                    phase: ItemPhase::Completed,
                    native_id,
                    ..
                } = &event
                {
                    self.folds.lock().await.items.remove(native_id);
                }
                journal.record(Entry::Event {
                    result: serde_json::to_value(&event)?,
                })?;
            }
        }
        self.changed.notify_waiters();
        Ok(())
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
        let this = self.clone();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                tokio::spawn(async move {
                    let result =
                        json!({"state":"completed","exit_code":127,"error":"process spawn failed"});
                    let _ = this.complete(&op, result).await;
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
        let stdout = child.stdout.take().context("stdout")?;
        let stderr = child.stderr.take().context("stderr")?;
        let directory = self.outputs()?;
        let filename = format!("{:x}", Sha256::digest(op.as_bytes()));
        let stdout_path = directory.join(format!("{filename}.stdout"));
        let stderr_path = directory.join(format!("{filename}.stderr"));
        tokio::spawn(async move {
            let out = tokio::spawn(capture(stdout, stdout_path.clone()));
            let err = tokio::spawn(capture(stderr, stderr_path.clone()));
            let (mut timed_out, mut cancelled) = (false, false);
            let group = Pid::from_raw(pid as i32);
            let status = tokio::select! {
                result = child.wait() => result,
                _ = tokio::time::sleep(Duration::from_millis(spec.timeout_ms)) => {
                    timed_out = true;
                    let _ = killpg(group, Signal::SIGKILL);
                    child.wait().await
                }
                _ = killed.recv() => {
                    cancelled = true;
                    let _ = killpg(group, Signal::SIGKILL);
                    child.wait().await
                }
            };
            // Descendants may still hold the pipes after the shell exits.
            let _ = killpg(group, Signal::SIGKILL);
            let output = out.await.ok().and_then(Result::ok).unwrap_or_default();
            let errors = err.await.ok().and_then(Result::ok).unwrap_or_default();
            let result = json!({
                "state":"completed","exit_code":status.ok().and_then(|s|s.code()),
                "timeout":timed_out,"cancelled":cancelled,
                "stdout_head":output.head,"stdout_tail":output.tail,
                "stderr_head":errors.head,"stderr_tail":errors.tail,
                "truncated":output.truncated||errors.truncated,
                "full_output_path":stdout_path,"stderr_path":stderr_path,
            });
            let _ = this.complete(&op, result).await;
        });
        Ok(())
    }

    fn outputs(&self) -> anyhow::Result<PathBuf> {
        let directory = self.directory.join("outputs");
        std::fs::create_dir_all(&directory)?;
        Ok(directory)
    }

    async fn complete(&self, op: &str, result: Value) -> anyhow::Result<()> {
        self.journal.lock().await.complete(op, result, true)?;
        self.jobs.lock().await.remove(op);
        self.changed.notify_waiters();
        Ok(())
    }

    pub async fn serve(self: Arc<Self>, socket: PathBuf) -> anyhow::Result<()> {
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:8090").await?;
        let app = crate::proxy::router(self.clone())?;
        tokio::spawn(async move {
            let _ = axum::serve(proxy, app).await;
        });
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        loop {
            let (stream, _) = listener.accept().await?;
            let this = self.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut read = BufReader::new(read).take(MAX_MESSAGE as u64 + 1);
                let mut line = String::new();
                let result = async {
                    tokio::time::timeout(Duration::from_secs(5), read.read_line(&mut line))
                        .await??;
                    ensure!(line.len() <= MAX_MESSAGE, "REQUEST_TOO_LARGE");
                    this.handle(serde_json::from_str(&line)?).await
                }
                .await;
                let _ = write.write_all(&Reply::from(result).line()).await;
            });
        }
    }

    pub async fn stop_jobs(&self) {
        for job in self.jobs.lock().await.values() {
            let _ = killpg(Pid::from_raw(job.pid as i32), Signal::SIGKILL);
        }
    }
}

/// Write the project instructions and make both harnesses' memory files import them.
fn install_project_instructions(text: &str) -> anyhow::Result<()> {
    ensure!(text.len() <= 600_000, "INVALID_PROJECT_INSTRUCTIONS");
    let dir = std::path::Path::new("/workspace/.platform");
    std::fs::create_dir_all(dir)?;
    let (project, memory) = text
        .rfind("\n<memory>\n<notice>")
        .map_or((text, ""), |at| text.split_at(at));
    journal::atomic_bytes(&dir.join("project.md"), project.as_bytes())?;
    journal::atomic_bytes(&dir.join("memory.md"), memory.as_bytes())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.join("memory.md"),
        std::fs::Permissions::from_mode(0o444),
    )?;
    for (name, import) in [
        (
            "AGENTS.md",
            "\n\n<!-- RyzHub project -->\nRead .platform/project.md for the current project instructions and .platform/memory.md for memory data. Knowledge files are read-only in /project.\n",
        ),
        (
            "CLAUDE.md",
            "\n\n<!-- RyzHub project -->\n@.platform/project.md\n@.platform/memory.md\n",
        ),
    ] {
        let path = std::path::Path::new("/workspace").join(name);
        let mut content = std::fs::read_to_string(&path).unwrap_or_default();
        if !content.contains("<!-- RyzHub project -->") {
            content.push_str(import);
            journal::atomic_bytes(&path, content.as_bytes())?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct Captured {
    head: String,
    tail: String,
    truncated: bool,
}

/// Keep the first and last 8 KiB for the model; the full output (up to 16 MiB) goes to disk.
async fn capture(mut reader: impl AsyncRead + Unpin, path: PathBuf) -> anyhow::Result<Captured> {
    const EDGE: usize = 8192;
    let mut file = tokio::fs::File::create(path).await?;
    let (mut head, mut tail, mut total) = (Vec::new(), Vec::new(), 0usize);
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
        let take = (EDGE - head.len()).min(n);
        head.extend_from_slice(&buffer[..take]);
        tail.extend_from_slice(&buffer[..n]);
        if tail.len() > EDGE {
            tail.drain(..tail.len() - EDGE);
        }
    }
    file.sync_all().await?;
    Ok(Captured {
        head: String::from_utf8_lossy(&head).into(),
        tail: if total > EDGE {
            String::from_utf8_lossy(&tail).into()
        } else {
            String::new()
        },
        truncated: total > EDGE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_exit_resolves_pending_requests_without_replaying_exec() {
        let directory = tempfile::tempdir().unwrap();
        let guest = Guest::open(Uuid::now_v7(), directory.path().into()).unwrap();
        {
            let mut journal = guest.journal.lock().await;
            journal
                .reserve("models", &"model/list", |fingerprint| Operation {
                    native: true,
                    ..Operation::running(fingerprint)
                })
                .unwrap();
            journal
                .reserve("exec", &"independent process", Operation::running)
                .unwrap();
        }
        guest
            .native_line(Incoming::Event(HarnessEvent::Exited {
                exit_code: Some(101),
                error: None,
            }))
            .await
            .unwrap();
        drop(guest);
        let (_, state) = Log::<State>::open(directory.path(), "journal").unwrap();
        assert_eq!(
            state.operations["models"].result["state"],
            "outcome_unknown"
        );
        // A guest restart turns the still-running exec into an unknown outcome, never a rerun.
        assert_eq!(state.operations["exec"].result["state"], "running");
        assert_eq!(state.events.last().unwrap().result["exit_code"], 101);
    }

    #[tokio::test]
    async fn acknowledged_completions_become_tombstones_and_replay_respects_the_byte_budget() {
        let directory = tempfile::tempdir().unwrap();
        let guest = Guest::open(Uuid::now_v7(), directory.path().into()).unwrap();
        {
            let mut journal = guest.journal.lock().await;
            for n in 0..3 {
                let id = format!("op{n}");
                journal.reserve(&id, &n, Operation::running).unwrap();
                let big = "x".repeat(REPLAY_BYTES);
                journal
                    .complete(&id, json!({"state":"completed","out":big}), true)
                    .unwrap();
            }
        }
        let first = guest.replay(0, 0, 0).await.unwrap();
        assert_eq!(first["events"].as_array().unwrap().len(), 1);
        let mut journal = guest.journal.lock().await;
        journal
            .record(Entry::Ack {
                through: 2,
                at_ms: now_ms(),
            })
            .unwrap();
        // Acknowledged results stay available for a while ...
        assert_eq!(journal.state.operations["op0"].result["state"], "completed");
        journal
            .record(Entry::Ack {
                through: 2,
                at_ms: now_ms() + RESULT_MS + 1,
            })
            .unwrap();
        // ... then only a tombstone is kept, and the operation is never executed again.
        assert_eq!(
            journal.state.operations["op0"].result["state"],
            "acknowledged"
        );
        assert_eq!(journal.state.operations["op2"].result["state"], "completed");
        let repeated = journal.reserve("op0", &0, Operation::running).unwrap();
        assert_eq!(repeated.unwrap()["state"], "acknowledged");
        // Tombstones expire eventually.
        journal
            .record(Entry::Ack {
                through: 3,
                at_ms: now_ms() + TOMBSTONE_MS + 1,
            })
            .unwrap();
        assert!(journal.state.operations.is_empty());
    }
}
