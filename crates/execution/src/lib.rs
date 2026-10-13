//! Execution node: the trusted host agent and the durable guest agent inside each sandbox.
//!
//! Both speak one-line JSON requests and replies: the guest over a Unix socket, the host over
//! a Unix socket or, for a remote node, TLS with client certificates (see [`Endpoint`]).
//! Checkpoint artifacts are streamed as raw bytes after the JSON header (see [`upload`] and
//! [`download`]).
pub mod acp;
pub mod archive;
pub mod egress;
pub mod guest;
pub mod harness;
pub mod host;
pub mod journal;
pub mod native;
pub mod picture;
pub mod proxy;
pub mod pty;
pub mod quota;
pub mod transport;

use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};

pub use transport::{Endpoint, ReadSide, Stream, WriteSide};
use uuid::Uuid;

pub use harness::{HarnessAnswer, HarnessEvent, HarnessRequest, HarnessResult, TurnSettings};

/// Largest JSON request or reply line.
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// A reply must start within this long. Long-poll requests wait at most 30 s on the guest.
const REPLY_TIMEOUT: Duration = Duration::from_secs(45);
/// A streamed artifact may stall this long between reads.
const STREAM_IDLE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecSpec {
    pub cmd: String,
    #[serde(default = "workspace")]
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub pty: bool,
}

fn codex() -> String {
    "codex".into()
}

fn workspace() -> String {
    "/workspace".into()
}

fn timeout() -> u64 {
    60_000
}

/// A skill's files: `SKILL.md` and its resources, by path in its directory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFiles {
    /// The skill's directory name, which is also its name.
    pub name: String,
    pub files: std::collections::BTreeMap<String, String>,
}

/// An agent spoken to over the Agent Client Protocol (v1), as an instance configures it
/// (`[[harness.acp]]`). The guest starts it in `/workspace` and talks to it on stdio.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AcpProfile {
    /// The harness name run configs use, e.g. `opencode`.
    pub name: String,
    /// The name clients show.
    pub label: String,
    /// The agent's executable in the sandbox image, and its arguments.
    pub command: String,
    pub args: Vec<String>,
    /// Its environment. In values, `{model}` is the run's model, `{models}` a JSON object with
    /// a key for each model the harness may use, `{proxy}` the guest's model proxy
    /// (`http://127.0.0.1:8090`) and `{instructions}` the file holding the platform's
    /// instructions for the session.
    pub env: std::collections::BTreeMap<String, String>,
    /// The agent's value for a model in its `model` session option, `{model}` replaced.
    pub model_option: String,
    /// The gateway protocol the agent's provider speaks: `anthropic`, `responses`,
    /// `chat_completions` or `gemini`.
    pub protocol: String,
    /// The model of runs that name none.
    pub model: String,
    /// The agent version the sandbox image pins. A checkpoint restores only with the version it
    /// was taken with.
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuestCommand {
    /// Adopt a new execution epoch; older epochs are rejected from now on.
    Fence,
    /// Attach to terminal `session` (see [`pty`]), sized `cols`×`rows`, replaying output from
    /// offset `since`. The connection carries terminal frames after the reply.
    PtyAttach {
        session: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        since: u64,
    },
    ProjectInstructions {
        text: String,
    },
    /// Put plugin skills where every harness finds them, replacing those installed before.
    InstallSkills {
        skills: Vec<SkillFiles>,
    },
    NativeStart {
        model: String,
        mcp: bool,
        #[serde(default = "codex")]
        harness: String,
        /// How to run `harness` when it is an agent spoken to over ACP.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acp: Option<AcpProfile>,
        /// The models the harness may switch between, `model` among them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        models: Vec<String>,
    },
    /// A platform-level request, translated by the guest into the harness's native call.
    Harness {
        op_id: String,
        request: HarnessRequest,
    },
    /// The user's decision on a harness approval or question.
    Answer {
        op_id: String,
        request_id: String,
        answer: HarnessAnswer,
    },
    Exec {
        op_id: String,
        spec: ExecSpec,
    },
    /// An operation's current result, waiting up to `wait_ms` for it to finish.
    Poll {
        op_id: String,
        #[serde(default)]
        wait_ms: u64,
    },
    Write {
        op_id: String,
        request_id: String,
        data: String,
    },
    Kill {
        op_id: String,
        request_id: String,
    },
    /// An image file, at most `max_edge` pixels on its long side and `max_bytes` encoded.
    ReadImage {
        path: String,
        max_edge: u32,
        max_bytes: usize,
    },
    /// Unacknowledged events after `after`, waiting up to `wait_ms` for new events or for
    /// streaming text newer than `folds_seen`.
    Replay {
        after: u64,
        #[serde(default)]
        wait_ms: u64,
        #[serde(default)]
        folds_seen: u64,
    },
    Ack {
        through: u64,
    },
    RotateKey {
        token: String,
        socket_name: String,
    },
}

impl GuestCommand {
    /// Commands that only read state; the host forwards them without serializing behind
    /// other operations of the same sandbox.
    pub fn read_only(&self) -> bool {
        matches!(
            self,
            Self::Poll { .. }
                | Self::Replay { .. }
                | Self::ReadImage { .. }
                | Self::PtyAttach { .. }
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GuestRequest {
    pub sandbox_id: Uuid,
    pub exec_epoch: u64,
    pub command: GuestCommand,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GuestEvent {
    pub gseq: u64,
    pub op_id: String,
    pub result: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub value: Value,
    pub error: Option<String>,
}

/// An error reply from the peer, as opposed to a transport failure.
#[derive(Debug)]
pub struct Rejected(pub String);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

impl Reply {
    pub fn result(self) -> anyhow::Result<Value> {
        if !self.ok {
            let error = self.error.unwrap_or_else(|| "EXECUTION_ERROR".into());
            return Err(Rejected(error).into());
        }
        Ok(self.value)
    }

    /// The reply as one newline-terminated JSON line.
    pub fn line(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(self).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
        bytes.push(b'\n');
        bytes
    }
}

impl From<anyhow::Result<Value>> for Reply {
    fn from(result: anyhow::Result<Value>) -> Self {
        match result {
            Ok(value) => Self {
                ok: true,
                value,
                error: None,
            },
            Err(error) => Self {
                ok: false,
                value: Value::Null,
                error: Some(error.to_string()),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub id: Uuid,
    pub org_id: Uuid,
    pub workspace_id: Uuid,
    pub exec_epoch: u64,
    pub image: String,
    pub runtime: String,
    pub memory_bytes: u64,
    pub pids_limit: u64,
    #[serde(default)]
    pub persistent: bool,
    /// Hostnames the sandbox may reach through the egress proxy (see [`egress::allowed`]).
    /// Empty: no network at all.
    #[serde(default)]
    pub egress: Vec<String>,
    /// Largest size of the writable layer (and of a Chat workspace) on a node with a quota
    /// volume; 0 is the node's default.
    #[serde(default)]
    pub disk_bytes: u64,
    /// A read-only dependency layer (package caches, common tools) over the base image,
    /// pinned by digest like it.
    #[serde(default)]
    pub deps: Option<String>,
}

/// A file of the conversation's library references, as `/attachments/<name>` holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentFile {
    pub sha256: String,
    pub size: u64,
}

/// Largest file `/attachments` holds.
pub const MAX_ATTACHMENT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostCommand {
    Project {
        sandbox_id: Uuid,
        exec_epoch: u64,
        files: BTreeMap<String, String>,
    },
    /// Make the sandbox's `/attachments` hold exactly `files`: drop the others and reply with
    /// the names still to upload (`{"missing":[..]}`). The directory belongs to the workspace,
    /// so later sandboxes of the same conversation on this node start with it.
    Attachments {
        sandbox_id: Uuid,
        exec_epoch: u64,
        files: BTreeMap<String, AttachmentFile>,
    },
    /// Receive one file for `/attachments`: `size` raw bytes follow the request line.
    AttachmentUpload {
        sandbox_id: Uuid,
        exec_epoch: u64,
        name: String,
        size: u64,
        sha256: String,
    },
    Create {
        spec: SandboxSpec,
    },
    Status,
    /// Archive workspace `workspace_id`'s directory (a Chat `/workspace`) into `artifact`:
    /// started on first request, then reported like `Checkpoint`.
    WorkspaceCheckpoint {
        org_id: Uuid,
        workspace_id: Uuid,
        artifact: Uuid,
    },
    /// Remove this node's copy of workspace `workspace_id` (its directory and `/attachments`)
    /// while no sandbox uses it. Unlike a purge it may come back, restored from a checkpoint.
    EvictWorkspace {
        org_id: Uuid,
        workspace_id: Uuid,
    },
    /// The digest-pinned images this node holds, each with whether a sandbox uses it.
    Images,
    /// Remove image `reference` unless a sandbox on this node uses it.
    RemoveImage {
        reference: String,
    },
    /// Whether this node holds workspace `workspace_id`'s directory.
    WorkspaceStatus {
        org_id: Uuid,
        workspace_id: Uuid,
    },
    /// Unpack uploaded `artifact` as workspace `workspace_id`'s directory, unless this node
    /// already has one: started on first request, then reported like `Restore`.
    WorkspaceRestore {
        org_id: Uuid,
        workspace_id: Uuid,
        artifact: Uuid,
        sha256: String,
        #[serde(default)]
        disk_bytes: u64,
    },
    /// Attach to a terminal in sandbox `sandbox_id` (see [`pty`]). The connection carries
    /// terminal frames after the reply; it is cut when the sandbox stops or is fenced.
    TerminalAttach {
        sandbox_id: Uuid,
        exec_epoch: u64,
        session: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        since: u64,
    },
    /// Egress log records after `after`, with the log's boot id. A cursor whose `boot` is not
    /// the current log's reads it from the start.
    Egress {
        boot: Option<Uuid>,
        after: u64,
    },
    /// Start, or report on, an asynchronous checkpoint of a persistent sandbox. The reply's
    /// `state` is `running`, `failed` or `completed` (with the manifest).
    Checkpoint {
        sandbox_id: Uuid,
        exec_epoch: u64,
        artifact: Uuid,
    },
    /// Start, or report on, an asynchronous restore of an uploaded artifact into a new
    /// persistent sandbox.
    Restore {
        spec: SandboxSpec,
        artifact: Uuid,
        sha256: String,
    },
    /// Stream an artifact: a header `{"size":n}` followed by `n` raw bytes.
    ArtifactDownload {
        artifact: Uuid,
    },
    /// Stream one regular file of a running sandbox's `/workspace`, `path` relative to it, the
    /// same way. Refusals: `INVALID_PATH`, `FILE_NOT_FOUND`, `NOT_A_FILE`, `FILE_TOO_LARGE`.
    WorkspaceFile {
        sandbox_id: Uuid,
        exec_epoch: u64,
        path: String,
    },
    /// Receive an artifact: `size` raw bytes follow the request line.
    ArtifactUpload {
        artifact: Uuid,
        size: u64,
        sha256: String,
    },
    DeleteArtifact {
        artifact: Uuid,
    },
    Guest {
        request: GuestRequest,
    },
    Stop {
        sandbox_id: Uuid,
        exec_epoch: u64,
    },
    Destroy {
        sandbox_id: Uuid,
        exec_epoch: u64,
    },
    PurgeWorkspace {
        org_id: Uuid,
        workspace_id: Uuid,
        #[serde(default)]
        artifacts: Vec<Uuid>,
    },
}

async fn send_request(
    endpoint: &Endpoint,
    request: &impl Serialize,
) -> anyhow::Result<(ReadSide, WriteSide)> {
    let mut bytes = serde_json::to_vec(request)?;
    ensure!(bytes.len() <= MAX_MESSAGE, "REQUEST_TOO_LARGE");
    bytes.push(b'\n');
    let (read, mut write) = tokio::io::split(endpoint.connect().await?);
    write.write_all(&bytes).await?;
    write.flush().await?;
    Ok((BufReader::new(read), write))
}

async fn read_reply(reader: &mut (impl AsyncBufReadExt + Unpin)) -> anyhow::Result<Value> {
    let mut line = String::new();
    let mut limited = (&mut *reader).take(MAX_MESSAGE as u64 + 1);
    tokio::time::timeout(REPLY_TIMEOUT, limited.read_line(&mut line)).await??;
    ensure!(line.len() <= MAX_MESSAGE, "RESPONSE_TOO_LARGE");
    if line.is_empty() {
        // An I/O error, so callers retry it like any other broken connection.
        let closed = std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed before reply",
        );
        return Err(closed.into());
    }
    serde_json::from_str::<Reply>(&line)?.result()
}

/// A request whose connection stays open after the reply: the reply, then the connection's
/// read side (with anything already buffered after the reply) and write side.
pub async fn attach(
    endpoint: impl Into<Endpoint>,
    request: &impl Serialize,
) -> anyhow::Result<(Value, ReadSide, WriteSide)> {
    let (mut reader, write) = send_request(&endpoint.into(), request).await?;
    let reply = read_reply(&mut reader).await?;
    Ok((reply, reader, write))
}

/// One request, one reply.
pub async fn call(
    endpoint: impl Into<Endpoint>,
    request: &impl Serialize,
) -> anyhow::Result<Value> {
    let (mut reader, _write) = send_request(&endpoint.into(), request).await?;
    read_reply(&mut reader).await
}

/// Request an artifact stream. Returns its size and a reader over exactly that many bytes.
pub async fn download(
    endpoint: impl Into<Endpoint>,
    request: &HostCommand,
) -> anyhow::Result<(u64, tokio::io::Take<ReadSide>)> {
    let (mut reader, _write) = send_request(&endpoint.into(), request).await?;
    let header = read_reply(&mut reader).await?;
    let size = header["size"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("artifact size missing"))?;
    Ok((size, reader.take(size)))
}

/// Send `size` bytes from `body` after the request line and wait for the reply.
pub async fn upload(
    endpoint: impl Into<Endpoint>,
    request: &HostCommand,
    size: u64,
    body: impl AsyncRead + Unpin,
) -> anyhow::Result<Value> {
    let (mut reader, mut write) = send_request(&endpoint.into(), request).await?;
    let copied = copy_exact(body, &mut write, size).await?;
    ensure!(
        copied == size,
        "artifact ended after {copied} of {size} bytes"
    );
    write.flush().await?;
    read_reply(&mut reader).await
}

/// Copy exactly `size` bytes, failing if either side stalls for [`STREAM_IDLE`].
pub async fn copy_exact(
    reader: impl AsyncRead + Unpin,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    size: u64,
) -> anyhow::Result<u64> {
    let mut reader = reader.take(size);
    let mut buffer = vec![0; 256 * 1024];
    let mut copied = 0;
    loop {
        let n = tokio::time::timeout(STREAM_IDLE, reader.read(&mut buffer)).await??;
        if n == 0 {
            return Ok(copied);
        }
        tokio::time::timeout(STREAM_IDLE, writer.write_all(&buffer[..n])).await??;
        copied += n as u64;
    }
}
