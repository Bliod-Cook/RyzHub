use anyhow::Context;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root: std::path::PathBuf = std::env::var("RYZHUB_EXECUTION_ROOT")
        .context("RYZHUB_EXECUTION_ROOT required")?
        .into();
    let socket = root.join("host.sock");
    let config = ryzhub_execution::host::Config {
        root: root.clone(),
        containerd: std::env::var("CONTAINERD_ADDRESS")
            .unwrap_or_else(|_| "/run/containerd/containerd.sock".into()),
        guest_binary: std::env::var("RYZHUB_GUEST_BINARY")
            .context("RYZHUB_GUEST_BINARY required")?
            .into(),
        multi_tenant: std::env::var("RYZHUB_MULTI_TENANT").is_ok_and(|v| v == "true"),
        allowed_uid: std::env::var("RYZHUB_CONTROL_UID")
            .unwrap_or_else(|_| nix::unistd::getuid().as_raw().to_string())
            .parse()?,
        runsc_config: std::env::var_os("RYZHUB_RUNSC_CONFIG")
            .map(Into::into)
            .or_else(|| {
                root.join("runsc.toml")
                    .exists()
                    .then(|| root.join("runsc.toml"))
            }),
    };
    ryzhub_execution::host::Host::open(config)
        .await?
        .serve(socket)
        .await
}
