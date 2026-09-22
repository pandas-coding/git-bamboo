use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("git-workbench-engine starting");

    // TODO: Step 8 — start tower-lsp server over stdio
    Ok(())
}
