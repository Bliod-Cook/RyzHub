//! Adding a file to a member's library, whether uploaded or published from a sandbox.
use std::{path::PathBuf, time::Duration};

use anyhow::Context;
use axum::body::Body;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use super::{Kind, TEXT_INLINE, content_lock, render, sniff};
use crate::{app::App, error::Code, store::Tx, workspace::attachment_stores};

/// A source that sends nothing for this long is dropped.
const IDLE: Duration = Duration::from_secs(30);

/// Content on its way in, spooled to a private temporary file that is removed when this is
/// dropped.
pub(crate) struct Spool {
    path: PathBuf,
    pub size: u64,
    pub sha256: String,
    /// The first bytes: enough to recognize a format, or the whole file if it is small.
    head: Vec<u8>,
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A spool being written, with the digest so far.
struct Writer {
    spool: Spool,
    file: tokio::fs::File,
    hasher: Sha256,
    limit: u64,
}

impl Writer {
    async fn new(limit: u64) -> anyhow::Result<Self> {
        let path = std::env::temp_dir().join(format!("ryzhub-upload-{}", Uuid::now_v7()));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        Ok(Self {
            spool: Spool {
                path,
                size: 0,
                sha256: String::new(),
                head: Vec::new(),
            },
            file,
            hasher: Sha256::new(),
            limit,
        })
    }

    async fn push(&mut self, chunk: &[u8]) -> anyhow::Result<()> {
        let spool = &mut self.spool;
        spool.size += chunk.len() as u64;
        anyhow::ensure!(spool.size <= self.limit, Code::AttachmentTooLarge);
        let room = (TEXT_INLINE + 1).saturating_sub(spool.head.len());
        spool
            .head
            .extend_from_slice(&chunk[..room.min(chunk.len())]);
        self.hasher.update(chunk);
        self.file.write_all(chunk).await?;
        Ok(())
    }

    async fn finish(mut self) -> anyhow::Result<Spool> {
        self.file.sync_all().await?;
        self.spool.sha256 = format!("{:x}", self.hasher.finalize());
        Ok(self.spool)
    }
}

impl Spool {
    /// The first bytes: enough to recognize a format.
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// An HTTP request body of at most `limit` bytes.
    pub async fn receive(body: Body, limit: u64) -> anyhow::Result<Self> {
        let mut writer = Writer::new(limit).await?;
        let mut stream = body.into_data_stream();
        loop {
            let chunk = tokio::time::timeout(IDLE, stream.next())
                .await
                .context(Code::InvalidInput)?;
            let Some(chunk) = chunk else { break };
            writer.push(&chunk.context(Code::InvalidInput)?).await?;
        }
        writer.finish().await
    }

    /// Exactly `size` bytes of `source`, at most `limit`; a source that ends early fails.
    pub async fn read(
        mut source: impl AsyncRead + Unpin,
        size: u64,
        limit: u64,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(size <= limit, Code::AttachmentTooLarge);
        let mut writer = Writer::new(limit).await?;
        let mut buffer = vec![0; 256 * 1024];
        while writer.spool.size < size {
            let read = tokio::time::timeout(IDLE, source.read(&mut buffer))
                .await
                .context("source stalled")??;
            anyhow::ensure!(read > 0, "source ended early");
            let wanted = (size - writer.spool.size).min(read as u64) as usize;
            writer.push(&buffer[..wanted]).await?;
        }
        writer.finish().await
    }
}

/// Add `spool` to `user`'s library as `name`: the type comes from the content, images get a
/// rendition models can read, and the library's `quota` (bytes) is kept. `direction` is
/// `upload` or `output`. The entry is ready once the content is stored.
pub(crate) async fn ingest(
    app: &App,
    org: Uuid,
    user: Uuid,
    name: &str,
    direction: &str,
    spool: &Spool,
    quota: Option<u64>,
) -> anyhow::Result<Uuid> {
    let (mut kind, media_type) = sniff(&spool.head, spool.size, name);
    let rendition = match kind {
        Kind::Image => render(app, &spool.path).await,
        _ => None,
    };
    if kind == Kind::Image && rendition.is_none() {
        kind = Kind::File;
    }
    let text = (kind == Kind::Text).then(|| String::from_utf8_lossy(&spool.head).into_owned());
    let id = Uuid::now_v7();
    {
        let mut tx = app.store.tx(org).await?;
        lock(&mut tx, &format!("attachment-quota:{org}:{user}")).await?;
        content_lock(&mut tx, org, &spool.sha256).await?;
        if let Some(quota) = quota {
            let used: i64 = sqlx::query_scalar(
                "SELECT coalesce(sum(size),0)::bigint FROM attachments
                 WHERE org_id=$1 AND user_id=$2 AND state IN ('uploading','ready')",
            )
            .bind(org)
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;
            anyhow::ensure!(
                used as u64 + spool.size <= quota,
                Code::AttachmentQuotaExceeded
            );
        }
        let model_sha = rendition
            .as_ref()
            .map(|r| format!("{:x}", Sha256::digest(&r.data)));
        // Committed before the object is stored, so reclamation sees the content as in use.
        sqlx::query(
            "INSERT INTO attachments(id,org_id,user_id,direction,state,name,media_type,kind,size,sha256,
                 width,height,model_sha,model_media_type,model_data,text_content)
             VALUES($1,$2,$3,$4,'uploading',$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
        )
        .bind(id)
        .bind(org)
        .bind(user)
        .bind(direction)
        .bind(name)
        .bind(media_type)
        .bind(kind.as_str())
        .bind(spool.size as i64)
        .bind(&spool.sha256)
        .bind(rendition.as_ref().map(|r| r.original.0 as i32))
        .bind(rendition.as_ref().map(|r| r.original.1 as i32))
        .bind(model_sha)
        .bind(rendition.as_ref().map(|r| r.media_type))
        .bind(rendition.as_ref().map(|r| r.data.as_slice()))
        .bind(text)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    if let Err(error) = store(app, org, spool).await {
        let mut tx = app.store.tx(org).await?;
        sqlx::query(
            "UPDATE attachments SET state='deleted',deleted_at=now() WHERE org_id=$1 AND id=$2",
        )
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Err(error);
    }
    let mut tx = app.store.tx(org).await?;
    sqlx::query(
        "UPDATE attachments SET state='ready' WHERE org_id=$1 AND id=$2 AND state='uploading'",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn lock(tx: &mut Tx<'_>, key: &str) -> anyhow::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Store the spooled content in the primary store and, if configured, the secondary one.
async fn store(app: &App, org: Uuid, spool: &Spool) -> anyhow::Result<()> {
    let (primary, secondary) = attachment_stores(app, org)?;
    for store in std::iter::once(primary).chain(secondary) {
        let file = tokio::fs::File::open(&spool.path).await?;
        store.put_stream(&spool.sha256, file, spool.size).await?;
    }
    Ok(())
}
