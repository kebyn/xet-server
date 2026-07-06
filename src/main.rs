//! Xet Storage server binary

use xet_server::config::ServerConfig;
use xet_server::server::start_server;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Load configuration
    let config = ServerConfig::try_from_env().map_err(std::io::Error::other)?;

    // Start server
    start_server(config).await
}
