//! Keeping a sandbox's `/attachments` equal to its conversation's files.
use std::collections::BTreeMap;

use ryzhub_execution::{AttachmentFile, HostCommand};
use uuid::Uuid;

use crate::{app::App, sandbox, store::Lease, workspace::attachment_stores};

/// Make `sandbox`'s `/attachments` hold the conversation's files as they are now: deleted
/// files go, new ones are uploaded from the library. A file whose content is missing from
/// storage is skipped rather than failing the caller.
pub async fn sync(app: &App, lease: &Lease, sandbox: Uuid) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT f.path_name,a.sha256,a.size FROM conversation_files f
         JOIN attachments a ON a.org_id=f.org_id AND a.id=f.attachment_id
         WHERE f.org_id=$1 AND f.conversation_id=$2 AND a.state='ready'",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let files: BTreeMap<String, AttachmentFile> = rows
        .into_iter()
        .map(|(name, sha256, size)| {
            let size = size as u64;
            (name, AttachmentFile { sha256, size })
        })
        .collect();
    let command = HostCommand::Attachments {
        sandbox_id: sandbox,
        exec_epoch: lease.epoch as u64,
        files: files.clone(),
    };
    let reply = sandbox::host(app, lease.org_id, sandbox, command).await?;
    let missing: Vec<String> = reply["missing"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let (primary, secondary) = attachment_stores(app, lease.org_id)?;
    for name in missing {
        let Some(file) = files.get(&name) else {
            continue;
        };
        let opened = match primary.open(&file.sha256).await {
            Ok(opened) => Ok(opened),
            Err(error) => match &secondary {
                Some(secondary) => secondary.open(&file.sha256).await,
                None => Err(error),
            },
        };
        let (size, reader) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                tracing::warn!(%error, sha256 = file.sha256, "attachment content missing");
                continue;
            }
        };
        let command = HostCommand::AttachmentUpload {
            sandbox_id: sandbox,
            exec_epoch: lease.epoch as u64,
            name,
            size,
            sha256: file.sha256.clone(),
        };
        let socket = sandbox::endpoint(app, lease.org_id, sandbox).await?;
        ryzhub_execution::upload(&socket, &command, size, reader).await?;
    }
    Ok(())
}
