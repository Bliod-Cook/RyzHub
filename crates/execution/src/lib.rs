//! Trusted host orchestration and an independently durable guest exec service.
pub mod archive;
pub mod guest;
pub mod host;
pub mod journal;
pub mod native;
pub mod proxy;

use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use uuid::Uuid;

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
    NativeRequest {
        op_id: String,
        method: String,
        params: Value,
    },
    NativeAnswer {
        op_id: String,
        id: Value,
        result: Value,
    },
    Exec {
        op_id: String,
        spec: ExecSpec,
    },
    Poll {
        op_id: String,
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
    Replay {
        after: u64,
    },
    Ack {
        through: u64,
    },
    RotateKey {
        token: String,
        socket_name: String,
    },
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
impl Reply {
    pub fn result(self) -> anyhow::Result<Value> {
        ensure!(
            self.ok,
            "{}",
            self.error.unwrap_or_else(|| "EXECUTION_ERROR".into())
        );
        Ok(self.value)
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
    Checkpoint {
        sandbox_id: Uuid,
        exec_epoch: u64,
        artifact: Uuid,
    },
    ReadArtifact {
        artifact: Uuid,
        offset: u64,
    },
    WriteArtifact {
        artifact: Uuid,
        offset: u64,
        data: String,
    },
    Restore {
        spec: SandboxSpec,
        artifact: Uuid,
        sha256: String,
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

pub async fn call(path: &Path, request: &impl Serialize) -> anyhow::Result<Value> {
    let socket = UnixStream::connect(path).await?;
    let (read, mut write) = socket.into_split();
    let mut bytes = serde_json::to_vec(request)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "REQUEST_TOO_LARGE");
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    let mut reader = BufReader::new(read).take(4 * 1024 * 1024 + 1);
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        reader.read_line(&mut line),
    )
    .await??;
    ensure!(line.len() <= 4 * 1024 * 1024, "RESPONSE_TOO_LARGE");
    serde_json::from_str::<Reply>(&line)?.result()
}
use tokio::io::AsyncReadExt;
