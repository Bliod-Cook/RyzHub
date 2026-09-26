use anyhow::Context;
#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let id = args.get(1).context("sandbox id required")?.parse()?;
    let directory = args.get(2).context("journal directory required")?.into();
    let socket = args.get(3).context("socket path required")?.into();
    ryzhub_execution::guest::Guest::open(id, directory)?
        .serve(socket)
        .await
}
