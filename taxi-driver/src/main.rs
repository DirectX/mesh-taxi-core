use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use h3o::CellIndex;
use taxi_core::{
    BargainAccept, BargainDecline, BargainMessage, DetailsRequest, DriverOffer, GeoPoint, MAX_ZONE,
    Node, NodeConfig, ObfuscatedRideRequest, PreciseRouteDetails, RatingClaim, board_label,
    board_record_key, scan_plan,
};
use veilid_core::{DHTReportScope, RoutingContext, Target, VeilidAPI};

/// How long to wait for the node to become attached and network-ready.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Parser, Debug)]
#[command(
    name = "taxi-driver",
    about = "Scan H3 zones for obfuscated ride requests and bargain over their private route"
)]
struct Cli {
    /// Current driver latitude
    #[arg(long)]
    lat: f64,
    /// Current driver longitude
    #[arg(long)]
    lon: f64,
    /// Maximum search zone (1 = own hexagon + 6 neighbours, then rings)
    #[arg(long, default_value_t = MAX_ZONE)]
    max_zone: u32,
    /// Seconds between scan rounds
    #[arg(long, default_value_t = 20)]
    scan_interval: u64,
    /// Ignore requests older than this many seconds (max age)
    #[arg(long, default_value_t = 3600)]
    max_age: u64,
    /// Counter-price offered on discovery (default: the passenger price)
    #[arg(long)]
    price: Option<u64>,
    /// Lowest counter-price the driver still accepts after a passenger bargain
    #[arg(long, default_value_t = 0)]
    min_price: u64,
    /// Send the bargain offer through the imported route blob
    #[arg(long, default_value_t = true)]
    offer: bool,
    /// Self-reported completed rides in the rating claim
    #[arg(long, default_value_t = 0)]
    rides: u32,
    /// Self-reported average rating in milli-units (4500 = 4.50 / 5)
    #[arg(long, default_value_t = 0)]
    average_rating: u32,
    /// Data directory for the veilid node (default: temp dir)
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let driver = GeoPoint::new(cli.lat, cli.lon)?;
    let board_cell = driver.search_cell();

    println!("Starting driver node (veilid)...");
    let cfg = NodeConfig::new("mesh_taxi_driver");
    let node = Node::start(cfg_with(&cli, cfg)).await?;
    node.wait_network_ready(ATTACH_TIMEOUT).await?;
    println!("Node attached to the network.");

    let routing = node.routing_context().await?;
    let mut seen = std::collections::HashSet::new();

    println!(
        "\n[DRV] scanning around own hexagon {} every {}s (zones 1..={})",
        board_label(board_cell),
        cli.scan_interval,
        cli.max_zone
    );

    loop {
        let now = now_secs();
        let mut requests = collect_requests(&routing, board_cell, cli.max_zone).await;
        requests.retain(|request| request.expires_at > now);

        let mut new_any = false;
        for request in requests {
            if seen.insert(request.request_id.clone()) {
                new_any = true;
                report_request(&driver, board_cell, &request);
                if cli.offer {
                    bargain(&node, &routing, &request, &driver_cell(board_cell), &cli).await;
                }
            }
        }

        if !new_any {
            println!("[DRV] no active ride requests this round");
        }

        let round = tokio::time::sleep(Duration::from_secs(cli.scan_interval));
        tokio::select! {
            _ = round => {}
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    node.shutdown().await;
    println!("Driver node stopped.");

    Ok(())
}

fn cfg_with(cli: &Cli, mut cfg: NodeConfig) -> NodeConfig {
    cfg.storage_dir = cli.data_dir.clone();
    cfg
}

fn driver_cell(board_cell: CellIndex) -> GeoPoint {
    GeoPoint::from_cell(board_cell)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock ok")
        .as_secs()
}

fn report_request(driver: &GeoPoint, center: CellIndex, request: &ObfuscatedRideRequest) {
    let zone = taxi_core::zone_of(center, request.from_point().search_cell(), MAX_ZONE);
    let distance = driver.distance_km(&request.from_point());
    println!(
        "  -> [Found!] {}\n     route {:x} -> {:x}, price {}\n     pickup is {:.2} km away, board zone {:?}. Import the request route_blob to connect.",
        request.request_id,
        request.from_cell_res9,
        request.to_cell_res9,
        request.price_offered,
        distance,
        zone,
    );
}

/// Driver -> passenger bargain over the imported route blob of a request:
///
/// 1. `app_call(Offer)` over the imported private route.
/// 2. On `Accept` / agreed `Counter`: `app_call(DetailsRequest)`.
/// 3. The passenger answers with `PreciseRouteDetails` once its rating
///    validation passed for the driver offer.
async fn bargain(
    node: &Node,
    routing: &RoutingContext,
    request: &ObfuscatedRideRequest,
    driver: &GeoPoint,
    cli: &Cli,
) {
    let api: &VeilidAPI = node.api();
    println!(
        "[DRV] importing route blob of {} for the bargain call",
        request.request_id
    );
    let route_id = match api.import_remote_private_route(request.route_blob.clone()) {
        Ok(route_id) => route_id,
        Err(e) => {
            println!("[DRV] route import failed: {}", e);
            return;
        }
    };

    let offer = BargainMessage::Offer(DriverOffer {
        request_id: request.request_id.clone(),
        counter_price: cli.price.unwrap_or(request.price_offered),
        rating: RatingClaim::now(cli.rides, cli.average_rating, now_secs()),
    });

    let staged = match send_call(routing, &route_id, &offer).await {
        Ok(reply) => reply,
        Err(e) => {
            println!("[DRV] offer call failed: {}", e);
            release(api, &route_id);
            return;
        }
    };

    match staged {
        BargainMessage::Accept(BargainAccept { request_id }) => {
            println!("[DRV] passenger accepted the offer for {}", request_id);
            settle(routing, &route_id, request, driver).await;
        }
        BargainMessage::Counter(counter) => {
            if counter.price >= cli.min_price {
                println!(
                    "[DRV] passenger countered: {}, driver agrees",
                    counter.price
                );
                settle(routing, &route_id, request, driver).await;
            } else {
                println!(
                    "[DRV] passenger counter {} below driver floor {}, declined",
                    counter.price, cli.min_price
                );
                decline(
                    routing,
                    &route_id,
                    &request.request_id,
                    "counter below driver floor",
                )
                .await;
                release(api, &route_id);
            }
        }
        BargainMessage::Decline(BargainDecline { reason, request_id }) => {
            println!(
                "[DRV] passenger declined offer for {}: {}",
                request_id, reason
            );
            release(api, &route_id);
        }
        other => {
            println!("[DRV] unexpected reply: {}", english(&other));
            release(api, &route_id);
        }
    }
}

/// Stage 2+: ask for the precise route and print the exchanged exact points.
async fn settle(
    routing: &RoutingContext,
    route_id: &veilid_core::RouteId,
    request: &ObfuscatedRideRequest,
    driver: &GeoPoint,
) {
    let ask = BargainMessage::DetailsRequest(DetailsRequest {
        request_id: request.request_id.clone(),
    });
    let reply = match send_call(routing, route_id, &ask).await {
        Ok(reply) => reply,
        Err(e) => {
            println!("[DRV] details call failed: {}", e);
            return;
        }
    };

    match reply {
        BargainMessage::Details(PreciseRouteDetails {
            from_point,
            to_point,
        }) => {
            println!(
                "[DRV] PreciseRouteDetails received! pickup {:.6},{:.6} ({:.2} km away), target {:.6},{:.6}",
                from_point.latitude,
                from_point.longitude,
                driver.distance_km(&from_point),
                to_point.latitude,
                to_point.longitude
            );
        }
        BargainMessage::Decline(BargainDecline { reason, request_id }) => {
            println!("[DRV] details declined for {}: {}", request_id, reason);
        }
        other => println!("[DRV] unexpected details reply: {}", english(&other)),
    }
}

async fn decline(
    routing: &RoutingContext,
    route_id: &veilid_core::RouteId,
    request_id: &str,
    reason: &str,
) {
    let message = BargainMessage::Decline(BargainDecline {
        request_id: request_id.to_string(),
        reason: reason.to_string(),
    });
    if let Err(e) = send_call(routing, route_id, &message).await {
        println!("[DRV] decline call failed: {}", e);
    }
}

async fn send_call(
    routing: &RoutingContext,
    route_id: &veilid_core::RouteId,
    message: &BargainMessage,
) -> Result<BargainMessage, Box<dyn std::error::Error>> {
    let bytes = routing
        .app_call(Target::RouteId(route_id.clone()), message.encode()?)
        .await?;
    BargainMessage::decode(&bytes).map_err(Into::into)
}

fn release(api: &VeilidAPI, route_id: &veilid_core::RouteId) {
    if let Err(e) = api.release_private_route(route_id.clone()) {
        println!("[DRV] route release failed: {}", e);
    }
}

fn english(message: &BargainMessage) -> &'static str {
    match message {
        BargainMessage::Offer(_) => "Offer",
        BargainMessage::Counter(_) => "Counter",
        BargainMessage::Accept(_) => "Accept",
        BargainMessage::Decline(_) => "Decline",
        BargainMessage::DetailsRequest(_) => "DetailsRequest",
        BargainMessage::Details(_) => "Details",
    }
}

/// Scans the board record of every cell within `max_zone` of `center`.
async fn collect_requests(
    routing: &RoutingContext,
    center: CellIndex,
    max_zone: u32,
) -> Vec<ObfuscatedRideRequest> {
    let mut requests = Vec::new();

    for zone in scan_plan(center, max_zone) {
        for cell in zone.cells() {
            let record_key = board_record_key(*cell);
            println!(
                "[DRV] zone {} scanning {} ({})",
                zone.id(),
                board_label(*cell),
                record_key
            );

            if routing
                .open_dht_record(record_key.clone(), None)
                .await
                .is_err()
            {
                continue;
            }

            let report = match routing
                .inspect_dht_record(record_key.clone(), None, DHTReportScope::SyncGet)
                .await
            {
                Ok(report) => report,
                Err(e) => {
                    println!("     inspect failed: {}", e);
                    let _ = routing.close_dht_record(record_key.clone()).await;
                    continue;
                }
            };

            for (subkey, local_seq, network_seq) in report
                .subkeys()
                .iter()
                .zip(report.local_seqs().iter())
                .zip(report.network_seqs().iter())
                .map(|((sk, lseq), nseq)| (sk, lseq, nseq))
            {
                if network_seq.is_none() && local_seq.is_none() {
                    continue;
                }
                let Some(dht_value) = routing
                    .get_dht_value(record_key.clone(), subkey, false)
                    .await
                    .ok()
                    .flatten()
                else {
                    continue;
                };
                if let Ok(request) = ObfuscatedRideRequest::decode(dht_value.data()) {
                    requests.push(request);
                }
            }

            let _ = routing.close_dht_record(record_key).await;
        }
    }

    requests
}
