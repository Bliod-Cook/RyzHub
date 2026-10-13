use crate::*;
use anyhow::{Context, ensure};
use serde_json::json;
use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::UnixListener,
    sync::Mutex,
};

#[derive(Clone, Debug)]
pub struct Config {
    pub root: PathBuf,
    pub containerd: String,
    pub guest_binary: PathBuf,
    pub multi_tenant: bool,
    pub allowed_uid: u32,
    pub runsc_config: Option<PathBuf>,
    /// Egress connections each organization may open per minute.
    pub egress_per_minute: u32,
    /// The quota volume for writable layers; without one they are unlimited.
    pub volume: Option<VolumeConfig>,
}

/// Where the quota volume's image lives, how large it is and where it is mounted.
#[derive(Clone, Debug)]
pub struct VolumeConfig {
    pub image: PathBuf,
    pub size: u64,
    pub sparse: bool,
    pub mountpoint: PathBuf,
}

/// The disk limit of a sandbox whose spec names none.
pub const DEFAULT_DISK_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    spec: SandboxSpec,
    epoch: u64,
    state: String,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    sandboxes: BTreeMap<Uuid, Entry>,
    #[serde(default)]
    deleted_workspaces: BTreeSet<(Uuid, Uuid)>,
    /// The last quota project assigned; 0 is the volume's default project.
    #[serde(default)]
    last_project: u32,
    /// Destroyed sandboxes and when (ms), so a late Create cannot bring one back; kept for
    /// [`TOMBSTONE_MS`].
    #[serde(default)]
    destroyed: BTreeMap<Uuid, u64>,
}

/// How long a destroyed sandbox's tombstone is kept.
const TOMBSTONE_MS: u64 = 30 * 24 * 3600 * 1000;

impl State {
    fn project(&mut self) -> u32 {
        self.last_project += 1;
        self.last_project
    }
}

pub struct Host {
    config: Config,
    state: Mutex<State>,
    operations: Mutex<BTreeMap<Uuid, Weak<Mutex<()>>>>,
    checkpointing: std::sync::Mutex<BTreeSet<Uuid>>,
    /// Asynchronous checkpoint and restore jobs by artifact id: None while running.
    jobs: std::sync::Mutex<Jobs>,
    egress: Arc<crate::egress::Egress>,
    volume: Option<crate::quota::Volume>,
    /// Open terminal connections by sandbox, with the epoch each was attached under.
    terminals: std::sync::Mutex<BTreeMap<Uuid, Vec<(u64, tokio::task::AbortHandle)>>>,
    _lock: std::fs::File,
}

/// The reported state of an asynchronous job.
type Jobs = BTreeMap<Uuid, Option<Result<Value, String>>>;

/// The state of `artifact`'s job, if one is known. A failure is reported once and then
/// forgotten, so the caller's retry with the same artifact starts the job again instead of
/// replaying a failure whose cause may be gone.
fn poll_job(jobs: &mut Jobs, artifact: Uuid) -> Option<Value> {
    match jobs.get(&artifact)? {
        Some(Err(_)) => jobs.remove(&artifact).map(|job| job_state(&job)),
        job => Some(job_state(job)),
    }
}

/// Register `artifact`'s job as running unless it is already known (then its state). Checked
/// and claimed under one lock, so concurrent requests start the job once.
fn claim_job(jobs: &mut Jobs, artifact: Uuid) -> Option<Value> {
    let state = poll_job(jobs, artifact);
    if state.is_none() {
        jobs.insert(artifact, None);
    }
    state
}

fn job_state(result: &Option<Result<Value, String>>) -> Value {
    match result {
        None => json!({"state":"running"}),
        Some(Ok(value)) => {
            let mut value = value.clone();
            value["state"] = json!("completed");
            value
        }
        Some(Err(error)) => json!({"state":"failed","error":error}),
    }
}

struct CheckpointActivity<'a> {
    active: &'a std::sync::Mutex<BTreeSet<Uuid>>,
    id: Uuid,
}

impl Drop for CheckpointActivity<'_> {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(&self.id);
    }
}

/// The control channel to one sandbox's guest agent.
pub struct GuestChannel {
    socket: PathBuf,
}

impl GuestChannel {
    /// One request and its reply.
    pub async fn call(&self, request: &impl serde::Serialize) -> anyhow::Result<Value> {
        call(&self.socket, request).await
    }

    /// A request whose connection stays open after the reply (see [`crate::attach`]).
    pub async fn attach(
        &self,
        request: &impl serde::Serialize,
    ) -> anyhow::Result<(Value, crate::ReadSide, crate::WriteSide)> {
        crate::attach(&self.socket, request).await
    }
}

pub trait SandboxBackend {
    fn capabilities(&self) -> Value;
    /// The control channel to the guest agent of sandbox `id`.
    fn connect(
        &self,
        id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<GuestChannel>> + Send;
    fn create(
        &self,
        spec: SandboxSpec,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + Send;
    fn stop(
        &self,
        id: Uuid,
        epoch: u64,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + Send;
    fn destroy(
        &self,
        id: Uuid,
        epoch: u64,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + Send;
}

impl Host {
    pub async fn open(config: Config) -> anyhow::Result<Arc<Self>> {
        ensure!(
            config.root.is_absolute() && config.guest_binary.is_absolute(),
            "ABSOLUTE_PATH_REQUIRED"
        );
        std::fs::create_dir_all(&config.root)?;
        let process_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(config.root.join("host.lock"))?;
        process_lock.try_lock()?;
        let state: State = journal::load(&config.root.join("host.json"))?;
        let egress = crate::egress::Egress::new(config.egress_per_minute);
        let volume = match &config.volume {
            Some(v) => Some(
                crate::quota::Volume::prepare(&v.image, v.size, v.sparse, &v.mountpoint).await?,
            ),
            None => None,
        };
        let host = Arc::new(Self {
            config,
            state: Mutex::new(state),
            operations: Mutex::new(BTreeMap::new()),
            checkpointing: std::sync::Mutex::new(BTreeSet::new()),
            jobs: std::sync::Mutex::new(BTreeMap::new()),
            egress,
            volume,
            terminals: std::sync::Mutex::new(BTreeMap::new()),
            _lock: process_lock,
        });
        let _ = host.ctr(&["namespaces", "create", "ryzhub"]).await;
        // Sandboxes outlive the host process; their egress proxies restart with it.
        let running: Vec<SandboxSpec> = host
            .state
            .lock()
            .await
            .sandboxes
            .values()
            .filter(|entry| entry.state == "running")
            .map(|entry| entry.spec.clone())
            .collect();
        for spec in running {
            host.start_egress(&spec)?;
        }
        Ok(host)
    }

    async fn ctr(&self, args: &[&str]) -> anyhow::Result<String> {
        let mut all = vec![
            "--address",
            &self.config.containerd,
            "--namespace",
            "ryzhub",
        ];
        all.extend_from_slice(args);
        run("ctr", &all).await
    }

    /// Where writable data goes: the quota volume when there is one.
    fn data(&self) -> &Path {
        self.volume
            .as_ref()
            .map_or(&self.config.root, |v| &v.mountpoint)
    }

    /// `relative` under the data directory, unless it exists only where it was kept before the
    /// node had a quota volume.
    fn located(&self, relative: &Path) -> PathBuf {
        let current = self.data().join(relative);
        let legacy = self.config.root.join(relative);
        if !current.exists() && legacy.exists() {
            legacy
        } else {
            current
        }
    }

    fn dir(&self, id: Uuid) -> PathBuf {
        self.located(&Path::new("sandboxes").join(id.to_string()))
    }

    /// Create `spec`'s sandbox directory, its writable layer limited to the spec's disk size.
    fn create_dirs(&self, state: &mut State, spec: &SandboxSpec) -> anyhow::Result<PathBuf> {
        let dir = self.dir(spec.id);
        let new = !dir.exists();
        for sub in ["base", "upper", "work", "merged", "control"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        if new && let Some(volume) = &self.volume {
            let bytes = disk_bytes(spec);
            volume.assign(
                &[&dir.join("upper"), &dir.join("work")],
                state.project(),
                bytes,
            )?;
            journal::save(&self.config.root.join("host.json"), &*state)?;
        }
        Ok(dir)
    }

    async fn operation(&self, id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut operations = self.operations.lock().await;
            operations.retain(|_, lock| lock.strong_count() > 0);
            match operations.get(&id).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(Mutex::new(()));
                    operations.insert(id, Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }

    async fn forward(&self, request: GuestRequest) -> anyhow::Result<Value> {
        // Reads (including long polls) never wait behind, or block, other operations.
        let _operation = if request.command.read_only() {
            None
        } else {
            Some(self.operation(request.sandbox_id).await)
        };
        let mut state = self.state.lock().await;
        let entry = state
            .sandboxes
            .get_mut(&request.sandbox_id)
            .context("UNKNOWN_SANDBOX")?;
        ensure!(
            entry.state == "running" && request.exec_epoch >= entry.epoch,
            "FENCED"
        );
        if matches!(request.command, GuestCommand::Fence) {
            if entry.epoch != request.exec_epoch {
                entry.epoch = request.exec_epoch;
                journal::save(&self.config.root.join("host.json"), &*state)?;
                self.cut_terminals(request.sandbox_id, request.exec_epoch);
            }
        } else {
            ensure!(request.exec_epoch == entry.epoch, "FENCE_REQUIRED");
        }
        drop(state);
        // Hold the sandbox fence through guest acknowledgement. New dispatch follows this barrier.
        self.connect(request.sandbox_id).await?.call(&request).await
    }

    pub async fn handle(self: &Arc<Self>, command: HostCommand) -> anyhow::Result<Value> {
        match command {
            HostCommand::Project {
                sandbox_id,
                exec_epoch,
                files,
            } => {
                let _operation = self.operation(sandbox_id).await;
                let state = self.state.lock().await;
                let entry = state
                    .sandboxes
                    .get(&sandbox_id)
                    .context("UNKNOWN_SANDBOX")?;
                ensure!(
                    entry.epoch == exec_epoch && entry.state == "running",
                    "FENCED"
                );
                drop(state);
                ensure!(
                    files.values().map(String::len).sum::<usize>() <= 2 * 1024 * 1024,
                    "PROJECT_FILE_LIMIT"
                );
                let directory = self.dir(sandbox_id).join("project");
                for name in files.keys() {
                    ensure!(
                        !name.is_empty()
                            && name.len() <= 128
                            && !name.starts_with('.')
                            && !name.contains(['/', '\\', '\0']),
                        "INVALID_PROJECT_FILE"
                    );
                }
                std::fs::create_dir_all(&directory)?;
                for file in std::fs::read_dir(&directory)? {
                    std::fs::remove_file(file?.path())?;
                }
                for (name, content) in files {
                    journal::atomic_bytes(&directory.join(name), content.as_bytes())?;
                }
                Ok(json!({"mounted":true}))
            }
            HostCommand::Attachments {
                sandbox_id,
                exec_epoch,
                files,
            } => {
                let _operation = self.operation(sandbox_id).await;
                let spec = self.fenced(sandbox_id, exec_epoch).await?;
                for (name, file) in &files {
                    valid_attachment(name, file)?;
                }
                let directory = self.attachments(&spec);
                std::fs::create_dir_all(&directory)?;
                let mut manifest: BTreeMap<String, String> =
                    journal::load(&directory.with_extension("json"))?;
                for entry in std::fs::read_dir(&directory)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let current = files.get(&name).is_some_and(|file| {
                        manifest.get(&name) == Some(&file.sha256)
                            && entry.metadata().is_ok_and(|m| m.len() == file.size)
                    });
                    if !current {
                        std::fs::remove_file(entry.path())?;
                        manifest.remove(&name);
                    }
                }
                manifest.retain(|name, _| files.contains_key(name));
                journal::save(&directory.with_extension("json"), &manifest)?;
                let missing: Vec<&String> = files
                    .keys()
                    .filter(|name| !manifest.contains_key(*name))
                    .collect();
                Ok(json!({"missing": missing}))
            }
            HostCommand::AttachmentUpload { .. } => {
                anyhow::bail!("attachment uploads need a dedicated connection")
            }
            HostCommand::Create { spec } => self.create(spec).await,
            HostCommand::WorkspaceCheckpoint {
                org_id,
                workspace_id,
                artifact,
            } => self.start_workspace_checkpoint(org_id, workspace_id, artifact),
            HostCommand::EvictWorkspace {
                org_id,
                workspace_id,
            } => {
                let state = self.state.lock().await;
                ensure!(
                    !state
                        .sandboxes
                        .values()
                        .any(|s| s.spec.org_id == org_id && s.spec.workspace_id == workspace_id),
                    "WORKSPACE_BUSY"
                );
                drop(state);
                let workspace = workspace_path(org_id, workspace_id);
                let attachments = self
                    .config
                    .root
                    .join("org")
                    .join(org_id.to_string())
                    .join("attachments")
                    .join(workspace_id.to_string());
                for path in [
                    self.config.root.join(&workspace),
                    self.data().join(&workspace),
                    attachments.with_extension("parts"),
                    attachments.clone(),
                ] {
                    match tokio::fs::remove_dir_all(path).await {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                        _ => {}
                    }
                }
                match tokio::fs::remove_file(attachments.with_extension("json")).await {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                }
                Ok(json!({"evicted": true}))
            }
            HostCommand::Images => {
                let listing = self.ctr(&["images", "list", "-q"]).await?;
                let state = self.state.lock().await;
                let used = |reference: &str| {
                    state.sandboxes.values().any(|entry| {
                        entry.spec.image == reference
                            || entry.spec.deps.as_deref() == Some(reference)
                    })
                };
                let images: Vec<Value> = listing
                    .lines()
                    .map(str::trim)
                    .filter(|reference| pinned(reference))
                    .map(|reference| json!({"reference": reference, "in_use": used(reference)}))
                    .collect();
                Ok(json!({ "images": images }))
            }
            HostCommand::RemoveImage { reference } => {
                ensure!(pinned(&reference), "IMAGE_DIGEST_REQUIRED");
                let state = self.state.lock().await;
                ensure!(
                    !state
                        .sandboxes
                        .values()
                        .any(|entry| entry.spec.image == reference
                            || entry.spec.deps.as_deref() == Some(reference.as_str())),
                    "IMAGE_IN_USE"
                );
                // Held while removing, so no Create can start using the image meanwhile.
                self.ctr(&["images", "rm", &reference]).await?;
                drop(state);
                Ok(json!({"removed": true}))
            }
            HostCommand::WorkspaceStatus {
                org_id,
                workspace_id,
            } => {
                Ok(json!({"exists": self.located(&workspace_path(org_id, workspace_id)).exists()}))
            }
            HostCommand::WorkspaceRestore {
                org_id,
                workspace_id,
                artifact,
                sha256,
                disk_bytes,
            } => {
                if let Some(state) = claim_job(&mut self.jobs.lock().unwrap(), artifact) {
                    return Ok(state);
                }
                let this = self.clone();
                tokio::spawn(async move {
                    let result = this
                        .workspace_restore(org_id, workspace_id, artifact, sha256, disk_bytes)
                        .await;
                    this.jobs
                        .lock()
                        .unwrap()
                        .insert(artifact, Some(result.map_err(|e| e.to_string())));
                });
                Ok(job_state(&None))
            }
            HostCommand::Egress { boot, after } => {
                let (boot, records) = self.egress.collect(boot, after, 1000);
                Ok(json!({"boot": boot, "records": records}))
            }
            HostCommand::Status => {
                let tasks = self.ctr(&["tasks", "list"]).await?;
                let state = self.state.lock().await;
                let checkpointing = self.checkpointing.lock().unwrap();
                Ok(
                    json!({"running":state.sandboxes.keys().filter(|id|tasks.lines().any(|l|task_heartbeat(l, &id.to_string(), checkpointing.contains(id)))).collect::<Vec<_>>()}),
                )
            }
            HostCommand::Checkpoint {
                sandbox_id,
                exec_epoch,
                artifact,
            } => {
                self.start_checkpoint(sandbox_id, exec_epoch, artifact)
                    .await
            }
            HostCommand::Restore {
                spec,
                artifact,
                sha256,
            } => self.start_restore(spec, artifact, sha256),
            HostCommand::ArtifactDownload { .. }
            | HostCommand::ArtifactUpload { .. }
            | HostCommand::WorkspaceFile { .. }
            | HostCommand::TerminalAttach { .. } => {
                anyhow::bail!("streams need a dedicated connection")
            }
            HostCommand::DeleteArtifact { artifact } => {
                for path in [
                    self.artifact(artifact),
                    self.artifact(artifact).with_extension("json"),
                    self.artifact(artifact).with_extension("part"),
                ] {
                    match tokio::fs::remove_file(path).await {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                self.jobs.lock().unwrap().remove(&artifact);
                Ok(json!({"deleted":true}))
            }
            HostCommand::Guest { request } => self.forward(request).await,
            HostCommand::Stop {
                sandbox_id,
                exec_epoch,
            } => self.stop(sandbox_id, exec_epoch).await,
            HostCommand::Destroy {
                sandbox_id,
                exec_epoch,
            } => self.destroy(sandbox_id, exec_epoch).await,
            HostCommand::PurgeWorkspace {
                org_id,
                workspace_id,
                artifacts,
            } => {
                let mut state = self.state.lock().await;
                ensure!(
                    !state
                        .sandboxes
                        .values()
                        .any(|s| s.spec.org_id == org_id && s.spec.workspace_id == workspace_id),
                    "WORKSPACE_BUSY"
                );
                state.deleted_workspaces.insert((org_id, workspace_id));
                journal::save(&self.config.root.join("host.json"), &*state)?;
                // The durable tombstone fences Create; filesystem reclamation must
                // not hold the global lock needed by other sandboxes' heartbeats.
                drop(state);
                let workspace = workspace_path(org_id, workspace_id);
                let paths = [
                    self.config.root.join(&workspace),
                    self.data().join(&workspace),
                ];
                let attachments = self
                    .config
                    .root
                    .join("org")
                    .join(org_id.to_string())
                    .join("attachments")
                    .join(workspace_id.to_string());
                for path in paths
                    .into_iter()
                    .chain([attachments.with_extension("parts"), attachments.clone()])
                {
                    match tokio::fs::remove_dir_all(path).await {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                match tokio::fs::remove_file(attachments.with_extension("json")).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                for id in artifacts {
                    for path in [self.artifact(id), self.artifact(id).with_extension("json")] {
                        match tokio::fs::remove_file(path).await {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
                Ok(json!({"purged":true}))
            }
        }
    }

    /// On the data volume, beside the layers it is copied from, so copies can be reflinks.
    fn artifact(&self, id: Uuid) -> PathBuf {
        self.data().join("artifacts").join(format!("{id}.tar"))
    }

    /// The workspace's `/attachments`, outside the sandbox's own directory so it outlives it.
    /// Beside it: `<dir>.json`, each file's content address, and `<dir>.parts`, uploads in
    /// progress.
    fn attachments(&self, spec: &SandboxSpec) -> PathBuf {
        self.config
            .root
            .join("org")
            .join(spec.org_id.to_string())
            .join("attachments")
            .join(spec.workspace_id.to_string())
    }

    /// The workspace directory a sandbox without its own copy mounts at `/workspace`.
    fn workspace(&self, spec: &SandboxSpec) -> PathBuf {
        self.located(&workspace_path(spec.org_id, spec.workspace_id))
    }

    /// Create `spec`'s workspace directory, limited to the spec's disk size.
    fn create_workspace(&self, state: &mut State, spec: &SandboxSpec) -> anyhow::Result<PathBuf> {
        let workspace = self.workspace(spec);
        if !workspace.exists() {
            std::fs::create_dir_all(&workspace)?;
            if let Some(volume) = &self.volume {
                volume.assign(&[&workspace], state.project(), disk_bytes(spec))?;
                journal::save(&self.config.root.join("host.json"), &*state)?;
            }
        }
        Ok(workspace)
    }

    /// The spec of a running sandbox whose current execution epoch is `exec_epoch`.
    async fn fenced(&self, sandbox_id: Uuid, exec_epoch: u64) -> anyhow::Result<SandboxSpec> {
        let state = self.state.lock().await;
        let entry = state
            .sandboxes
            .get(&sandbox_id)
            .context("UNKNOWN_SANDBOX")?;
        ensure!(
            entry.epoch == exec_epoch && entry.state == "running",
            "FENCED"
        );
        Ok(entry.spec.clone())
    }

    /// Receive one `/attachments` file, verified before it becomes visible.
    async fn attachment_upload(
        &self,
        sandbox_id: Uuid,
        exec_epoch: u64,
        name: &str,
        file: AttachmentFile,
        body: impl tokio::io::AsyncRead + Unpin,
    ) -> anyhow::Result<Value> {
        let _operation = self.operation(sandbox_id).await;
        let spec = self.fenced(sandbox_id, exec_epoch).await?;
        valid_attachment(name, &file)?;
        let directory = self.attachments(&spec);
        let parts = directory.with_extension("parts");
        std::fs::create_dir_all(&directory)?;
        std::fs::create_dir_all(&parts)?;
        let partial = parts.join(Uuid::now_v7().to_string());
        let mut out = tokio::fs::File::create(&partial).await?;
        let copied = copy_exact(body, &mut out, file.size).await;
        let result = async {
            ensure!(
                copied? == file.size,
                "attachment ended before {} bytes",
                file.size
            );
            out.sync_all().await?;
            let check = partial.clone();
            let (actual, _) =
                tokio::task::spawn_blocking(move || crate::archive::digest(&check)).await??;
            ensure!(actual == file.sha256, "CHECKSUM_MISMATCH");
            std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o444))?;
            tokio::fs::rename(&partial, directory.join(name)).await?;
            let path = directory.with_extension("json");
            let mut manifest: BTreeMap<String, String> = journal::load(&path)?;
            manifest.insert(name.to_owned(), file.sha256.clone());
            journal::save(&path, &manifest)?;
            Ok(json!({"size": file.size}))
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        result
    }

    /// Report on `artifact`'s checkpoint, starting it on first request. The sandbox is frozen
    /// only for the local copy; encoding runs after it resumes.
    async fn start_checkpoint(
        self: &Arc<Self>,
        id: Uuid,
        epoch: u64,
        artifact: Uuid,
    ) -> anyhow::Result<Value> {
        let manifest = self.artifact(artifact).with_extension("json");
        if manifest.exists() {
            let value: Value = serde_json::from_slice(&std::fs::read(&manifest)?)?;
            ensure!(
                value["sandbox_id"] == id.to_string()
                    && value["epoch"].as_u64().is_some_and(|old| old <= epoch),
                "ARTIFACT_ID_REUSED"
            );
            return Ok(job_state(&Some(Ok(value))));
        }
        if let Some(state) = poll_job(&mut self.jobs.lock().unwrap(), artifact) {
            return Ok(state);
        }
        {
            let state = self.state.lock().await;
            let entry = state.sandboxes.get(&id).context("UNKNOWN_SANDBOX")?;
            ensure!(
                entry.epoch == epoch && entry.state == "running" && entry.spec.persistent,
                "FENCED"
            );
        }
        if let Some(state) = claim_job(&mut self.jobs.lock().unwrap(), artifact) {
            return Ok(state);
        }
        let this = self.clone();
        tokio::spawn(async move {
            let result = this.checkpoint(id, epoch, artifact).await;
            this.jobs
                .lock()
                .unwrap()
                .insert(artifact, Some(result.map_err(|e| e.to_string())));
        });
        Ok(job_state(&None))
    }

    async fn checkpoint(&self, id: Uuid, epoch: u64, artifact: Uuid) -> anyhow::Result<Value> {
        let _operation = self.operation(id).await;
        let (image, deps) = {
            let state = self.state.lock().await;
            let entry = state.sandboxes.get(&id).context("UNKNOWN_SANDBOX")?;
            ensure!(
                entry.epoch == epoch && entry.state == "running" && entry.spec.persistent,
                "FENCED"
            );
            (entry.spec.image.clone(), entry.spec.deps.clone())
        };
        std::fs::create_dir_all(self.data().join("artifacts"))?;
        let dir = self.dir(id);
        let local = self.artifact(artifact).with_extension("upper");
        if local.exists() {
            std::fs::remove_dir_all(&local)?;
        }
        let captured_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        self.checkpointing.lock().unwrap().insert(id);
        let activity = CheckpointActivity {
            active: &self.checkpointing,
            id,
        };
        self.ctr(&["tasks", "pause", &id.to_string()]).await?;
        let frozen = async {
            run("sync", &["-f", dir.join("merged").to_str().unwrap()]).await?;
            std::fs::create_dir_all(&local)?;
            // Reflinks make the frozen window independent of the upper's size where supported.
            run(
                "cp",
                &[
                    "-a",
                    "--reflink=auto",
                    &format!("{}/.", dir.join("upper").display()),
                    local.to_str().unwrap(),
                ],
            )
            .await?;
            // The watermark: every guest event up to here is reflected in the copied files.
            crate::guest::journal_head(&dir.join("control"))
        }
        .await;
        let thaw = self.ctr(&["tasks", "resume", &id.to_string()]).await;
        drop(activity);
        let gseq = frozen?;
        thaw?;
        let archive = self.artifact(artifact);
        let source = local.clone();
        let path = archive.clone();
        let (sha256, size) = tokio::task::spawn_blocking(move || {
            crate::archive::export(&source, &path)?;
            crate::archive::digest(&path)
        })
        .await??;
        let excluded = crate::archive::excluded(&local)?;
        let value = json!({"format":1,"encoding":"tar.zst","captured_at_ms":captured_at_ms,"sandbox_id":id,
            "epoch":epoch,"image":image,"deps":deps,"gseq":gseq,"sha256":sha256,"size":size,"excluded":excluded});
        journal::save(&self.artifact(artifact).with_extension("json"), &value)?;
        std::fs::remove_dir_all(local)?;
        Ok(value)
    }

    /// Report on the workspace checkpoint into `artifact`, starting it on first request.
    fn start_workspace_checkpoint(
        self: &Arc<Self>,
        org: Uuid,
        workspace: Uuid,
        artifact: Uuid,
    ) -> anyhow::Result<Value> {
        let manifest = self.artifact(artifact).with_extension("json");
        if manifest.exists() {
            let value: Value = serde_json::from_slice(&std::fs::read(&manifest)?)?;
            ensure!(
                value["workspace_id"] == workspace.to_string(),
                "ARTIFACT_ID_REUSED"
            );
            return Ok(job_state(&Some(Ok(value))));
        }
        if let Some(state) = claim_job(&mut self.jobs.lock().unwrap(), artifact) {
            return Ok(state);
        }
        let this = self.clone();
        tokio::spawn(async move {
            let result = this.workspace_checkpoint(org, workspace, artifact).await;
            this.jobs
                .lock()
                .unwrap()
                .insert(artifact, Some(result.map_err(|e| e.to_string())));
        });
        Ok(job_state(&None))
    }

    /// Archive a workspace directory. It is copied (as reflinks where the filesystem has them)
    /// and the copy archived, so the archive is one moment's files. Nothing runs in the
    /// workspace then: its Owner checkpoints it only between runs.
    async fn workspace_checkpoint(
        &self,
        org: Uuid,
        workspace: Uuid,
        artifact: Uuid,
    ) -> anyhow::Result<Value> {
        ensure!(
            !self
                .state
                .lock()
                .await
                .deleted_workspaces
                .contains(&(org, workspace)),
            "WORKSPACE_DELETED"
        );
        let dir = self.located(&workspace_path(org, workspace));
        ensure!(dir.exists(), "UNKNOWN_WORKSPACE");
        std::fs::create_dir_all(self.data().join("artifacts"))?;
        let local = self.artifact(artifact).with_extension("workspace");
        if local.exists() {
            std::fs::remove_dir_all(&local)?;
        }
        let captured_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        std::fs::create_dir_all(&local)?;
        run(
            "cp",
            &[
                "-a",
                "--reflink=auto",
                &format!("{}/.", dir.display()),
                local.to_str().context("INVALID_PATH")?,
            ],
        )
        .await?;
        let archive = self.artifact(artifact);
        let (source, path) = (local.clone(), archive.clone());
        let (sha256, size) = tokio::task::spawn_blocking(move || {
            crate::archive::export_tree(&source, &path, crate::archive::Tree::Directory)?;
            crate::archive::digest(&path)
        })
        .await??;
        let excluded = crate::archive::excluded_in(&local, crate::archive::Tree::Directory)?;
        let value = json!({"format":1,"encoding":"tar.zst","scope":"workspace",
            "captured_at_ms":captured_at_ms,"workspace_id":workspace,"sha256":sha256,"size":size,
            "excluded":excluded});
        journal::save(&archive.with_extension("json"), &value)?;
        std::fs::remove_dir_all(local)?;
        Ok(value)
    }

    /// Unpack `artifact` as the workspace's directory if this node has none. It is unpacked
    /// beside the destination (under the same quota) and renamed into place, so a failure
    /// leaves no partial workspace.
    async fn workspace_restore(
        &self,
        org: Uuid,
        workspace: Uuid,
        artifact: Uuid,
        sha256: String,
        disk_bytes: u64,
    ) -> anyhow::Result<Value> {
        let relative = workspace_path(org, workspace);
        let target = self.located(&relative);
        if target.exists() {
            return Ok(json!({"restored": false}));
        }
        let path = self.artifact(artifact);
        let digest_path = path.clone();
        let (actual, _) =
            tokio::task::spawn_blocking(move || crate::archive::digest(&digest_path)).await??;
        ensure!(actual == sha256, "CHECKSUM_MISMATCH");
        let staging = self.data().join(&relative).with_extension("restoring");
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;
        {
            let mut state = self.state.lock().await;
            ensure!(
                !state.deleted_workspaces.contains(&(org, workspace)),
                "WORKSPACE_DELETED"
            );
            if let Some(volume) = &self.volume {
                let bytes = match disk_bytes {
                    0 => DEFAULT_DISK_BYTES,
                    bytes => bytes,
                };
                volume.assign(&[&staging], state.project(), bytes)?;
                journal::save(&self.config.root.join("host.json"), &*state)?;
            }
        }
        let unpacked = staging.clone();
        tokio::task::spawn_blocking(move || crate::archive::restore(&path, &unpacked)).await??;
        std::fs::rename(&staging, self.data().join(&relative))?;
        Ok(json!({"restored": true}))
    }

    /// Report on `artifact`'s restore, starting it on first request.
    fn start_restore(
        self: &Arc<Self>,
        spec: SandboxSpec,
        artifact: Uuid,
        sha256: String,
    ) -> anyhow::Result<Value> {
        ensure!(spec.persistent, "WORK_ONLY");
        if let Some(state) = claim_job(&mut self.jobs.lock().unwrap(), artifact) {
            return Ok(state);
        }
        let this = self.clone();
        tokio::spawn(async move {
            let result = this.restore(spec, artifact, sha256).await;
            this.jobs
                .lock()
                .unwrap()
                .insert(artifact, Some(result.map_err(|e| e.to_string())));
        });
        Ok(job_state(&None))
    }

    async fn restore(
        &self,
        spec: SandboxSpec,
        artifact: Uuid,
        sha256: String,
    ) -> anyhow::Result<Value> {
        let _operation = self.operation(spec.id).await;
        let path = self.artifact(artifact);
        let digest_path = path.clone();
        let (actual, _) =
            tokio::task::spawn_blocking(move || crate::archive::digest(&digest_path)).await??;
        ensure!(actual == sha256, "CHECKSUM_MISMATCH");
        // The writable layer gets its limit before the checkpoint is unpacked into it.
        let dir = self.create_dirs(&mut *self.state.lock().await, &spec)?;
        if !dir.join("restored.json").exists() {
            let upper = dir.join("upper");
            tokio::task::spawn_blocking(move || crate::archive::restore(&path, &upper)).await??;
            journal::save(
                &dir.join("restored.json"),
                &json!({"artifact":artifact,"sha256":sha256}),
            )?;
        }
        self.create_inner(spec).await
    }

    /// Stream an artifact to the caller after a `{"size":n}` header.
    async fn download(&self, artifact: Uuid, write: &mut crate::WriteSide) -> anyhow::Result<()> {
        let path = self.artifact(artifact);
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(error) => {
                write
                    .write_all(&Reply::from(Err(error.into())).line())
                    .await?;
                return Ok(());
            }
        };
        let size = file.metadata().await?.len();
        write
            .write_all(&Reply::from(Ok(json!({"size":size}))).line())
            .await?;
        copy_exact(file, write, size).await?;
        write.flush().await?;
        Ok(())
    }

    /// Stream a file of a sandbox's `/workspace` to the caller after a `{"size":n}` header.
    /// A refusal replies instead of the header.
    async fn workspace_file(
        &self,
        sandbox_id: Uuid,
        exec_epoch: u64,
        path: String,
        write: &mut crate::WriteSide,
    ) -> anyhow::Result<()> {
        let opened = async {
            let spec = self.fenced(sandbox_id, exec_epoch).await?;
            // A persistent sandbox's `/workspace` is a directory of its own overlay.
            let root = if spec.persistent {
                self.dir(sandbox_id).join("merged/workspace")
            } else {
                self.workspace(&spec)
            };
            let (file, size) =
                tokio::task::spawn_blocking(move || open_inside(&root, &path, MAX_ATTACHMENT))
                    .await?
                    .map_err(|code| anyhow::anyhow!(code))?;
            Ok::<_, anyhow::Error>((tokio::fs::File::from_std(file), size))
        }
        .await;
        let (file, size) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                write.write_all(&Reply::from(Err(error)).line()).await?;
                return Ok(());
            }
        };
        write
            .write_all(&Reply::from(Ok(json!({"size":size}))).line())
            .await?;
        copy_exact(file, write, size).await?;
        write.flush().await?;
        Ok(())
    }

    /// Receive `size` bytes into the artifact, verifying the digest before it becomes visible.
    async fn upload(
        &self,
        artifact: Uuid,
        size: u64,
        sha256: &str,
        body: impl tokio::io::AsyncRead + Unpin,
    ) -> anyhow::Result<Value> {
        ensure!(size <= 64 * 1024 * 1024 * 1024, "ARTIFACT_TOO_LARGE");
        std::fs::create_dir_all(self.data().join("artifacts"))?;
        let path = self.artifact(artifact);
        let partial = path.with_extension("part");
        let mut file = tokio::fs::File::create(&partial).await?;
        let copied = copy_exact(body, &mut file, size).await?;
        ensure!(
            copied == size,
            "artifact ended after {copied} of {size} bytes"
        );
        file.sync_all().await?;
        drop(file);
        let check = partial.clone();
        let (actual, _) =
            tokio::task::spawn_blocking(move || crate::archive::digest(&check)).await??;
        ensure!(actual == sha256, "CHECKSUM_MISMATCH");
        tokio::fs::rename(&partial, &path).await?;
        Ok(json!({"size":size}))
    }

    pub async fn serve(self: Arc<Self>, socket: PathBuf) -> anyhow::Result<()> {
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        if nix::unistd::getuid().is_root() {
            run(
                "chown",
                &[
                    &self.config.allowed_uid.to_string(),
                    socket.to_str().context("INVALID_PATH")?,
                ],
            )
            .await?;
        }
        loop {
            let (stream, _) = listener.accept().await?;
            let uid = stream.peer_cred()?.uid();
            if uid != self.config.allowed_uid && uid != 0 {
                continue;
            }
            tokio::spawn(self.clone().connection(Box::new(stream)));
        }
    }

    /// Serve remote control planes over TLS at `address`, accepting clients whose certificate
    /// `acceptor` trusts.
    pub async fn serve_tls(
        self: Arc<Self>,
        address: String,
        acceptor: tokio_rustls::TlsAcceptor,
    ) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(&address).await?;
        loop {
            let (tcp, _) = listener.accept().await?;
            let (this, acceptor) = (self.clone(), acceptor.clone());
            tokio::spawn(async move {
                let handshake =
                    tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(tcp));
                if let Ok(Ok(stream)) = handshake.await {
                    this.connection(Box::new(stream)).await;
                }
            });
        }
    }

    /// One request on `stream`: its reply, or a stream that follows it.
    async fn connection(self: Arc<Self>, stream: Box<dyn crate::Stream>) {
        let (read, mut write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        let request = async {
            let mut limited = (&mut reader).take(MAX_MESSAGE as u64 + 1);
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                limited.read_line(&mut line),
            )
            .await??;
            ensure!(line.len() <= MAX_MESSAGE, "REQUEST_TOO_LARGE");
            Ok::<HostCommand, anyhow::Error>(serde_json::from_str(&line)?)
        }
        .await;
        let reply = match request {
            Ok(HostCommand::ArtifactDownload { artifact }) => {
                if let Err(error) = self.download(artifact, &mut write).await {
                    eprintln!("artifact {artifact} download failed: {error:#}");
                }
                return;
            }
            Ok(HostCommand::WorkspaceFile {
                sandbox_id,
                exec_epoch,
                path,
            }) => {
                let streamed = self
                    .workspace_file(sandbox_id, exec_epoch, path, &mut write)
                    .await;
                if let Err(error) = streamed {
                    eprintln!("sandbox {sandbox_id} file stream failed: {error:#}");
                }
                return;
            }
            Ok(HostCommand::TerminalAttach {
                sandbox_id,
                exec_epoch,
                session,
                cols,
                rows,
                since,
            }) => {
                let command = GuestCommand::PtyAttach {
                    session,
                    cols,
                    rows,
                    since,
                };
                self.terminal(sandbox_id, exec_epoch, command, reader, write)
                    .await;
                return;
            }
            Ok(HostCommand::ArtifactUpload {
                artifact,
                size,
                sha256,
            }) => Reply::from(self.upload(artifact, size, &sha256, &mut reader).await),
            Ok(HostCommand::AttachmentUpload {
                sandbox_id,
                exec_epoch,
                name,
                size,
                sha256,
            }) => {
                let file = AttachmentFile { sha256, size };
                let received = self
                    .attachment_upload(sandbox_id, exec_epoch, &name, file, &mut reader)
                    .await;
                Reply::from(received)
            }
            Ok(command) => Reply::from(self.handle(command).await),
            Err(error) => Reply::from(Err(error)),
        };
        let _ = write.write_all(&reply.line()).await;
    }
}

impl SandboxBackend for Host {
    fn capabilities(&self) -> Value {
        json!({"backend":"containerd","runtimes":["runc","runsc"],"memory_snapshot":false,"gpu":false,"network":"egress-proxy","pty":false,"terminal":true})
    }

    async fn connect(&self, id: Uuid) -> anyhow::Result<GuestChannel> {
        ensure!(
            self.state.lock().await.sandboxes.contains_key(&id),
            "UNKNOWN_SANDBOX"
        );
        Ok(GuestChannel {
            socket: self.dir(id).join("control/guest.sock"),
        })
    }

    async fn create(&self, spec: SandboxSpec) -> anyhow::Result<Value> {
        let _operation = self.operation(spec.id).await;
        self.create_inner(spec).await
    }

    async fn stop(&self, id: Uuid, epoch: u64) -> anyhow::Result<Value> {
        let _operation = self.operation(id).await;
        self.stop_inner(id, epoch).await
    }

    async fn destroy(&self, id: Uuid, epoch: u64) -> anyhow::Result<Value> {
        let _operation = self.operation(id).await;
        self.destroy_inner(id, epoch).await
    }
}

impl Host {
    async fn create_inner(&self, spec: SandboxSpec) -> anyhow::Result<Value> {
        validate_spec(&spec, self.config.multi_tenant)?;
        ensure!(
            !self.state.lock().await.destroyed.contains_key(&spec.id),
            "SANDBOX_DESTROYED"
        );
        if spec.runtime == "runsc" {
            let text = std::fs::read_to_string(
                self.config
                    .runsc_config
                    .as_ref()
                    .context("RUNSC_CONFIG_REQUIRED")?,
            )?;
            ensure!(
                toml::from_str::<toml::Value>(&text)?["runsc_config"]["overlay2"].as_str()
                    == Some("none"),
                "RUNSC_OVERLAY_MUST_BE_NONE"
            );
        }
        let mut state = self.state.lock().await;
        ensure!(
            !state
                .deleted_workspaces
                .contains(&(spec.org_id, spec.workspace_id)),
            "WORKSPACE_DELETED"
        );
        ensure!(
            !state.sandboxes.iter().any(|(id, entry)| *id != spec.id
                && entry.spec.org_id == spec.org_id
                && entry.spec.workspace_id == spec.workspace_id
                && matches!(entry.state.as_str(), "running" | "provisioning")),
            "WORKSPACE_BUSY"
        );
        if let Some(entry) = state.sandboxes.get(&spec.id) {
            ensure!(
                entry.spec.org_id == spec.org_id
                    && entry.spec.workspace_id == spec.workspace_id
                    && entry.spec.image == spec.image
                    && entry.spec.deps == spec.deps
                    && entry.spec.runtime == spec.runtime
                    && spec.exec_epoch >= entry.epoch,
                "SANDBOX_ID_REUSED"
            );
            if entry.state == "running" {
                drop(state);
                let tasks = self.ctr(&["tasks", "list"]).await?;
                ensure!(
                    tasks
                        .lines()
                        .any(|l| l.starts_with(&spec.id.to_string()) && l.contains("RUNNING")),
                    "SANDBOX_LOST"
                );
                return Ok(
                    json!({"id":spec.id,"state":"running","isolation_level":if spec.runtime=="runsc"{"user_kernel"}else{"container"}}),
                );
            }
            ensure!(entry.state == "provisioning", "SANDBOX_STOPPED");
        }
        let dir = self.create_dirs(&mut state, &spec)?;
        let workspace = self.create_workspace(&mut state, &spec)?;
        state.sandboxes.insert(
            spec.id,
            Entry {
                spec: spec.clone(),
                epoch: spec.exec_epoch,
                state: "provisioning".into(),
            },
        );
        journal::save(&self.config.root.join("host.json"), &*state)?;
        drop(state);
        let base = dir.join("base");
        let merged = dir.join("merged");
        // Images are prepared by the operator; no mutable tags or implicit pulls on a user request.
        if mount_options(&base)?.is_none() {
            self.ctr(&["images", "mount", &spec.image, base.to_str().unwrap()])
                .await?;
        }
        // The dependency layer sits over the base: lowerdir lists the topmost layer first.
        let mut lower = base.display().to_string();
        if let Some(deps) = &spec.deps {
            let layer = dir.join("deps");
            std::fs::create_dir_all(&layer)?;
            if mount_options(&layer)?.is_none() {
                self.ctr(&["images", "mount", deps, layer.to_str().unwrap()])
                    .await?;
            }
            lower = format!("{}:{lower}", layer.display());
        }
        if mount_options(&merged)?.is_none() {
            run(
                "mount",
                &[
                    "-t",
                    "overlay",
                    "overlay",
                    "-o",
                    &format!(
                        "lowerdir={lower},upperdir={},workdir={},redirect_dir=off,metacopy=off",
                        dir.join("upper").display(),
                        dir.join("work").display()
                    ),
                    merged.to_str().unwrap(),
                ],
            )
            .await?;
        }
        let options = mount_options(&merged)?.context("OVERLAY_NOT_MOUNTED")?;
        validate_overlay(
            &options,
            &std::fs::read_to_string("/sys/module/overlay/parameters/redirect_dir")
                .unwrap_or_default(),
            &std::fs::read_to_string("/sys/module/overlay/parameters/metacopy").unwrap_or_default(),
        )?;
        let mut config = oci_spec(
            &spec,
            &merged,
            &workspace,
            &dir.join("control"),
            &self.config.guest_binary,
        );
        if spec.persistent {
            std::fs::create_dir_all(merged.join("workspace"))?;
            std::fs::create_dir_all(merged.join("home/agent/.codex"))?;
            mounts(&mut config).retain(|m| {
                !matches!(
                    m["destination"].as_str(),
                    Some("/workspace" | "/home/agent")
                )
            });
            let codex = self.config.root.join("bin/codex");
            ensure!(codex.is_file(), "CODEX_BINARY_REQUIRED");
            mounts(&mut config).push(bind("/codex", &codex, &["bind", "ro"]));
        }
        let project = dir.join("project");
        std::fs::create_dir_all(&project)?;
        mounts(&mut config).push(bind(
            "/project",
            &project,
            &["bind", "ro", "nosuid", "nodev", "noexec"],
        ));
        let attachments = self.attachments(&spec);
        std::fs::create_dir_all(&attachments)?;
        mounts(&mut config).push(bind(
            "/attachments",
            &attachments,
            &["bind", "ro", "nosuid", "nodev", "noexec"],
        ));
        let config_path = dir.join("config.json");
        let gateway_socket = self.config.root.join("gateway");
        if gateway_socket.exists() {
            mounts(&mut config).push(bind("/gateway", &gateway_socket, &["rbind", "ro"]));
        }
        journal::save(&config_path, &config)?;
        let id = spec.id.to_string();
        let task = self.ctr(&["tasks", "list"]).await?;
        if !task
            .lines()
            .any(|line| line.starts_with(&id) && line.contains("RUNNING"))
        {
            let runtime = if spec.runtime == "runsc" {
                "io.containerd.runsc.v1"
            } else {
                "io.containerd.runc.v2"
            };
            let mut args = vec![
                "run",
                "--detach",
                "--null-io",
                "--runtime",
                runtime,
                "--config",
                config_path.to_str().unwrap(),
            ];
            if let Some(path) = &self.config.runsc_config
                && spec.runtime == "runsc"
            {
                args.extend(["--runtime-config-path", path.to_str().unwrap()]);
            }
            args.push(&id);
            self.ctr(&args).await?;
        }
        let guest_socket = dir.join("control/guest.sock");
        let tasks = self.ctr(&["tasks", "list"]).await?;
        let pid = tasks
            .lines()
            .find(|line| line.starts_with(&id))
            .and_then(|line| line.split_whitespace().nth(1))
            .context("MISSING_SANDBOX_PID")?;
        ensure!(pid.parse::<u32>().is_ok(), "INVALID_SANDBOX_PID");
        run(
            "nsenter",
            &["-t", pid, "-n", "ip", "link", "set", "lo", "up"],
        )
        .await?;
        for _ in 0..100 {
            if guest_socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        call(
            &guest_socket,
            &GuestRequest {
                sandbox_id: spec.id,
                exec_epoch: spec.exec_epoch,
                command: GuestCommand::Fence,
            },
        )
        .await?;
        let mut state = self.state.lock().await;
        state.sandboxes.get_mut(&spec.id).unwrap().state = "running".into();
        journal::save(&self.config.root.join("host.json"), &*state)?;
        drop(state);
        self.start_egress(&spec)?;
        let network = if spec.egress.is_empty() {
            "none"
        } else {
            "egress-proxy"
        };
        Ok(
            json!({"id":spec.id,"state":"running","isolation_level":if spec.runtime=="runsc"{"user_kernel"}else{"container"},"network":network}),
        )
    }

    /// Connect a client to a terminal in sandbox `id`: pass on the guest's reply, then copy
    /// bytes both ways until either side closes or the connection is cut.
    async fn terminal(
        &self,
        id: Uuid,
        epoch: u64,
        command: GuestCommand,
        mut reader: crate::ReadSide,
        mut write: crate::WriteSide,
    ) {
        let attached = async {
            let state = self.state.lock().await;
            let entry = state.sandboxes.get(&id).context("UNKNOWN_SANDBOX")?;
            ensure!(entry.state == "running" && entry.epoch == epoch, "FENCED");
            drop(state);
            let request = GuestRequest {
                sandbox_id: id,
                exec_epoch: epoch,
                command,
            };
            self.connect(id).await?.attach(&request).await
        }
        .await;
        let (reply, mut guest_read, mut guest_write) = match attached {
            Ok(attached) => attached,
            Err(error) => {
                let _ = write.write_all(&Reply::from(Err(error)).line()).await;
                return;
            }
        };
        if write
            .write_all(&Reply::from(Ok(reply)).line())
            .await
            .is_err()
        {
            return;
        }
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = tokio::io::copy(&mut reader, &mut guest_write) => {}
                _ = tokio::io::copy(&mut guest_read, &mut write) => {}
            }
        });
        {
            let mut terminals = self.terminals.lock().unwrap();
            let open = terminals.entry(id).or_default();
            open.retain(|(_, task)| !task.is_finished());
            open.push((epoch, task.abort_handle()));
        }
        let _ = task.await;
    }

    /// Cut sandbox `id`'s terminal connections attached under an epoch below `epoch`.
    fn cut_terminals(&self, id: Uuid, epoch: u64) {
        if let Some(open) = self.terminals.lock().unwrap().get_mut(&id) {
            open.retain(|(attached, task)| {
                let cut = *attached < epoch;
                if cut {
                    task.abort();
                }
                !cut && !task.is_finished()
            });
        }
    }

    /// Serve `spec`'s egress proxy when its allowlist is not empty.
    fn start_egress(&self, spec: &SandboxSpec) -> anyhow::Result<()> {
        if spec.egress.is_empty() {
            return Ok(());
        }
        self.egress.start(
            self.dir(spec.id)
                .join("control")
                .join(crate::egress::GUEST_SOCKET),
            spec.org_id,
            spec.id,
            spec.egress.clone(),
        )
    }

    async fn stop_inner(&self, id: Uuid, epoch: u64) -> anyhow::Result<Value> {
        let mut state = self.state.lock().await;
        let entry = state.sandboxes.get_mut(&id).context("UNKNOWN_SANDBOX")?;
        ensure!(epoch >= entry.epoch, "FENCED");
        self.egress.stop(id);
        self.cut_terminals(id, u64::MAX);
        entry.epoch = epoch;
        journal::save(&self.config.root.join("host.json"), &*state)?;
        drop(state);
        let id_text = id.to_string();
        let tasks = self.ctr(&["tasks", "list"]).await?;
        if tasks.lines().any(|line| {
            task_alive(line, &id_text) && line.split_whitespace().nth(2) != Some("RUNNING")
        }) {
            // A host restart may leave an interrupted checkpoint frozen. Thaw before
            // killing, and still require confirmed exit before permitting a new writer.
            self.ctr(&["tasks", "resume", &id_text]).await?;
        }
        let _ = self
            .ctr(&["tasks", "kill", "--signal", "SIGKILL", "--all", &id_text])
            .await;
        // gVisor takes seconds to tear down a sandbox whose processes hold much memory.
        let started = std::time::Instant::now();
        for _ in 0..200 {
            let tasks = self.ctr(&["tasks", "list"]).await?;
            if !tasks.lines().any(|l| task_alive(l, &id_text)) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let tasks = self.ctr(&["tasks", "list"]).await?;
        ensure!(
            !tasks.lines().any(|l| task_alive(l, &id_text)),
            "SANDBOX_STOP_NOT_CONFIRMED"
        );
        if started.elapsed() > std::time::Duration::from_secs(2) {
            eprintln!("sandbox {id} took {:?} to stop", started.elapsed());
        }
        // A stopped task still owns a containerd shim. Reap the task after
        // confirmed exit while retaining its container metadata and workspace.
        if tasks
            .lines()
            .any(|l| l.split_whitespace().next() == Some(id_text.as_str()))
        {
            self.reap(&id_text).await?;
        }
        let mut state = self.state.lock().await;
        state.sandboxes.get_mut(&id).unwrap().state = "stopped".into();
        journal::save(&self.config.root.join("host.json"), &*state)?;
        Ok(json!({"stopped":true}))
    }

    /// Delete a killed task. Under gVisor containerd may still hold it as running for a moment
    /// after the list shows it stopped; a task that stays so is deleted by force, which kills
    /// whatever is left. Its absence from the list confirms the exit either way.
    async fn reap(&self, id: &str) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        for _ in 0..100 {
            match self.ctr(&["tasks", "delete", id]).await {
                Ok(_) => {
                    if started.elapsed() > std::time::Duration::from_secs(1) {
                        eprintln!("task {id} took {:?} to delete", started.elapsed());
                    }
                    return Ok(());
                }
                Err(error) if format!("{error:#}").contains("running") => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(error) => return Err(error),
            }
        }
        eprintln!(
            "task {id} still running {:?} after its kill; deleting it by force",
            started.elapsed()
        );
        let _ = self.ctr(&["tasks", "delete", "--force", id]).await;
        let tasks = self.ctr(&["tasks", "list"]).await?;
        ensure!(
            !tasks
                .lines()
                .any(|l| l.split_whitespace().next() == Some(id)),
            "SANDBOX_STOP_NOT_CONFIRMED"
        );
        Ok(())
    }

    async fn destroy_inner(&self, id: Uuid, epoch: u64) -> anyhow::Result<Value> {
        {
            let mut state = self.state.lock().await;
            let now = now_ms();
            state
                .destroyed
                .retain(|_, at| now.saturating_sub(*at) < TOMBSTONE_MS);
            state.destroyed.insert(id, now);
            journal::save(&self.config.root.join("host.json"), &*state)?;
            if !state.sandboxes.contains_key(&id) {
                return Ok(json!({"destroyed":true}));
            }
        }
        self.stop_inner(id, epoch).await?;
        let id_text = id.to_string();
        let _ = self.ctr(&["tasks", "delete", &id_text]).await;
        let _ = self.ctr(&["containers", "delete", &id_text]).await;
        let dir = self.dir(id);
        for target in [dir.join("merged"), dir.join("deps"), dir.join("base")] {
            if mount_options(&target)?.is_some() {
                run("umount", &[target.to_str().unwrap()]).await?;
            }
        }
        // Workspace is a separate bind mount and is deliberately retained.
        std::fs::remove_dir_all(&dir)?;
        let mut state = self.state.lock().await;
        state.sandboxes.remove(&id);
        journal::save(&self.config.root.join("host.json"), &*state)?;
        Ok(json!({"destroyed":true}))
    }
}

fn task_alive(line: &str, id: &str) -> bool {
    let mut fields = line.split_whitespace();
    fields.next() == Some(id) && matches!(fields.nth(1), Some("RUNNING" | "PAUSED" | "PAUSING"))
}

fn task_heartbeat(line: &str, id: &str, checkpointing: bool) -> bool {
    task_alive(line, id) && (checkpointing || line.split_whitespace().nth(2) == Some("RUNNING"))
}

/// Whether `image` names an image by digest, which nothing can change.
pub fn pinned(image: &str) -> bool {
    image.rsplit_once("@sha256:").is_some_and(|(_, digest)| {
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

pub fn validate_spec(spec: &SandboxSpec, multi_tenant: bool) -> anyhow::Result<()> {
    ensure!(
        pinned(&spec.image) && spec.deps.as_deref().is_none_or(pinned),
        "IMAGE_DIGEST_REQUIRED"
    );
    ensure!(
        matches!(spec.runtime.as_str(), "runc" | "runsc")
            && (!multi_tenant || spec.runtime == "runsc"),
        "ISOLATION_POLICY_DENIED"
    );
    ensure!(
        (64 * 1024 * 1024..=4 * 1024 * 1024 * 1024).contains(&spec.memory_bytes)
            && (16..=1024).contains(&spec.pids_limit)
            && spec.exec_epoch > 0
            && (spec.disk_bytes == 0
                || (256 * 1024 * 1024..=1024 * 1024 * 1024 * 1024).contains(&spec.disk_bytes)),
        "INVALID_RESOURCES"
    );
    ensure!(
        spec.egress.len() <= crate::egress::MAX_RULES
            && spec
                .egress
                .iter()
                .all(|rule| crate::egress::valid_rule(rule)),
        "INVALID_EGRESS_RULE"
    );
    Ok(())
}

pub fn validate_overlay(
    options: &str,
    redirect_default: &str,
    metacopy_default: &str,
) -> anyhow::Result<()> {
    for (key, default) in [
        ("redirect_dir", redirect_default),
        ("metacopy", metacopy_default),
    ] {
        let value = options
            .split(',')
            .find_map(|option| option.strip_prefix(&format!("{key}=")));
        ensure!(
            match value {
                Some("off") => true,
                None => default.trim() == "N",
                _ => false,
            },
            "OVERLAY_OPTIONS_UNSAFE"
        );
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A workspace directory relative to the root (or data volume).
fn workspace_path(org: Uuid, workspace: Uuid) -> PathBuf {
    Path::new("org")
        .join(org.to_string())
        .join("workspaces")
        .join(workspace.to_string())
}

fn disk_bytes(spec: &SandboxSpec) -> u64 {
    match spec.disk_bytes {
        0 => DEFAULT_DISK_BYTES,
        bytes => bytes,
    }
}

pub(crate) fn mount_options(path: &Path) -> anyhow::Result<Option<String>> {
    let path = path.to_str().context("INVALID_PATH")?;
    for line in std::fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.get(4) == Some(&path) {
            return Ok(parts.last().map(|s| s.to_string()));
        }
    }
    Ok(None)
}

pub(crate) async fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    ensure!(
        output.status.success(),
        "{} failed: {}",
        program,
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(1024)
            .collect::<String>()
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn oci_spec(
    spec: &SandboxSpec,
    root: &Path,
    workspace: &Path,
    control: &Path,
    guest: &Path,
) -> Value {
    let capabilities = [
        "CAP_CHOWN",
        "CAP_DAC_OVERRIDE",
        "CAP_FOWNER",
        "CAP_SETGID",
        "CAP_SETUID",
        "CAP_SYS_CHROOT",
        "CAP_KILL",
    ];
    let tmpfs = |destination: &str, options: &[&str]| json!({"destination": destination, "type": "tmpfs", "source": "tmpfs", "options": options});
    let mut env = vec![
        "PATH=/usr/local/bin:/usr/bin:/bin".to_owned(),
        "HOME=/home/agent".to_owned(),
    ];
    if !spec.egress.is_empty() {
        // The guest passes the proxy on to everything it starts.
        env.push(format!(
            "{}={}",
            crate::egress::PROXY_ENV,
            crate::egress::LOCAL_PROXY
        ));
    }
    json!({
        "ociVersion": "1.0.2",
        "root": {"path": root, "readonly": false},
        "hostname": "ryzhub",
        "process": {
            "terminal": false,
            "user": {"uid": 0, "gid": 0},
            "args": ["/ryzhub-guest", spec.id.to_string(), "/control", "/control/guest.sock"],
            "env": env,
            "cwd": "/workspace",
            // Under memory pressure the guest outlives the commands it runs (see
            // `guest::killable_first`).
            "oomScoreAdj": -998,
            "noNewPrivileges": true,
            "capabilities": {
                "bounding": capabilities,
                "effective": capabilities,
                "permitted": capabilities,
            },
        },
        "mounts": [
            {"destination": "/proc", "type": "proc", "source": "proc", "options": ["nosuid", "noexec", "nodev"]},
            tmpfs("/dev", &["nosuid", "strictatime", "mode=755", "size=65536k"]),
            // Terminals: the runtime links /dev/ptmx to this instance's ptmx.
            {"destination": "/dev/pts", "type": "devpts", "source": "devpts",
             "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]},
            tmpfs("/tmp", &["nosuid", "nodev", "size=67108864"]),
            tmpfs("/home/agent", &["nosuid", "nodev", "size=67108864"]),
            bind("/workspace", workspace, &["rbind", "rw", "nosuid", "nodev"]),
            bind("/control", control, &["rbind", "rw", "nosuid", "nodev"]),
            bind("/ryzhub-guest", guest, &["bind", "ro"]),
        ],
        "linux": {
            "namespaces": [
                {"type": "pid"},
                {"type": "network"},
                {"type": "ipc"},
                {"type": "uts"},
                {"type": "mount"},
                {"type": "cgroup"},
            ],
            "resources": {
                "memory": {"limit": spec.memory_bytes},
                "pids": {"limit": spec.pids_limit},
                "cpu": {"quota": 200000, "period": 100000},
            },
            "maskedPaths": ["/proc/kcore", "/proc/keys", "/proc/timer_list", "/sys/firmware"],
            "readonlyPaths": ["/proc/sys", "/proc/sysrq-trigger", "/proc/irq", "/proc/bus"],
        },
    })
}

fn mounts(config: &mut Value) -> &mut Vec<Value> {
    config["mounts"]
        .as_array_mut()
        .expect("OCI spec has mounts")
}

/// A name and content address `/attachments` may hold.
fn valid_attachment(name: &str, file: &AttachmentFile) -> anyhow::Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && !name.starts_with('.')
            && !name.contains(['/', '\\', '\0']),
        "INVALID_ATTACHMENT"
    );
    ensure!(
        file.sha256.len() == 64 && file.sha256.bytes().all(|c| c.is_ascii_hexdigit()),
        "INVALID_ATTACHMENT"
    );
    ensure!(file.size <= MAX_ATTACHMENT, "ATTACHMENT_TOO_LARGE");
    Ok(())
}

/// A bind mount in OCI form.
fn bind(destination: &str, source: &Path, options: &[&str]) -> Value {
    json!({"destination": destination, "type": "bind", "source": source, "options": options})
}

/// Open `path`, relative to `/workspace`, as a regular file of at most `limit` bytes under
/// `root`. The sandbox controls the directory, so the kernel resolves every component inside
/// `root`: `..` and symlinks, even absolute ones, cannot lead out of it, and magic links and
/// other mounts are refused. The sandbox cannot make device nodes; a FIFO opens without
/// blocking and, like a socket or a directory, is not a file.
fn open_inside(root: &Path, path: &str, limit: u64) -> Result<(std::fs::File, u64), &'static str> {
    use nix::{
        errno::Errno,
        fcntl::{OFlag, OpenHow, ResolveFlag},
        sys::stat::Mode,
    };
    if path.is_empty() || path.len() > 4096 || path.contains('\0') {
        return Err("INVALID_PATH");
    }
    let directory = nix::fcntl::open(
        root,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| "FILE_NOT_FOUND")?;
    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC | OFlag::O_NOCTTY)
        .resolve(
            ResolveFlag::RESOLVE_IN_ROOT
                | ResolveFlag::RESOLVE_NO_MAGICLINKS
                | ResolveFlag::RESOLVE_NO_XDEV,
        );
    let file = nix::fcntl::openat2(&directory, path, how).map_err(|error| match error {
        Errno::ENAMETOOLONG => "INVALID_PATH",
        Errno::ENXIO => "NOT_A_FILE",
        Errno::ENOSYS => "OPENAT2_UNSUPPORTED",
        _ => "FILE_NOT_FOUND",
    })?;
    let file = std::fs::File::from(file);
    let metadata = file.metadata().map_err(|_| "FILE_NOT_FOUND")?;
    if !metadata.is_file() {
        return Err("NOT_A_FILE");
    }
    if metadata.len() > limit {
        return Err("FILE_TOO_LARGE");
    }
    Ok((file, metadata.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_files_open_only_inside_the_workspace() {
        use std::{io::Read, os::unix::fs::symlink};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/report.txt"), b"report").unwrap();
        std::fs::write(dir.path().join("secret"), b"secret").unwrap();
        let secret = dir.path().join("secret");
        symlink(&secret, root.join("absolute")).unwrap();
        symlink("../secret", root.join("relative")).unwrap();
        symlink("out/report.txt", root.join("inside")).unwrap();
        nix::unistd::mkfifo(&root.join("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        let read = |path: &str| {
            open_inside(&root, path, 1024).map(|(mut file, size)| {
                let mut text = String::new();
                file.read_to_string(&mut text).unwrap();
                (text, size)
            })
        };
        assert_eq!(read("out/report.txt"), Ok(("report".into(), 6)));
        // A link inside the workspace is followed, staying inside.
        assert_eq!(read("inside"), Ok(("report".into(), 6)));
        // `..` and links, even absolute ones, resolve inside the workspace: nothing outside.
        assert_eq!(read("../secret"), Err("FILE_NOT_FOUND"));
        assert_eq!(read("out/../../secret"), Err("FILE_NOT_FOUND"));
        assert_eq!(read("relative"), Err("FILE_NOT_FOUND"));
        assert_eq!(read("absolute"), Err("FILE_NOT_FOUND"));
        assert_eq!(read(secret.to_str().unwrap()), Err("FILE_NOT_FOUND"));
        // A FIFO opens without waiting for a writer, and is refused.
        assert_eq!(read("pipe"), Err("NOT_A_FILE"));
        assert_eq!(read("out"), Err("NOT_A_FILE"));
        assert_eq!(read("missing.txt"), Err("FILE_NOT_FOUND"));
        assert_eq!(read(""), Err("INVALID_PATH"));
        assert_eq!(read("a\0b"), Err("INVALID_PATH"));
        assert_eq!(
            open_inside(&root, "out/report.txt", 5).map(|(_, size)| size),
            Err("FILE_TOO_LARGE")
        );
    }

    #[test]
    fn a_failed_job_is_reported_once_then_retried() {
        let (failed, done) = (Uuid::now_v7(), Uuid::now_v7());
        let mut jobs = Jobs::new();
        jobs.insert(failed, Some(Err("CHECKSUM_MISMATCH".into())));
        jobs.insert(done, Some(Ok(json!({"size": 1}))));
        assert_eq!(poll_job(&mut jobs, failed).unwrap()["state"], "failed");
        // The next request with the same artifact starts the job afresh.
        assert!(poll_job(&mut jobs, failed).is_none());
        // Completed jobs keep answering.
        assert_eq!(poll_job(&mut jobs, done).unwrap()["state"], "completed");
        assert_eq!(poll_job(&mut jobs, done).unwrap()["state"], "completed");
    }

    /// A certificate authority in `dir`: its certificate's path and an issuer for it.
    fn authority(dir: &Path, stem: &str) -> (PathBuf, rcgen::Issuer<'static, rcgen::KeyPair>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let path = dir.join(format!("{stem}.pem"));
        std::fs::write(&path, params.self_signed(&key).unwrap().pem()).unwrap();
        (path, rcgen::Issuer::new(params, key))
    }

    /// A certificate for `name` from `issuer`: its certificate and key paths.
    fn issue(
        dir: &Path,
        stem: &str,
        name: &str,
        issuer: &rcgen::Issuer<'static, rcgen::KeyPair>,
    ) -> (PathBuf, PathBuf) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
            .unwrap()
            .signed_by(&key, issuer)
            .unwrap();
        let (cert_path, key_path) = (
            dir.join(format!("{stem}.pem")),
            dir.join(format!("{stem}.key")),
        );
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key.serialize_pem()).unwrap();
        (cert_path, key_path)
    }

    #[tokio::test]
    async fn a_remote_control_plane_needs_a_trusted_certificate() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let tls = root.path().join("tls");
        std::fs::create_dir_all(&tls)?;
        let host = Arc::new(Host {
            config: Config {
                root: root.path().to_path_buf(),
                containerd: String::new(),
                guest_binary: "/unused".into(),
                multi_tenant: false,
                allowed_uid: 0,
                runsc_config: None,
                egress_per_minute: 600,
                volume: None,
            },
            state: Mutex::new(State::default()),
            operations: Mutex::new(BTreeMap::new()),
            checkpointing: std::sync::Mutex::new(BTreeSet::new()),
            jobs: std::sync::Mutex::new(BTreeMap::new()),
            egress: crate::egress::Egress::new(600),
            volume: None,
            terminals: std::sync::Mutex::new(BTreeMap::new()),
            _lock: std::fs::File::create(root.path().join("host.lock"))?,
        });
        let (ca, issuer) = authority(&tls, "ca");
        let (node_cert, node_key) = issue(&tls, "node", "node.test", &issuer);
        let (client_cert, client_key) = issue(&tls, "client", "control.test", &issuer);
        let (other_ca, other) = authority(&tls, "other-ca");
        let (stranger_cert, stranger_key) = issue(&tls, "stranger", "control.test", &other);
        let port = std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        let address = format!("127.0.0.1:{port}");
        let acceptor = crate::transport::acceptor(&node_cert, &node_key, &ca)?;
        tokio::spawn(host.clone().serve_tls(address.clone(), acceptor));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let status = HostCommand::WorkspaceStatus {
            org_id: Uuid::now_v7(),
            workspace_id: Uuid::now_v7(),
        };
        let trusted = crate::Endpoint::tls(&address, "node.test", &ca, &client_cert, &client_key)?;
        assert_eq!(call(&trusted, &status).await?, json!({"exists": false}));
        // A client the node does not trust is turned away.
        let stranger =
            crate::Endpoint::tls(&address, "node.test", &ca, &stranger_cert, &stranger_key)?;
        assert!(call(&stranger, &status).await.is_err());
        // A node the client does not trust is not spoken to.
        let doubtful =
            crate::Endpoint::tls(&address, "node.test", &other_ca, &client_cert, &client_key)?;
        assert!(call(&doubtful, &status).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn a_destroyed_sandbox_cannot_be_created_again() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let host = Arc::new(Host {
            config: Config {
                root: root.path().to_path_buf(),
                containerd: String::new(),
                guest_binary: "/unused".into(),
                multi_tenant: false,
                allowed_uid: 0,
                runsc_config: None,
                egress_per_minute: 600,
                volume: None,
            },
            state: Mutex::new(State::default()),
            operations: Mutex::new(BTreeMap::new()),
            checkpointing: std::sync::Mutex::new(BTreeSet::new()),
            jobs: std::sync::Mutex::new(BTreeMap::new()),
            egress: crate::egress::Egress::new(600),
            volume: None,
            terminals: std::sync::Mutex::new(BTreeMap::new()),
            _lock: std::fs::File::create(root.path().join("host.lock"))?,
        });
        let id = Uuid::now_v7();
        // Destroying a sandbox the node never had still fences a late Create.
        host.destroy(id, 1).await?;
        let late = host
            .create(SandboxSpec {
                id,
                org_id: Uuid::now_v7(),
                workspace_id: Uuid::now_v7(),
                exec_epoch: 1,
                image: format!("registry.test/base@sha256:{}", "a".repeat(64)),
                runtime: "runc".into(),
                memory_bytes: 256 * 1024 * 1024,
                pids_limit: 64,
                persistent: false,
                egress: vec![],
                disk_bytes: 0,
                deps: None,
            })
            .await;
        assert!(late.unwrap_err().to_string().contains("SANDBOX_DESTROYED"));
        let stored: State = journal::load(&root.path().join("host.json"))?;
        assert!(stored.destroyed.contains_key(&id));
        Ok(())
    }

    #[tokio::test]
    async fn a_workspace_checkpoint_restores_where_the_node_lacks_the_workspace()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let host = Arc::new(Host {
            config: Config {
                root: root.path().to_path_buf(),
                containerd: String::new(),
                guest_binary: "/unused".into(),
                multi_tenant: false,
                allowed_uid: 0,
                runsc_config: None,
                egress_per_minute: 600,
                volume: None,
            },
            state: Mutex::new(State::default()),
            operations: Mutex::new(BTreeMap::new()),
            checkpointing: std::sync::Mutex::new(BTreeSet::new()),
            jobs: std::sync::Mutex::new(BTreeMap::new()),
            egress: crate::egress::Egress::new(600),
            volume: None,
            terminals: std::sync::Mutex::new(BTreeMap::new()),
            _lock: std::fs::File::create(root.path().join("host.lock"))?,
        });
        let (org, workspace, artifact) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let dir = root.path().join(workspace_path(org, workspace));
        // Top-level names an upper layer leaves out are ordinary files in a workspace.
        std::fs::create_dir_all(dir.join("tmp"))?;
        std::fs::write(dir.join("tmp/notes.txt"), "kept")?;
        std::fs::write(dir.join(".env"), "SECRET=1")?;
        let manifest = loop {
            let state = host.start_workspace_checkpoint(org, workspace, artifact)?;
            match state["state"].as_str() {
                Some("running") => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                Some("completed") => break state,
                _ => anyhow::bail!("{state}"),
            }
        };
        assert_eq!(manifest["scope"], "workspace");
        assert_eq!(manifest["excluded"], json!(["/workspace/.env"]));
        let sha256 = manifest["sha256"].as_str().unwrap().to_owned();
        // The node still has the workspace: nothing is unpacked.
        let kept = host
            .workspace_restore(org, workspace, artifact, sha256.clone(), 0)
            .await?;
        assert_eq!(kept["restored"], false);
        std::fs::remove_dir_all(&dir)?;
        let restored = host
            .workspace_restore(org, workspace, artifact, sha256, 0)
            .await?;
        assert_eq!(restored["restored"], true);
        assert_eq!(std::fs::read_to_string(dir.join("tmp/notes.txt"))?, "kept");
        assert!(!dir.join(".env").exists(), "secrets are not archived");
        assert!(!dir.with_extension("restoring").exists());
        Ok(())
    }

    #[tokio::test]
    async fn workspace_purge_is_scoped_idempotent_and_prevents_recreation() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let host = Arc::new(Host {
            config: Config {
                root: root.path().to_path_buf(),
                containerd: String::new(),
                guest_binary: "/unused".into(),
                multi_tenant: false,
                allowed_uid: 0,
                runsc_config: None,
                egress_per_minute: 600,
                volume: None,
            },
            state: Mutex::new(State::default()),
            operations: Mutex::new(BTreeMap::new()),
            checkpointing: std::sync::Mutex::new(BTreeSet::new()),
            jobs: std::sync::Mutex::new(BTreeMap::new()),
            egress: crate::egress::Egress::new(600),
            volume: None,
            terminals: std::sync::Mutex::new(BTreeMap::new()),
            _lock: std::fs::File::create(root.path().join("host.lock"))?,
        });
        let org = Uuid::now_v7();
        let workspace = Uuid::now_v7();
        let other = Uuid::now_v7();
        let path = |org: Uuid| {
            root.path()
                .join("org")
                .join(org.to_string())
                .join("workspaces")
                .join(workspace.to_string())
        };
        for org in [org, other] {
            std::fs::create_dir_all(path(org))?;
            std::fs::write(path(org).join("secret"), b"contents")?;
        }
        for _ in 0..2 {
            host.handle(HostCommand::PurgeWorkspace {
                org_id: org,
                workspace_id: workspace,
                artifacts: vec![],
            })
            .await?;
        }
        assert!(!path(org).exists());
        assert!(path(other).join("secret").exists());
        let stored: State = journal::load(&root.path().join("host.json"))?;
        assert!(stored.deleted_workspaces.contains(&(org, workspace)));
        let err = host
            .create(SandboxSpec {
                id: Uuid::now_v7(),
                org_id: org,
                workspace_id: workspace,
                exec_epoch: 1,
                image: format!("local@sha256:{}", "a".repeat(64)),
                runtime: "runc".into(),
                memory_bytes: 512 * 1024 * 1024,
                pids_limit: 64,
                persistent: true,
                egress: vec![],
                disk_bytes: 0,
                deps: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "WORKSPACE_DELETED");
        Ok(())
    }

    #[tokio::test]
    async fn attachments_hold_exactly_the_listed_verified_files() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir()?;
        let host = Arc::new(Host {
            config: Config {
                root: root.path().to_path_buf(),
                containerd: String::new(),
                guest_binary: "/unused".into(),
                multi_tenant: false,
                allowed_uid: 0,
                runsc_config: None,
                egress_per_minute: 600,
                volume: None,
            },
            state: Mutex::new(State::default()),
            operations: Mutex::new(BTreeMap::new()),
            checkpointing: std::sync::Mutex::new(BTreeSet::new()),
            jobs: std::sync::Mutex::new(BTreeMap::new()),
            egress: crate::egress::Egress::new(600),
            volume: None,
            terminals: std::sync::Mutex::new(BTreeMap::new()),
            _lock: std::fs::File::create(root.path().join("host.lock"))?,
        });
        let (sandbox, org, workspace) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let spec = SandboxSpec {
            id: sandbox,
            org_id: org,
            workspace_id: workspace,
            exec_epoch: 2,
            image: String::new(),
            runtime: "runc".into(),
            memory_bytes: 0,
            pids_limit: 0,
            persistent: false,
            egress: vec![],
            disk_bytes: 0,
            deps: None,
        };
        host.state.lock().await.sandboxes.insert(
            sandbox,
            Entry {
                spec: spec.clone(),
                epoch: 2,
                state: "running".into(),
            },
        );
        let file = |content: &[u8]| AttachmentFile {
            sha256: format!("{:x}", Sha256::digest(content)),
            size: content.len() as u64,
        };
        let sync = |files: BTreeMap<String, AttachmentFile>, epoch| {
            host.handle(HostCommand::Attachments {
                sandbox_id: sandbox,
                exec_epoch: epoch,
                files,
            })
        };
        let listed = BTreeMap::from([("a.txt".to_owned(), file(b"alpha"))]);
        assert_eq!(sync(listed.clone(), 2).await?["missing"], json!(["a.txt"]));
        // A body that does not match its address never becomes visible.
        let wrong = host
            .attachment_upload(sandbox, 2, "a.txt", file(b"alpha"), &b"alpho"[..])
            .await;
        assert_eq!(wrong.unwrap_err().to_string(), "CHECKSUM_MISMATCH");
        host.attachment_upload(sandbox, 2, "a.txt", file(b"alpha"), &b"alpha"[..])
            .await?;
        let directory = host.attachments(&spec);
        assert_eq!(std::fs::read(directory.join("a.txt"))?, b"alpha");
        assert_eq!(sync(listed, 2).await?["missing"], json!([]));
        // The same name with other content is replaced; unlisted files go.
        let replaced = BTreeMap::from([("a.txt".to_owned(), file(b"omega"))]);
        assert_eq!(sync(replaced, 2).await?["missing"], json!(["a.txt"]));
        assert!(!directory.join("a.txt").exists());
        assert_eq!(sync(BTreeMap::new(), 2).await?["missing"], json!([]));
        // Stale writers and bad names are refused.
        assert_eq!(
            sync(BTreeMap::new(), 1).await.unwrap_err().to_string(),
            "FENCED"
        );
        let bad = BTreeMap::from([("../x".to_owned(), file(b"x"))]);
        assert_eq!(
            sync(bad, 2).await.unwrap_err().to_string(),
            "INVALID_ATTACHMENT"
        );
        host.state.lock().await.sandboxes.clear();
        host.handle(HostCommand::PurgeWorkspace {
            org_id: org,
            workspace_id: workspace,
            artifacts: vec![],
        })
        .await?;
        assert!(!directory.exists() && !directory.with_extension("json").exists());
        Ok(())
    }

    #[tokio::test]
    async fn blocked_guest_keeps_host_state_and_other_sandboxes_available() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let id = Uuid::now_v7();
        let host = Arc::new(Host {
            config: Config {
                root: root.path().into(),
                containerd: String::new(),
                guest_binary: "/unused".into(),
                multi_tenant: false,
                allowed_uid: 0,
                runsc_config: None,
                egress_per_minute: 600,
                volume: None,
            },
            state: Mutex::new(State::default()),
            operations: Mutex::new(BTreeMap::new()),
            checkpointing: std::sync::Mutex::new(BTreeSet::new()),
            jobs: std::sync::Mutex::new(BTreeMap::new()),
            egress: crate::egress::Egress::new(600),
            volume: None,
            terminals: std::sync::Mutex::new(BTreeMap::new()),
            _lock: std::fs::File::create(root.path().join("host.lock"))?,
        });
        host.state.lock().await.sandboxes.insert(
            id,
            Entry {
                spec: SandboxSpec {
                    id,
                    org_id: Uuid::now_v7(),
                    workspace_id: Uuid::now_v7(),
                    exec_epoch: 1,
                    image: String::new(),
                    runtime: "runc".into(),
                    memory_bytes: 0,
                    pids_limit: 0,
                    persistent: true,
                    egress: vec![],
                    disk_bytes: 0,
                    deps: None,
                },
                epoch: 1,
                state: "running".into(),
            },
        );
        std::fs::create_dir_all(host.dir(id).join("control"))?;
        let listener = UnixListener::bind(host.dir(id).join("control/guest.sock"))?;
        let forwarding = {
            let host = host.clone();
            tokio::spawn(async move {
                host.forward(GuestRequest {
                    sandbox_id: id,
                    exec_epoch: 1,
                    command: GuestCommand::Fence,
                })
                .await
            })
        };
        let (_stream, _) = listener.accept().await?;
        let timeout = std::time::Duration::from_millis(200);
        let state = tokio::time::timeout(timeout, host.state.lock()).await?;
        assert_eq!(state.sandboxes[&id].epoch, 1);
        drop(state);
        let other = tokio::time::timeout(timeout, host.operation(Uuid::now_v7())).await?;
        drop(other);
        assert!(
            tokio::time::timeout(timeout, host.operation(id))
                .await
                .is_err()
        );
        forwarding.abort();
        let _ = forwarding.await;
        Ok(())
    }

    #[test]
    fn checkpoint_pauses_remain_live_for_heartbeats_and_stop_confirmation() {
        assert!(task_alive("box 42 RUNNING", "box"));
        assert!(task_alive("box 42 PAUSED", "box"));
        assert!(task_alive("box 42 PAUSING", "box"));
        assert!(!task_alive("box 42 STOPPED", "box"));
        assert!(!task_alive("box-other 42 RUNNING", "box"));
        assert!(task_heartbeat("box 42 PAUSED", "box", true));
        assert!(!task_heartbeat("box 42 PAUSED", "box", false));
        let id = Uuid::now_v7();
        let active = std::sync::Mutex::new(BTreeSet::from([id]));
        {
            let _activity = CheckpointActivity {
                active: &active,
                id,
            };
        }
        assert!(active.lock().unwrap().is_empty());
    }

    #[test]
    fn reject_unsafe_overlay_and_runc_multitenancy() {
        assert!(
            validate_overlay(
                "lowerdir=/a,upperdir=/b,redirect_dir=off,metacopy=off",
                "Y",
                "Y"
            )
            .is_ok()
        );
        assert!(validate_overlay("redirect_dir=on,metacopy=off", "N", "N").is_err());
        assert!(validate_overlay("redirect_dir=off,metacopy=on", "N", "N").is_err());
        assert!(validate_overlay("rw", "Y", "N").is_err());
        let mut spec = SandboxSpec {
            id: Uuid::now_v7(),
            org_id: Uuid::now_v7(),
            workspace_id: Uuid::now_v7(),
            exec_epoch: 1,
            image: format!("example/image@sha256:{}", "a".repeat(64)),
            runtime: "runc".into(),
            memory_bytes: 128 * 1024 * 1024,
            pids_limit: 64,
            persistent: false,
            egress: vec![],
            disk_bytes: 0,
            deps: None,
        };
        assert!(validate_spec(&spec, false).is_ok());
        assert!(validate_spec(&spec, true).is_err());
        spec.image = "alpine:latest".into();
        assert!(validate_spec(&spec, false).is_err());
    }
}
