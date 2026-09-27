//! Execution node: the trusted host agent and the durable guest agent inside each sandbox.
//!
//! Both speak one-line JSON requests and replies over Unix sockets. Checkpoint artifacts are
//! streamed as raw bytes after the JSON header (see [`upload`] and [`download`]).
pub mod archive;
pub mod guest;
pub mod harness;
pub mod host;
pub mod journal;
pub mod native;
pub mod proxy;

use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixStream, unix::OwnedReadHalf},
};
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuestCommand {
    /// Adopt a new execution epoch; older epochs are rejected from now on.
    Fence,
    ProjectInstructions {
        text: String,
    },
    NativeStart {
        model: String,
        mcp: bool,
        #[serde(default = "codex")]
        harness: String,
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
        matches!(self, Self::Poll { .. } | Self::Replay { .. })
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostCommand {
    Project {
        sandbox_id: Uuid,
        exec_epoch: u64,
        files: BTreeMap<String, String>,
    },
    Create {
        spec: SandboxSpec,
    },
    Status,
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

async fn send_request(path: &Path, request: &impl Serialize) -> anyhow::Result<UnixStream> {
    let mut socket = UnixStream::connect(path).await?;
    let mut bytes = serde_json::to_vec(request)?;
    ensure!(bytes.len() <= MAX_MESSAGE, "REQUEST_TOO_LARGE");
    bytes.push(b'\n');
    socket.write_all(&bytes).await?;
    Ok(socket)
}

async fn read_reply(reader: &mut BufReader<OwnedReadHalf>) -> anyhow::Result<Value> {
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

/// One request, one reply.
pub async fn call(path: &Path, request: &impl Serialize) -> anyhow::Result<Value> {
    let socket = send_request(path, request).await?;
    let (read, _write) = socket.into_split();
    read_reply(&mut BufReader::new(read)).await
}

/// Request an artifact stream. Returns its size and a reader over exactly that many bytes.
pub async fn download(
    path: &Path,
    request: &HostCommand,
) -> anyhow::Result<(u64, impl AsyncRead + Unpin + use<>)> {
    let socket = send_request(path, request).await?;
    let (read, _write) = socket.into_split();
    let mut reader = BufReader::new(read);
    let header = read_reply(&mut reader).await?;
    let size = header["size"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("artifact size missing"))?;
    Ok((size, reader.take(size)))
}

/// Send `size` bytes from `body` after the request line and wait for the reply.
pub async fn upload(
    path: &Path,
    request: &HostCommand,
    size: u64,
    body: impl AsyncRead + Unpin,
) -> anyhow::Result<Value> {
    let socket = send_request(path, request).await?;
    let (read, mut write) = socket.into_split();
    let copied = copy_exact(body, &mut write, size).await?;
    ensure!(
        copied == size,
        "artifact ended after {copied} of {size} bytes"
    );
    write.flush().await?;
    read_reply(&mut BufReader::new(read)).await
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
