use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use taxi_core::{
    BargainMessage, GeoPoint, Node, NodeConfig, ObfuscatedRideRequest, PassengerBargain,
    RatingPolicy, board_label, open_or_seed_board_record, subkey_for_request, validate_subkey,
};
use veilid_core::{VeilidAPI, VeilidAppCall, VeilidUpdate};

/// How long to wait for the node to become attached and network-ready.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Parser, Debug)]
#[command(
    name = "taxi-passenger",
    about = "Publish an obfuscated ride request on the H3 DHT board"
)]
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
    /// Lowest accepted price (default: the offered price)
    #[arg(long)]
    min_price: Option<u64>,
    /// Driver average-rating floor in milli-units (0 = open network)
    #[arg(long, default_value_t = 0)]
    min_rating: u32,
    /// Driver minimum completed rides (0 = open network)
    #[arg(long, default_value_t = 0)]
    min_rides: u32,
    /// Rating claim freshness window in seconds
    #[arg(long, default_value_t = 600)]
    claim_age: u64,
    /// Request lifetime in seconds
    #[arg(long, default_value_t = 3600)]
    ttl_secs: u64,
    /// Data directory for the veilid node (default: temp dir)
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// Bootstrap hosts/URLs (direct udp/tcp/ws URLs), defaults to veilid's
    /// public bootstrap
    #[arg(long)]
    bootstrap: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let pickup = GeoPoint::new(cli.from_lat, cli.from_lon)?;
    let target = GeoPoint::new(cli.to_lat, cli.to_lon)?;
    let policy = RatingPolicy {
        min_rides: cli.min_rides,
        min_average_milli: cli.min_rating,
        max_claim_age_secs: cli.claim_age,
        ..RatingPolicy::open_network()
    };
    let mut bargains = PassengerBargain::new(
        pickup.clone(),
        target.clone(),
        cli.min_price.unwrap_or(cli.price),
        policy,
    );
    println!("Starting passenger node (veilid)...");
    let mut cfg = NodeConfig::new("mesh_taxi_passenger");
    cfg.storage_dir = cli.data_dir.clone();
    if !cli.bootstrap.is_empty() {
        cfg.bootstrap = Some(cli.bootstrap.clone());
    }
    let mut node = Node::start(cfg).await?;
    node.wait_network_ready(ATTACH_TIMEOUT).await?;
    println!("Node attached to the network.");

    let routing = node.routing_context().await?;

    // 1. Allocate an E2E private route; its publishable blob later imported
    //    by the winning driver. Exact coordinates never reach the DHT.
    let route = node
        .api()
        .new_custom_private_route(veilid_core::PrivateSpec {
            hop_count: 1,
            stability: veilid_core::Stability::LowLatency,
            sequencing: veilid_core::Sequencing::PreferUnordered,
            crypto_kinds: vec![veilid_core::CRYPTO_KIND_VLD0],
        })
        .await?;

    let expires_at = now_secs() + cli.ttl_secs;

    let request = ObfuscatedRideRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        route_blob: route.blob.clone(),
        from_cell_res9: pickup.position_cell().into(),
        to_cell_res9: target.position_cell().into(),
        price_offered: cli.price,
        expires_at,
    };

    // 2. Publish on the board of the pickup Res-8 hexagon.
    // Boards never publish through the create descriptor (veilid embeds a
    // random encryption key there) but open under the deterministic
    // plaintext record key, so reads survive across sessions.
    let board_cell = pickup.search_cell();
    let (record, _board_notes) = open_or_seed_board_record(&routing, board_cell).await?;

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

    // 3. Answer driver `app_call`s arriving over the imported route blob:
    //    offer -> accept/counter/decline, then PreciseRouteDetails after the
    //    driver passes rating validation.
    let mut updates = node
        .take_update_stream()
        .expect("update stream not consumed yet");
    println!("\n[PASS] waiting for driver offers over the private route (Ctrl-C to stop)...");
    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Some(VeilidUpdate::AppCall(app_call)) => {
                    answer_call(node.api(), app_call, &mut bargains).await
                }
                Some(VeilidUpdate::Shutdown) => break,
                _ => {}
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    routing.close_dht_record(record.key()).await?;
    node.shutdown().await;
    println!("Passenger node stopped.");

    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock ok")
        .as_secs()
}

async fn answer_call(
    api: &VeilidAPI,
    app_call: Box<VeilidAppCall>,
    bargains: &mut PassengerBargain,
) {
    let reply = match BargainMessage::decode(app_call.message()) {
        Ok(message) => bargains.handle(message, now_secs()),
        Err(_) => {
            println!("[PASS] app call with unknown payload, declined");
            return;
        }
    };
    let Some(reply) = reply else {
        return;
    };

    for log in reply.logs {
        println!("[PASS] {}", log);
    }
    send_reply(api, &app_call, &reply.message).await;
}

async fn send_reply(api: &VeilidAPI, app_call: &VeilidAppCall, message: &BargainMessage) {
    let encoded = match message.encode() {
        Ok(bytes) => bytes,
        Err(e) => {
            println!("[PASS] failed to encode reply: {}", e);
            return;
        }
    };
    if let Err(e) = api.app_call_reply(app_call.id(), encoded).await {
        println!("[PASS] app_call_reply failed: {}", e);
    }
}
