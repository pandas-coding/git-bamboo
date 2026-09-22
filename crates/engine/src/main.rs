use tracing_subscriber::EnvFilter;

use git_workbench_engine::server::WorkbenchServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("git-workbench-engine starting");

    let (server, notify_rx) = WorkbenchServer::new();
    server.run(notify_rx).await;

    tracing::info!("git-workbench-engine stopped");
    Ok(())
}
