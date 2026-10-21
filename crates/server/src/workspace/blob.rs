//! Content-addressed checkpoint storage: a local (or NAS) directory, or an S3-compatible bucket.
//!
//! Keys are the SHA-256 of the object. Writes stream from the source while hashing and are
//! accepted only when the digest matches the key, so no stored object is ever re-read to
//! verify an upload. Objects are laid out as `<org>/<sha256>`.
use anyhow::{Context, ensure};
use futures_util::{StreamExt, TryStreamExt};
use object_store::{
    ClientOptions, ObjectStoreExt, PutPayload, RetryConfig, WriteMultipart,
    aws::{AmazonS3, AmazonS3Builder},
    multipart::{MultipartStore, PartId},
    path::Path as ObjectPath,
    signer::{SignedUrlOptions, Signer},
};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, pin::Pin, sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Objects up to this size use one PUT; larger ones a multipart upload in parts of this size.
const PART_SIZE: usize = 16 * 1024 * 1024;
/// Parts uploaded concurrently.
const PART_CONCURRENCY: usize = 4;

pub type Reader = Pin<Box<dyn AsyncRead + Send>>;

#[derive(Clone)]
enum Backend {
    Local(PathBuf),
    S3(Arc<AmazonS3>),
}

#[derive(Clone)]
pub struct BlobStore {
    backend: Backend,
    /// `<org>/` once scoped, plus a namespace if any.
    prefix: String,
}

#[derive(serde::Deserialize)]
struct S3Config {
    endpoint: String,
    bucket: String,
    region: String,
    access_env: String,
    secret_env: String,
}

fn valid_key(key: &str) -> anyhow::Result<()> {
    ensure!(
        key.len() == 64 && key.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid blob key"
    );
    Ok(())
}

impl BlobStore {
    /// A directory path, or `s3config:<toml>` naming the endpoint, bucket and the environment
    /// variables that hold the credentials.
    pub fn configured(value: &str) -> anyhow::Result<Self> {
        let Some(path) = value.strip_prefix("s3config:") else {
            return Ok(Self {
                backend: Backend::Local(value.into()),
                prefix: String::new(),
            });
        };
        let config: S3Config =
            crate::config::parse_toml(&std::fs::read_to_string(path)?).context(path.to_owned())?;
        let endpoint = reqwest::Url::parse(&config.endpoint)?;
        ensure!(
            matches!(endpoint.scheme(), "http" | "https")
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.query().is_none()
                && endpoint.fragment().is_none(),
            "invalid S3 endpoint"
        );
        ensure!(
            config
                .bucket
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'),
            "invalid S3 bucket"
        );
        let store = AmazonS3Builder::new()
            .with_endpoint(config.endpoint.trim_end_matches('/'))
            .with_virtual_hosted_style_request(false)
            // Objects are deleted one at a time; a plain DELETE works on every S3 implementation.
            .with_disable_bulk_delete(true)
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(
                std::env::var(&config.access_env).context("S3 access key is not set")?,
            )
            .with_secret_access_key(
                std::env::var(&config.secret_env).context("S3 secret key is not set")?,
            )
            // Client options replace the builder's, so `allow_http` must be set here.
            .with_client_options(
                ClientOptions::new()
                    .with_allow_http(endpoint.scheme() == "http")
                    .with_connect_timeout(Duration::from_secs(10))
                    // Per request (one part at most); the upload as a whole has no deadline.
                    .with_timeout(Duration::from_secs(300)),
            )
            .with_retry(RetryConfig::default())
            .build()?;
        Ok(Self {
            backend: Backend::S3(Arc::new(store)),
            prefix: String::new(),
        })
    }

    pub fn scoped(mut self, org: uuid::Uuid) -> Self {
        self.prefix = format!("{org}/");
        self
    }

    /// A namespace of its own under the organization, e.g. `<org>/attachments/`. Checkpoint
    /// reclamation never looks inside it.
    pub fn within(mut self, namespace: &str) -> Self {
        self.prefix.push_str(namespace);
        self.prefix.push('/');
        self
    }

    fn local_path(root: &std::path::Path, prefix: &str, key: &str) -> PathBuf {
        root.join(prefix.trim_end_matches('/')).join(key)
    }

    fn object(&self, key: &str) -> ObjectPath {
        ObjectPath::from(format!("{}{key}", self.prefix))
    }

    pub async fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.put_stream(
            key,
            std::io::Cursor::new(bytes.to_vec()),
            bytes.len() as u64,
        )
        .await
    }

    /// Store exactly `size` bytes from `reader` under `key`. An object already present under
    /// its content address is kept as is.
    pub async fn put_stream(
        &self,
        key: &str,
        reader: impl AsyncRead + Unpin,
        size: u64,
    ) -> anyhow::Result<()> {
        valid_key(key)?;
        if self.exists(key).await? {
            return Ok(());
        }
        let mut reader = reader.take(size);
        let mut hasher = Sha256::new();
        match &self.backend {
            Backend::Local(root) => {
                let path = Self::local_path(root, &self.prefix, key);
                let directory = path.parent().context("blob directory")?.to_path_buf();
                tokio::fs::create_dir_all(&directory).await?;
                let temporary = directory.join(format!("{}.upload", uuid::Uuid::now_v7()));
                let result = async {
                    let mut file = tokio::fs::File::create(&temporary).await?;
                    let mut buffer = vec![0; 1024 * 1024];
                    loop {
                        let n = reader.read(&mut buffer).await?;
                        if n == 0 {
                            break;
                        }
                        hasher.update(&buffer[..n]);
                        file.write_all(&buffer[..n]).await?;
                    }
                    file.sync_all().await?;
                    ensure!(
                        format!("{:x}", hasher.finalize_reset()) == key,
                        "checksum mismatch"
                    );
                    tokio::fs::rename(&temporary, &path).await?;
                    std::fs::File::open(&directory)?.sync_all()?;
                    Ok(())
                }
                .await;
                if result.is_err() {
                    let _ = tokio::fs::remove_file(&temporary).await;
                }
                result
            }
            Backend::S3(store) => {
                let location = self.object(key);
                if size as usize <= PART_SIZE {
                    let mut body = Vec::with_capacity(size as usize);
                    reader.read_to_end(&mut body).await?;
                    ensure!(
                        format!("{:x}", Sha256::digest(&body)) == key,
                        "checksum mismatch"
                    );
                    store.put(&location, PutPayload::from(body)).await?;
                    return Ok(());
                }
                let mut upload = WriteMultipart::new_with_chunk_size(
                    store.put_multipart(&location).await?,
                    PART_SIZE,
                );
                let result = async {
                    let mut buffer = vec![0; 1024 * 1024];
                    loop {
                        let n = reader.read(&mut buffer).await?;
                        if n == 0 {
                            break;
                        }
                        hasher.update(&buffer[..n]);
                        upload.wait_for_capacity(PART_CONCURRENCY).await?;
                        upload.write(&buffer[..n]);
                    }
                    ensure!(
                        format!("{:x}", hasher.finalize_reset()) == key,
                        "checksum mismatch"
                    );
                    Ok(())
                }
                .await;
                match result {
                    Ok(()) => {
                        upload.finish().await?;
                        Ok(())
                    }
                    Err(error) => {
                        let _ = upload.abort().await;
                        Err(error)
                    }
                }
            }
        }
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        match &self.backend {
            Backend::Local(root) => {
                Ok(tokio::fs::try_exists(Self::local_path(root, &self.prefix, key)).await?)
            }
            Backend::S3(store) => match store.head(&self.object(key)).await {
                Ok(_) => Ok(true),
                Err(object_store::Error::NotFound { .. }) => Ok(false),
                Err(error) => Err(error.into()),
            },
        }
    }

    /// A reader over the object and its size.
    pub async fn open(&self, key: &str) -> anyhow::Result<(u64, Reader)> {
        valid_key(key)?;
        match &self.backend {
            Backend::Local(root) => {
                let file = tokio::fs::File::open(Self::local_path(root, &self.prefix, key)).await?;
                let size = file.metadata().await?.len();
                Ok((size, Box::pin(file)))
            }
            Backend::S3(store) => {
                let object = store.get(&self.object(key)).await?;
                let size = object.meta.size;
                let stream = object
                    .into_stream()
                    .map_err(|error| std::io::Error::other(error.to_string()));
                Ok((
                    size,
                    Box::pin(tokio_util::io::StreamReader::new(stream.boxed())),
                ))
            }
        }
    }

    /// The whole object, verified against its key. For small objects and tests.
    pub async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let (_, mut reader) = self.open(key).await?;
        let mut bytes = vec![];
        reader.read_to_end(&mut bytes).await?;
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == key,
            "checksum mismatch"
        );
        Ok(bytes)
    }

    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        valid_key(key)?;
        match &self.backend {
            Backend::Local(root) => {
                match tokio::fs::remove_file(Self::local_path(root, &self.prefix, key)).await {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }
            Backend::S3(store) => match store.delete(&self.object(key)).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(error) => Err(error.into()),
            },
        }
    }

    /// Whether clients can send objects straight to this store, over presigned URLs.
    pub fn direct(&self) -> bool {
        matches!(self.backend, Backend::S3(_))
    }

    fn bucket(&self) -> anyhow::Result<&Arc<AmazonS3>> {
        match &self.backend {
            Backend::S3(store) => Ok(store),
            Backend::Local(_) => anyhow::bail!("not an S3 store"),
        }
    }

    /// Start a multipart upload to `key` that clients send straight to the bucket. Its content
    /// is not its address: it is read back, and checked, before use.
    pub async fn start_direct(&self, key: &str) -> anyhow::Result<String> {
        valid_key(key)?;
        Ok(self.bucket()?.create_multipart(&self.object(key)).await?)
    }

    /// A URL that accepts part `part` (from 1) of direct upload `upload` with a PUT, for
    /// `expires`.
    pub async fn presign_part(
        &self,
        key: &str,
        upload: &str,
        part: usize,
        expires: Duration,
    ) -> anyhow::Result<String> {
        valid_key(key)?;
        let options = SignedUrlOptions::default().with_query([
            ("partNumber", part.to_string()),
            ("uploadId", upload.to_owned()),
        ]);
        let url = self
            .bucket()?
            .signed_url_opts(
                axum::http::Method::PUT,
                &self.object(key),
                expires,
                &options,
            )
            .await?;
        Ok(url.into())
    }

    /// Put direct upload `upload` together from its parts' entity tags, in order.
    pub async fn finish_direct(
        &self,
        key: &str,
        upload: &str,
        etags: Vec<String>,
    ) -> anyhow::Result<()> {
        valid_key(key)?;
        let parts = etags
            .into_iter()
            .map(|content_id| PartId { content_id })
            .collect();
        self.bucket()?
            .complete_multipart(&self.object(key), &upload.to_owned(), parts)
            .await?;
        Ok(())
    }

    pub async fn abort_direct(&self, key: &str, upload: &str) -> anyhow::Result<()> {
        valid_key(key)?;
        match self
            .bucket()?
            .abort_multipart(&self.object(key), &upload.to_owned())
            .await
        {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Replace a missing or corrupt object here with a verified copy from `source`.
    pub async fn repair_from(&self, source: &BlobStore, key: &str) -> anyhow::Result<()> {
        self.delete(key).await?;
        let (size, reader) = source.open(key).await?;
        self.put_stream(key, reader, size).await
    }
}
