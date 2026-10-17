use std::time::{Duration, SystemTime};

use clap::Parser;
use taxi_core::{
    GeoPoint, Node, NodeConfig, ObfuscatedRideRequest, board_label, board_owner_keypair,
    board_record_key, board_schema, subkey_for_request, validate_subkey,
};
use veilid_core::CRYPTO_KIND_VLD0;

/// How long to wait for the node to become attached and network-ready.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Parser, Debug)]
#[command(name = "taxi-passenger", about = "Publish an obfuscated ride request on the H3 DHT board")]
struct Cli {
    /// Pickup latitude
    #[arg(long)]
    from_lat: f64,
    /// Pickup longitude
    #[arg(long)]
    from_lon: f64,
    /// Destination latitude
    #[arg(long)]
    to_lat: f64,
    /// Destination longitude
    #[arg(long)]
    to_lon: f64,
    /// Offered price (satoshi)
    #[arg(long, default_value_t = 25000)]
    price: u64,
    /// Request lifetime in seconds
    #[arg(long, default_value_t = 3600)]
    ttl_secs: u64,
    /// Data directory for the veilid node (default: temp dir)
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let pickup = GeoPoint::new(cli.from_lat, cli.from_lon)?;
    let target = GeoPoint::new(cli.to_lat, cli.to_lon)?;

    println!("Starting passenger node (veilid)...");
    let mut cfg = NodeConfig::new("mesh_taxi_passenger");
    cfg.storage_dir = cli.data_dir.clone();
    let node = Node::start(cfg).await?;
    node.wait_network_ready(ATTACH_TIMEOUT).await?;
    println!("Node attached to the network.");

    let routing = node.routing_context().await?;

    // 1. Allocate an E2E private route; its publishable blob later imported
    //    by the winning driver. Exact coordinates never reach the DHT.
    let route = node.api().new_private_route().await?;

    let expires_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs()
        + cli.ttl_secs;

    let request = ObfuscatedRideRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        route_blob: route.blob.clone(),
        from_cell_res9: pickup.position_cell().into(),
        to_cell_res9: target.position_cell().into(),
        price_offered: cli.price,
        expires_at,
    };

    // 2. Publish on the board of the pickup Res-8 hexagon.
    // Boards are shared aggregators with deterministic owner keys, so the
    // record may already exist (locally cached or on the network): create it
    // as owner first, then fall back to opening it as a writer.
    let board_cell = pickup.search_cell();
    let board_keypair = board_owner_keypair(board_cell);
    let record = match routing
        .create_dht_record(CRYPTO_KIND_VLD0, board_schema(), Some(board_keypair.clone()))
        .await
    {
        Ok(record) => record,
        Err(_) => {
            routing
                .open_dht_record(board_record_key(board_cell), Some(board_keypair))
                .await?
        }
    };

    let subkey = subkey_for_request(&request.request_id);
    validate_subkey(subkey)?;
    let bytes = request.encode()?;
    routing
        .set_dht_value(record.key(), subkey, bytes, None)
        .await?;

    let flushed = routing
        .flush_dht_record(record.key(), Some(Duration::from_secs(30)))
        .await?;
    println!(
        "\n[PASS] request {} published on board {} ({}), subkey {} (flushed={})",
        request.request_id,
        board_label(board_cell),
        record.key(),
        subkey,
        flushed
    );
    println!(
        "[PASS] obfuscated route: hex(a)={:x} -> hex(b)={:x}, price {}",
        request.from_cell_res9, request.to_cell_res9, request.price_offered
    );

    println!("\n[PASS] staying online so drivers can reach the route (Ctrl-C to stop)...");
    tokio::signal::ctrl_c().await?;

    routing.close_dht_record(record.key()).await?;
    node.shutdown().await;
    println!("Passenger node stopped.");

    Ok(())
}
