//! Content-addressed checkpoint storage: a local (or NAS) directory, or an S3-compatible bucket.
//!
//! Keys are the SHA-256 of the object. Writes stream from the source while hashing and are
//! accepted only when the digest matches the key, so no stored object is ever re-read to
//! verify an upload. Objects are laid out as `<org>/<sha256>`.
use anyhow::{Context, ensure};
use futures_util::{StreamExt, TryStreamExt};
use object_store::{
    ClientOptions, ObjectStore, ObjectStoreExt, PutPayload, RetryConfig, WriteMultipart,
    aws::AmazonS3Builder, path::Path as ObjectPath,
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
    S3(Arc<dyn ObjectStore>),
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
        let config: S3Config = toml::from_str(&std::fs::read_to_string(path)?)?;
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

    /// Whether the stored object still matches its content address.
    pub async fn verify(&self, key: &str) -> anyhow::Result<bool> {
        let Ok((_, mut reader)) = self.open(key).await else {
            return Ok(false);
        };
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1024 * 1024];
        loop {
            let n = match reader.read(&mut buffer).await {
                Ok(n) => n,
                Err(_) => return Ok(false),
            };
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()) == key)
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

    /// Replace a missing or corrupt object here with a verified copy from `source`.
    pub async fn repair_from(&self, source: &BlobStore, key: &str) -> anyhow::Result<()> {
        if self.verify(key).await? {
            return Ok(());
        }
        self.delete(key).await?;
        let (size, reader) = source.open(key).await?;
        self.put_stream(key, reader, size).await
    }
}
