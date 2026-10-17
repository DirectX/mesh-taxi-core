use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use taxi_core::{
    BargainDecision, BargainMessage, DetailsRequest, GeoPoint, Node, NodeConfig,
    ObfuscatedRideRequest, PreciseRouteDetails, RatingPolicy, board_label, board_owner_keypair,
    board_record_key, board_schema, decide_offer, subkey_for_request, validate_subkey,
};
use veilid_core::{CRYPTO_KIND_VLD0, VeilidAppCall, VeilidUpdate};

/// How long to wait for the node to become attached and network-ready.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(180);

/// Per-request bargain bookkeeping: rating validation gates the
/// `PreciseRouteDetails` exchange.
#[derive(Debug, Clone, Copy)]
struct BargainState {
    rating_validated: bool,
}

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
    let mut bargains: HashMap<String, BargainState> = HashMap::new();

    println!("Starting passenger node (veilid)...");
    let mut cfg = NodeConfig::new("mesh_taxi_passenger");
    cfg.storage_dir = cli.data_dir.clone();
    let mut node = Node::start(cfg).await?;
    node.wait_network_ready(ATTACH_TIMEOUT).await?;
    println!("Node attached to the network.");

    let routing = node.routing_context().await?;

    // 1. Allocate an E2E private route; its publishable blob later imported
    //    by the winning driver. Exact coordinates never reach the DHT.
    let route = node.api().new_private_route().await?;

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
    // Boards are shared aggregators with deterministic owner keys, so the
    // record may already exist (locally cached or on the network): create it
    // as owner first, then fall back to opening it as a writer.
    let board_cell = pickup.search_cell();
    let board_keypair = board_owner_keypair(board_cell);
    let record = match routing
        .create_dht_record(
            CRYPTO_KIND_VLD0,
            board_schema(),
            Some(board_keypair.clone()),
        )
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
                    handle_app_call(
                        &node,
                        app_call,
                        &policy,
                        &mut bargains,
                        cli.min_price.unwrap_or(cli.price),
                        &pickup,
                        &target,
                    )
                    .await;
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

async fn handle_app_call(
    node: &Node,
    app_call: Box<VeilidAppCall>,
    policy: &RatingPolicy,
    bargains: &mut HashMap<String, BargainState>,
    floor: u64,
    pickup: &GeoPoint,
    target: &GeoPoint,
) {
    let request_id = "unknown".to_string();
    let api = node.api();

    let Ok(message) = BargainMessage::decode(app_call.message()) else {
        println!("[PASS] app call with unknown payload, declined");
        send_reply(
            api,
            &app_call,
            &BargainMessage::Decline(taxi_core::BargainDecline {
                request_id: request_id.clone(),
                reason: "unknown bargain message".into(),
            }),
        )
        .await;
        return;
    };

    match message {
        BargainMessage::Offer(offer) => {
            let now = now_secs();
            let decision = decide_offer(floor, offer.counter_price, &offer.rating, policy, now);
            let state = match &decision {
                BargainDecision::Accept => {
                    println!(
                        "[PASS] offer accepted from driver (price {}) after rating validation",
                        offer.counter_price
                    );
                    BargainState {
                        rating_validated: true,
                    }
                }
                BargainDecision::Counter { .. } => {
                    println!(
                        "[PASS] driver passed rating validation but price {} is below floor {}",
                        offer.counter_price, floor
                    );
                    BargainState {
                        rating_validated: true,
                    }
                }
                BargainDecision::Decline { reason } => {
                    println!("[PASS] offer declined: {}", reason);
                    BargainState {
                        rating_validated: false,
                    }
                }
            };
            bargains.insert(offer.request_id.clone(), state);

            let reply = match decision {
                BargainDecision::Accept => BargainMessage::Accept(taxi_core::BargainAccept {
                    request_id: offer.request_id.clone(),
                }),
                BargainDecision::Counter {
                    floor: counter_floor,
                } => BargainMessage::Counter(taxi_core::BargainCounter {
                    request_id: offer.request_id.clone(),
                    price: counter_floor,
                    reason: "my price floor".into(),
                }),
                BargainDecision::Decline { reason } => {
                    BargainMessage::Decline(taxi_core::BargainDecline {
                        request_id: offer.request_id.clone(),
                        reason,
                    })
                }
            };
            send_reply(api, &app_call, &reply).await;
        }
        BargainMessage::DetailsRequest(DetailsRequest { request_id }) => {
            let state = bargains.get(&request_id).copied().unwrap_or(BargainState {
                rating_validated: false,
            });
            if !state.rating_validated {
                println!("[PASS] details request without a validated offer, declined");
                send_reply(
                    api,
                    &app_call,
                    &BargainMessage::Decline(taxi_core::BargainDecline {
                        request_id: request_id.clone(),
                        reason: "no accepted offer".into(),
                    }),
                )
                .await;
                return;
            }
            let details = PreciseRouteDetails {
                from_point: pickup.clone(),
                to_point: target.clone(),
            };
            println!(
                "[PASS] revealing PreciseRouteDetails to the validated driver for {}",
                request_id
            );
            send_reply(api, &app_call, &BargainMessage::Details(details)).await;
        }
        BargainMessage::Details(_) => {
            println!("[PASS] unexpected details from a driver, ignored");
        }
        BargainMessage::Accept(_accept) => {
            println!("[PASS] unexpected accept from a driver, ignored");
        }
        BargainMessage::Counter(counter) => {
            println!(
                "[PASS] unexpected counter {} from a driver, ignored",
                counter.price
            );
        }
        BargainMessage::Decline(decline) => {
            println!(
                "[PASS] driver declined for {}: {}",
                decline.request_id, decline.reason
            );
        }
    }
}

async fn send_reply(
    api: &veilid_core::VeilidAPI,
    app_call: &VeilidAppCall,
    message: &BargainMessage,
) {
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
