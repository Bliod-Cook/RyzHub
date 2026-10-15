//! Untrusted input (images, plugin packages, documents) is read in a process of its own: this
//! server binary, or `attachment_renderer`, run with a subcommand, the input on its stdin and
//! a JSON reply on its stdout.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::Context;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

/// The program to run: the configured one, or else this very binary. The kernel's link to the
/// running binary still works once a rebuild or an upgrade has replaced the file it was started
/// from, which the path `current_exe` names no longer does.
pub fn program(configured: &str) -> std::io::Result<PathBuf> {
    if !configured.is_empty() {
        return Ok(configured.into());
    }
    let running = Path::new("/proc/self/exe");
    if running.exists() {
        return Ok(running.to_path_buf());
    }
    std::env::current_exe()
}

/// Run `program` with `args` and `input` on its stdin; its reply, of at most `max` bytes,
/// within `timeout`.
pub async fn run(
    program: &Path,
    args: &[&str],
    mut input: impl AsyncRead + Unpin,
    max: u64,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    let mut stdin = child.stdin.take().context("child input")?;
    let mut stdout = child.stdout.take().context("child output")?;
    let exchange = async {
        // A refusal may close stdin early; the reply says why. Dropping stdin ends the input.
        let feed = async move {
            let fed = tokio::io::copy(&mut input, &mut stdin).await;
            drop(stdin);
            fed
        };
        let mut reply = vec![];
        let mut limited = (&mut stdout).take(max);
        let read = limited.read_to_end(&mut reply);
        let (_, read) = tokio::join!(feed, read);
        read?;
        child.wait().await?;
        Ok::<_, std::io::Error>(reply)
    };
    let reply = tokio::time::timeout(timeout, exchange)
        .await
        .context("timed out")??;
    serde_json::from_slice(&reply).context("unreadable reply")
}
