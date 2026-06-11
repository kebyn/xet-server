use hub_api::config::HubConfig;
use hub_api::server::start_server;
use hub_api::auth::token_store::TokenStore;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();

    // Note: rustls crypto provider is initialized in CasClient::client()
    // using std::sync::Once to ensure it's only done once.

    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "create-token" {
        return create_token_command(&args[2..]);
    }

    let config = HubConfig::from_env();
    start_server(config).await
}

fn create_token_command(args: &[String]) -> std::io::Result<()> {
    // Parse --username, --scope, --name flags
    let mut username: Option<&str> = None;
    let mut scope: Option<&str> = None;
    let mut name: Option<&str> = None;
    let mut db_path: Option<&str> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--username" | "-u" => {
                i += 1;
                if i < args.len() {
                    username = Some(&args[i]);
                }
            }
            "--scope" | "-s" => {
                i += 1;
                if i < args.len() {
                    scope = Some(&args[i]);
                }
            }
            "--name" | "-n" => {
                i += 1;
                if i < args.len() {
                    name = Some(&args[i]);
                }
            }
            "--db" | "-d" => {
                i += 1;
                if i < args.len() {
                    db_path = Some(&args[i]);
                }
            }
            _ => {}
        }
        i += 1;
    }

    let username = username.unwrap_or("admin");
    let scope = scope.unwrap_or("write");
    let name = name.unwrap_or("default-token");
    let db_path = db_path.unwrap_or("hub.db");

    let token_store = TokenStore::new(db_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let token = token_store.create_token(username, name, scope)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    println!("Token created successfully!");
    println!("Username: {}", username);
    println!("Scope: {}", scope);
    println!("Token name: {}", name);
    println!("Token (keep this secret): {}", token);

    Ok(())
}