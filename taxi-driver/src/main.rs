use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use h3o::CellIndex;
use taxi_core::{
    GeoPoint, Node, NodeConfig, ObfuscatedRideRequest, MAX_ZONE, board_label, board_record_key,
    scan_plan,
};
use veilid_core::DHTReportScope;

/// How long to wait for the node to become attached and network-ready.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Parser, Debug)]
#[command(name = "taxi-driver", about = "Scan H3 zones for obfuscated ride requests")]
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
        let mut requests = collect_requests(&routing, board_cell, cli.max_zone).await;
        requests.retain(|request| request.expires_at > now);

        let mut new_any = false;
        for request in requests {
            if seen.insert(request.request_id.clone()) {
                new_any = true;
                report_request(&driver, board_cell, &request);
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
        "  -> [Found!] {}\n     route {:x} -> {:x}, price {}\n     pickup is {:.2} km away, board zone {:?}. Import the request route_blob to connect.",
        request.request_id,
        request.from_cell_res9,
        request.to_cell_res9,
        request.price_offered,
        distance,
        zone,
    );
}

/// Scans the board record of every cell within `max_zone` of `center`.
async fn collect_requests(
    routing: &veilid_core::RoutingContext,
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

            if routing.open_dht_record(record_key.clone(), None).await.is_err() {
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
