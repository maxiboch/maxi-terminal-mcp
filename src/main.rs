use anyhow::Result;
use maxi_terminal_mcp::McpServer;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("maxi-terminal fatal error: {:?}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let server = McpServer::new();
    server.run().await
}
