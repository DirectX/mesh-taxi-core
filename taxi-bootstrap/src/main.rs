use std::time::Duration;

use clap::Parser;
use taxi_core::{Node, NodeConfig};

#[derive(Parser, Debug)]
#[command(
    name = "taxi-bootstrap",
    about = "Keep a plain veilid node up as a local bootstrap for testing"
)]
struct Cli {
    /// Data directory for the veilid node (default: temp dir)
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let mut cfg = NodeConfig::new("mesh_taxi_bootstrap");
    cfg.storage_dir = cli.data_dir.clone();
    let node = Node::start(cfg).await?;

    println!("[BOOT] local bootstrap node up, listening on the default ports");
    loop {
        let state = node.api().get_state().await?;
        let a = state.attachment;
        println!(
            "[BOOT] attach state: {:?} pub_ready={} local_ready={}",
            a.state,
            a.public_internet_ready,
            a.local_network_ready
        );
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
