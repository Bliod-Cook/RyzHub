//! Codex 0.154 JSONL driver. Its process belongs to the guest, never to an Owner.
use crate::guest::Guest;
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::ChildStdin,
    sync::Mutex,
};

pub struct Native {
    input: Mutex<ChildStdin>,
}
pub fn capabilities(harness: &str) -> Value {
    if harness == "claude" {
        return json!({"harness":"claude","protocol":"agent-sdk","tested_version":"0.3.283","steer":"emulated","interrupt":"native","resume":"native","approval":"native","question":"native","fork":"native","compact":"native","pty":false});
    }
    json!({"harness":"codex","protocol":"app-server","tested_version":"0.154.0","send":"native","steer":"native","interrupt":"native","resume":"native","approval":"native","question":"native","side_query":"unsupported","fork":"unsupported","pty":false})
}
impl Native {
    pub async fn start(
        guest: Arc<Guest>,
        model: &str,
        mcp: bool,
        harness: &str,
    ) -> anyhow::Result<Arc<Self>> {
        ensure!(!model.is_empty() && model.len() < 256, "INVALID_MODEL");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/agent".into());
        let config_dir = std::path::Path::new(&home).join(".codex");
        std::fs::create_dir_all(&config_dir)?;
        let mut config = format!(
            "model = {}\nmodel_provider = \"ryzhub\"\nweb_search = \"disabled\"\n[model_providers.ryzhub]\nname = \"RyzHub\"\nbase_url = \"http://127.0.0.1:8090/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nsupports_websockets = false\nrequest_max_retries = 0\nstream_max_retries = 0\n",
            serde_json::to_string(model)?
        );
        if mcp {
            config.push_str("[mcp_servers.ryzhub]\nurl = \"http://127.0.0.1:8090/mcp\"\n");
        }
        // No provider or gateway credentials ever enter the durable home directory.
        crate::journal::atomic_bytes(&config_dir.join("config.toml"), config.as_bytes())?;
        ensure!(matches!(harness, "codex" | "claude"), "INVALID_HARNESS");
        let mut command = if harness == "claude" {
            let mut c = tokio::process::Command::new("/usr/local/bin/node");
            c.arg("/opt/ryzhub-claude/dist/adapter.js");
            c
        } else {
            let mut c = tokio::process::Command::new("/codex");
            c.arg("app-server");
            c
        };
        let mut child = command
            .env_clear()
            .env("HOME", &home)
            .env("CODEX_HOME", &config_dir)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("TERM", "dumb")
            .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:8090")
            .env("ANTHROPIC_API_KEY", "guest-proxy")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("RYZHUB_CLAUDE_MODEL", model)
            .env("RYZHUB_MCP", if mcp { "1" } else { "0" })
            .env("TOKIO_WORKER_THREADS", "2")
            .current_dir("/workspace")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let native = Arc::new(Self {
            input: Mutex::new(child.stdin.take().context("MISSING_STDIN")?),
        });
        let mut lines = BufReader::new(child.stdout.take().context("MISSING_STDOUT")?).lines();
        let this = native.clone();
        tokio::spawn(async move {
            let result = async {
                while let Some(line) = lines.next_line().await? {
                    ensure!(line.len() <= 4 * 1024 * 1024, "NATIVE_EVENT_TOO_LARGE");
                    let value: Value = serde_json::from_str(&line)?;
                    if value["id"] == "__initialize" && value.get("error").is_none() {
                        this.send(json!({"method":"initialized"})).await?;
                    }
                    guest.native_event(value).await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            let status = match child.try_wait() {
                Ok(Some(status)) => Some(status),
                _ => {
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            };
            let _=guest.native_event(json!({"method":"harness/exited","params":{"outcome":"unknown","exit_code":status.and_then(|s|s.code()),"error":result.err().map(|e|e.to_string())}})).await;
        });
        native.send(json!({"id":"__initialize","method":"initialize","params":{"clientInfo":{"name":"ryzhub","version":"0.2.0"},"capabilities":{"experimentalApi":true}}})).await?;
        Ok(native)
    }
    pub async fn send(&self, value: Value) -> anyhow::Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        ensure!(bytes.len() <= 4 * 1024 * 1024, "NATIVE_REQUEST_TOO_LARGE");
        bytes.push(b'\n');
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.input.lock().await.write_all(&bytes),
        )
        .await??;
        Ok(())
    }
}
