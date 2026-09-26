//! Immutable content-addressed local and S3-compatible storage.
use anyhow::{Context, ensure};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
#[derive(Clone)]
pub enum BlobStore {
    Local(PathBuf),
    S3 {
        endpoint: String,
        bucket: String,
        region: String,
        access_env: String,
        secret_env: String,
        prefix: String,
    },
}
impl BlobStore {
    pub fn configured(value: &str) -> anyhow::Result<Self> {
        if let Some(path) = value.strip_prefix("s3config:") {
            #[derive(serde::Deserialize)]
            struct Config {
                endpoint: String,
                bucket: String,
                region: String,
                access_env: String,
                secret_env: String,
            }
            let c: Config = toml::from_str(&std::fs::read_to_string(path)?)?;
            let u = reqwest::Url::parse(&c.endpoint)?;
            ensure!(
                matches!(u.scheme(), "http" | "https")
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.query().is_none()
                    && u.fragment().is_none(),
                "INVALID_S3_ENDPOINT"
            );
            ensure!(
                c.bucket
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'),
                "INVALID_BUCKET"
            );
            Ok(Self::S3 {
                endpoint: c.endpoint,
                bucket: c.bucket,
                region: c.region,
                access_env: c.access_env,
                secret_env: c.secret_env,
                prefix: String::new(),
            })
        } else {
            Ok(Self::Local(value.into()))
        }
    }
    pub fn scoped(self, org: uuid::Uuid) -> Self {
        match self {
            Self::Local(root) => Self::Local(root.join(org.to_string())),
            Self::S3 {
                endpoint,
                bucket,
                region,
                access_env,
                secret_env,
                ..
            } => Self::S3 {
                endpoint,
                bucket,
                region,
                access_env,
                secret_env,
                prefix: format!("{org}/"),
            },
        }
    }
    fn key(key: &str) -> anyhow::Result<()> {
        ensure!(
            key.len() == 64 && key.bytes().all(|c| c.is_ascii_hexdigit()),
            "INVALID_BLOB_KEY"
        );
        Ok(())
    }
    pub async fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        Self::key(key)?;
        ensure!(
            format!("{:x}", Sha256::digest(bytes)) == key,
            "CHECKSUM_MISMATCH"
        );
        match self {
            Self::Local(root) => {
                tokio::fs::create_dir_all(root).await?;
                let path = root.join(key);
                if path.exists()
                    && let Ok(existing) = self.get(key).await
                {
                    ensure!(existing == bytes, "BLOB_COLLISION");
                    return Ok(());
                }
                let temp = root.join(format!("{}.upload", uuid::Uuid::now_v7()));
                {
                    use tokio::io::AsyncWriteExt;
                    let mut f = tokio::fs::File::create(&temp).await?;
                    f.write_all(bytes).await?;
                    f.sync_all().await?;
                }
                tokio::fs::rename(&temp, &path).await?;
                std::fs::File::open(root)?.sync_all()?;
            }
            _ => {
                self.request("PUT", key, bytes).await?;
            }
        }
        ensure!(self.get(key).await? == bytes, "UPLOAD_VERIFY_FAILED");
        Ok(())
    }
    pub async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        Self::key(key)?;
        let bytes = match self {
            Self::Local(root) => tokio::fs::read(root.join(key)).await?,
            _ => self.request("GET", key, &[]).await?,
        };
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == key,
            "CHECKSUM_MISMATCH"
        );
        Ok(bytes)
    }
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        Self::key(key)?;
        match self {
            Self::Local(root) => match tokio::fs::remove_file(root.join(key)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
            _ => {
                self.request("DELETE", key, &[]).await?;
            }
        }
        Ok(())
    }
    async fn request(&self, method: &str, key: &str, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        let Self::S3 {
            endpoint,
            bucket,
            region,
            access_env,
            secret_env,
            prefix,
        } = self
        else {
            unreachable!()
        };
        let url = reqwest::Url::parse(&format!(
            "{}/{bucket}/{prefix}{key}",
            endpoint.trim_end_matches('/')
        ))?;
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().context("INVALID_HOST")?),
            None => url.host_str().context("INVALID_HOST")?.into(),
        };
        let now = chrono::Utc::now();
        let date = now.format("%Y%m%d").to_string();
        let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        let hash = format!("{:x}", Sha256::digest(bytes));
        let signed = "host;x-amz-content-sha256;x-amz-date";
        let canonical = format!(
            "{method}\n{}\n\nhost:{host}\nx-amz-content-sha256:{hash}\nx-amz-date:{stamp}\n\n{signed}\n{hash}",
            url.path()
        );
        let scope = format!("{date}/{region}/s3/aws4_request");
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{:x}",
            Sha256::digest(canonical.as_bytes())
        );
        fn mac(key: &[u8], data: &str) -> Vec<u8> {
            let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
            m.update(data.as_bytes());
            m.finalize().into_bytes().to_vec()
        }
        let secret = std::env::var(secret_env).context("S3_CREDENTIAL_UNAVAILABLE")?;
        let signing = mac(
            &mac(
                &mac(&mac(format!("AWS4{secret}").as_bytes(), &date), region),
                "s3",
            ),
            "aws4_request",
        );
        let signature = mac(&signing, &to_sign)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            std::env::var(access_env).context("S3_CREDENTIAL_UNAVAILABLE")?
        );
        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(60))
            .build()?
            .request(method.parse()?, url)
            .header("host", host)
            .header("x-amz-date", stamp)
            .header("x-amz-content-sha256", hash)
            .header("authorization", authorization)
            .body(bytes.to_vec())
            .send()
            .await?;
        ensure!(
            response.status().is_success() || (method == "DELETE" && response.status() == 404),
            "S3_REQUEST_FAILED: {}",
            response.status()
        );
        Ok(response.bytes().await?.to_vec())
    }
}
