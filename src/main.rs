use anyhow::Result;
use maxi_terminal_mcp::McpServer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let server = McpServer::new();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        tokio::select! {
            result = server.run() => result,
            _ = sigterm.recv() => {
                server.shutdown().await;
                Ok(())
            }
            _ = sigint.recv() => {
                server.shutdown().await;
                Ok(())
            }
        }
    }

    #[cfg(not(unix))]
    {
        use tokio::signal::ctrl_c;

        tokio::select! {
            result = server.run() => result,
            _ = ctrl_c() => {
                server.shutdown().await;
                Ok(())
            }
        }
    }
}
