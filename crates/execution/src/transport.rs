//! How requests reach an agent: a Unix socket on the same machine, or TLS over TCP with
//! certificates both ways for an execution node on another machine.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadHalf, WriteHalf};

/// A connection to an agent.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub type ReadSide = BufReader<ReadHalf<Box<dyn Stream>>>;
pub type WriteSide = WriteHalf<Box<dyn Stream>>;

/// Where an agent listens.
#[derive(Clone)]
pub enum Endpoint {
    Unix(PathBuf),
    Tls {
        address: String,
        server_name: ServerName<'static>,
        connector: tokio_rustls::TlsConnector,
    },
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
            Self::Tls { address, .. } => write!(f, "tls:{address}"),
        }
    }
}

impl From<&Path> for Endpoint {
    fn from(path: &Path) -> Self {
        Self::Unix(path.into())
    }
}

impl From<&PathBuf> for Endpoint {
    fn from(path: &PathBuf) -> Self {
        Self::Unix(path.clone())
    }
}

impl From<PathBuf> for Endpoint {
    fn from(path: PathBuf) -> Self {
        Self::Unix(path)
    }
}

impl From<&Endpoint> for Endpoint {
    fn from(endpoint: &Endpoint) -> Self {
        endpoint.clone()
    }
}

/// The cryptography rustls uses here: ring, which builds for every target the nodes use.
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn certificates(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let certificates = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("reading {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        !certificates.is_empty(),
        "no certificate in {}",
        path.display()
    );
    Ok(certificates)
}

fn key(path: &Path) -> anyhow::Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(path).with_context(|| format!("reading {}", path.display()))
}

fn roots(ca: &Path) -> anyhow::Result<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    for certificate in certificates(ca)? {
        roots.add(certificate)?;
    }
    Ok(roots)
}

impl Endpoint {
    /// A node at `address` whose certificate names `server_name` and chains to `ca`; this side
    /// presents `cert` and `key`.
    pub fn tls(
        address: &str,
        server_name: &str,
        ca: &Path,
        cert: &Path,
        key_path: &Path,
    ) -> anyhow::Result<Self> {
        let config = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots(ca)?)
            .with_client_auth_cert(certificates(cert)?, key(key_path)?)?;
        Ok(Self::Tls {
            address: address.into(),
            server_name: ServerName::try_from(server_name.to_owned())?,
            connector: tokio_rustls::TlsConnector::from(Arc::new(config)),
        })
    }

    pub async fn connect(&self) -> std::io::Result<Box<dyn Stream>> {
        Ok(match self {
            Self::Unix(path) => Box::new(tokio::net::UnixStream::connect(path).await?),
            Self::Tls {
                address,
                server_name,
                connector,
            } => {
                let tcp = tokio::net::TcpStream::connect(address).await?;
                tcp.set_nodelay(true)?;
                Box::new(connector.connect(server_name.clone(), tcp).await?)
            }
        })
    }
}

/// The server side: this node's `cert` and `key`, accepting only clients whose certificate
/// chains to `client_ca`.
pub fn acceptor(
    cert: &Path,
    key_path: &Path,
    client_ca: &Path,
) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots(client_ca)?),
        provider(),
    )
    .build()?;
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates(cert)?, key(key_path)?)?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}
