use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use h3o::CellIndex;
use taxi_core::{
    BargainStep, GeoPoint, MAX_ZONE, Node, NodeConfig, ObfuscatedRideRequest, RatingClaim,
    bargain_request, board_label, collect_board_requests,
};

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
    let mut cfg = NodeConfig::new("mesh_taxi_driver");
    cfg.storage_dir = cli.data_dir.clone();
    let node = Node::start(cfg).await?;
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
        let mut requests = collect_board_requests(&routing, board_cell, cli.max_zone, |line| {
            println!("[DRV] {}", line)
        })
        .await;
        requests.retain(|request| request.expires_at > now);

        let mut new_any = false;
        for request in requests {
            if seen.insert(request.request_id.clone()) {
                new_any = true;
                report_request(&driver, board_cell, &request);
                if cli.offer {
                    let steps = bargain_request(
                        node.api(),
                        &routing,
                        &request,
                        cli.price,
                        RatingClaim::now(cli.rides, cli.average_rating, now),
                        cli.min_price,
                    )
                    .await;
                    for step in steps {
                        report_step(&driver, step);
                    }
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
        "  -> [Found!] {}\n     route {:x} -> {:x}, price {}\n     pickup is {:.2} km away, board zone {:?}. Bargaining over the route blob...",
        request.request_id,
        request.from_cell_res9,
        request.to_cell_res9,
        request.price_offered,
        distance,
        zone,
    );
}

fn report_step(driver: &GeoPoint, step: BargainStep) {
    match step {
        BargainStep::OfferSent(offer) => {
            println!(
                "[DRV] offer sent over the imported route blob (price {}, rides {}, rating {})",
                offer.counter_price, offer.rating.rides, offer.rating.average_milli
            );
        }
        BargainStep::AcceptReceived(accept) => {
            println!(
                "[DRV] passenger accepted the offer for {}",
                accept.request_id
            );
        }
        BargainStep::CounterReceived(counter) => {
            println!(
                "[DRV] passenger countered with {}, driver agrees",
                counter.price
            );
        }
        BargainStep::DeclineReceived(decline) => {
            println!(
                "[DRV] passenger declined for {}: {}",
                decline.request_id, decline.reason
            );
        }
        BargainStep::DetailsRequestSent(ask) => {
            println!("[DRV] asking for the precise route for {}", ask.request_id);
        }
        BargainStep::DetailsReceived(details) => {
            println!(
                "[DRV] PreciseRouteDetails received! pickup {:.6},{:.6}, {:.2} km away, target {:.6},{:.6}",
                details.from_point.latitude,
                details.from_point.longitude,
                driver.distance_km(&details.from_point),
                details.to_point.latitude,
                details.to_point.longitude
            );
        }
        BargainStep::CallFailed(reason) => println!("[DRV] {}", reason),
    }
}
