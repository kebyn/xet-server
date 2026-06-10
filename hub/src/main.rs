use hub_api::config::HubConfig;
use hub_api::server::start_server;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();
    let config = HubConfig::from_env();
    start_server(config).await
}