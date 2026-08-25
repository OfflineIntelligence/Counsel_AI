// Offline_Intelligence/offline-intelligence/crates/src/main.rs

use offline_intelligence::{config::Config, run_thread_server};
use dotenvy::dotenv;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    
    let cfg = Config::from_env()?;
    
    println!("🚀 Starting with thread-based architecture (only mode available)");
    run_thread_server(cfg, None).await
}