use std::time::Duration;

use tracing_subscriber::EnvFilter;

use git_workbench_engine::server::WorkbenchServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("git-workbench-engine starting");

    // Optionally watch the parent process and exit when it is gone, so a
    // crashed/killed extension cannot leak a running engine.
    if let Err(e) = spawn_parent_watch() {
        tracing::warn!(error = %e, "invalid --parent-pid argument; ignoring");
    }

    let (server, notify_rx) = WorkbenchServer::new();

    // SIGINT: request a graceful shutdown of the serve loop (installing the
    // handler replaces the default terminate behavior).
    {
        let server = std::sync::Arc::clone(&server);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("SIGINT received; shutting down");
                server.request_shutdown();
            }
        });
    }

    server.run(notify_rx).await;

    tracing::info!("git-workbench-engine stopped");
    Ok(())
}

/// Parse `--parent-pid <pid>` from argv and start the parent-watch thread.
/// Returns Ok(()) if no argument was given; an error describes a malformed
/// argument (the caller logs and continues without the watch).
fn spawn_parent_watch() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pid = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--parent-pid" {
            let value = iter
                .next()
                .ok_or_else(|| anyhow::anyhow!("--parent-pid requires a value"))?;
            pid = Some(
                value
                    .parse::<u32>()
                    .map_err(|e| anyhow::anyhow!("invalid --parent-pid {value:?}: {e}"))?,
            );
        }
    }
    let Some(pid) = pid else {
        return Ok(());
    };

    std::thread::Builder::new()
        .name("git-workbench-parent-watch".into())
        .spawn(move || parent_watch_loop(pid))?;
    Ok(())
}

/// Poll the parent every ~2s; exit(0) once it is gone.
#[cfg(unix)]
fn parent_watch_loop(pid: u32) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if !parent_alive(pid) {
            tracing::info!(pid, "parent process is gone; exiting");
            std::process::exit(0);
        }
    }
}

/// Poll the parent every ~2s; exit(0) once it is gone.
#[cfg(not(unix))]
fn parent_watch_loop(pid: u32) {
    // No portable way to poll another process's existence without extra
    // dependencies; documented no-op on non-unix platforms (the client is
    // expected to terminate the engine explicitly or via the shutdown RPC).
    let _ = pid;
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Check whether the process `pid` still exists (unix equivalent of
/// `kill(pid, 0)` == ESRCH means gone). Implemented without libc: shell
/// `kill -0`, which exits non-zero when the process does not exist.
#[cfg(unix)]
fn parent_alive(pid: u32) -> bool {
    match std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}
