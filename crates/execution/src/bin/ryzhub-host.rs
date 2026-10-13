use anyhow::Context;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root: std::path::PathBuf = std::env::var("RYZHUB_EXECUTION_ROOT")
        .context("RYZHUB_EXECUTION_ROOT required")?
        .into();
    // Settings come from the environment, else from `<root>/host.env` (KEY=VALUE lines), so
    // a node's settings can change without recreating the container it runs in.
    let file: std::collections::BTreeMap<String, String> =
        std::fs::read_to_string(root.join("host.env"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
            .collect();
    let var = |key: &str| std::env::var(key).ok().or_else(|| file.get(key).cloned());
    let socket = root.join("host.sock");
    let config = ryzhub_execution::host::Config {
        root: root.clone(),
        containerd: var("CONTAINERD_ADDRESS")
            .unwrap_or_else(|| "/run/containerd/containerd.sock".into()),
        guest_binary: var("RYZHUB_GUEST_BINARY")
            .context("RYZHUB_GUEST_BINARY required")?
            .into(),
        multi_tenant: var("RYZHUB_MULTI_TENANT").is_some_and(|v| v == "true"),
        allowed_uid: var("RYZHUB_CONTROL_UID")
            .unwrap_or_else(|| nix::unistd::getuid().as_raw().to_string())
            .parse()?,
        egress_per_minute: var("RYZHUB_EGRESS_PER_MINUTE").map_or(Ok(600), |v| v.parse())?,
        // A size in bytes enables the quota volume: an XFS image `<root>/volume.img`,
        // mounted at `<root>/volume`.
        volume: match var("RYZHUB_QUOTA_VOLUME_SIZE") {
            Some(size) => Some(ryzhub_execution::host::VolumeConfig {
                image: root.join("volume.img"),
                size: size.parse()?,
                sparse: var("RYZHUB_QUOTA_VOLUME_SPARSE").is_some_and(|v| v == "1"),
                mountpoint: root.join("volume"),
            }),
            None => None,
        },
        runsc_config: var("RYZHUB_RUNSC_CONFIG").map(Into::into).or_else(|| {
            root.join("runsc.toml")
                .exists()
                .then(|| root.join("runsc.toml"))
        }),
    };
    let host = ryzhub_execution::host::Host::open(config).await?;
    // A remote control plane reaches the node over TLS, both sides presenting certificates.
    if let Some(address) = var("RYZHUB_TLS_LISTEN") {
        let path = |key: &str| {
            var(key)
                .map(std::path::PathBuf::from)
                .with_context(|| format!("{key} required with RYZHUB_TLS_LISTEN"))
        };
        let acceptor = ryzhub_execution::transport::acceptor(
            &path("RYZHUB_TLS_CERT")?,
            &path("RYZHUB_TLS_KEY")?,
            &path("RYZHUB_TLS_CLIENT_CA")?,
        )?;
        let remote = host.clone();
        tokio::spawn(async move {
            if let Err(error) = remote.serve_tls(address, acceptor).await {
                eprintln!("TLS listener stopped: {error:#}");
            }
        });
    }
    host.serve(socket).await
}
