//! Open Street Map web GUI for end-to-end testing of the mesh taxi workflow.
//!
//! The axum harness runs the passenger and the driver as two in-process
//! Veilid nodes (distinct `program_name + namespace`), serves a Leaflet/OSM
//! single page app and exposes the publish -> zone scan -> bargain chain as
//! JSON endpoints, so the whole DHT + route-blob flow is clickable on a map.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{Html, Json},
    routing::{get, post},
};
use h3o::CellIndex;
use tokio::sync::Mutex;
use veilid_core::VeilidUpdate;

use taxi_core::{
    BargainMessage, BargainStep, GeoPoint, MAX_ZONE, Node, NodeConfig, ObfuscatedRideRequest,
    PassengerBargain, RatingClaim, RatingPolicy, bargain_request, board_label, cell_label,
    collect_board_requests, open_or_seed_board_record, scan_plan, subkey_for_request,
    validate_subkey, zone_of,
};

/// Result alias for the JSON endpoints.
type ApiResult<T> = Result<T, (StatusCode, String)>;

/// Shared app state for the harness.
struct AppState {
    passenger: Arc<Node>,
    driver: Arc<Node>,
    /// Passenger bargain engine fed by the spawned update-stream task.
    bargain: Arc<Mutex<Option<PassengerBargain>>>,
    /// Ride requests discovered by the driver scans, keyed by request id.
    requests: Arc<Mutex<HashMap<String, ObfuscatedRideRequest>>>,
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(8088);

    let mut passenger_cfg = NodeConfig::new("mesh_taxi_web_passenger");
    passenger_cfg.namespace = "web".into();
    let mut driver_cfg = NodeConfig::new("mesh_taxi_web_driver");
    driver_cfg.namespace = "web".into();

    println!("Starting driver + passenger veilid nodes (in-process)...");
    let driver_node = Node::start(driver_cfg).await.expect("driver node");
    wait_ready(&driver_node, "driver").await;
    let mut passenger_node = Node::start(passenger_cfg).await.expect("passenger node");
    wait_ready(&passenger_node, "passenger").await;
    let driver = Arc::new(driver_node);
    println!("Both nodes attached to the network.");

    // Draining the update stream before moving the node into the shared state.
    let passenger_updates = passenger_node
        .take_update_stream()
        .expect("passenger update stream not consumed yet");
    let passenger_api = passenger_node.api().clone();

    let passenger = Arc::new(passenger_node);
    let state = Arc::new(AppState {
        passenger: passenger.clone(),
        driver: driver.clone(),
        bargain: Arc::new(Mutex::new(None)),
        requests: Arc::new(Mutex::new(HashMap::new())),
    });

    spawn_passenger_service(passenger_api, passenger_updates, state.bargain.clone());

    let app = Router::new()
        .route("/", get(index))
        .route(
            "/leaflet.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css")],
                    include_str!("assets/leaflet.min.css"),
                )
            }),
        )
        .route(
            "/leaflet.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    include_str!("assets/leaflet.min.js"),
                )
            }),
        )
        .route(
            "/images/marker-icon.png",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    include_bytes!("assets/images/marker-icon.png"),
                )
            }),
        )
        .route(
            "/images/marker-icon-2x.png",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    include_bytes!("assets/images/marker-icon-2x.png"),
                )
            }),
        )
        .route(
            "/images/marker-shadow.png",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    include_bytes!("assets/images/marker-shadow.png"),
                )
            }),
        )
        .route("/api/ride", post(publish_ride))
        .route("/api/driver/scan", post(driver_scan))
        .route("/api/driver/offer", post(driver_offer))
        .route("/api/cells", post(cells))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "web port {port} already in use: another taxi-web instance is running\n\
                 stop it (killall taxi-web) or pick another: PORT=8089 cargo run -p taxi-web"
            );
            std::process::exit(1);
        }
        Err(e) => panic!("bind port {port}: {e}"),
    };
    println!(
        "OSM test GUI listening on http://localhost:{} (Ctrl-C stops)",
        port
    );
    axum::serve(listener, app).await.expect("http server");
}

async fn index() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

/// Keeps answering driver `app_call`s over the passenger's private routes
/// through `PassengerBargain`; logs land on the local console.
fn spawn_passenger_service(
    api: veilid_core::VeilidAPI,
    mut updates: tokio::sync::mpsc::UnboundedReceiver<VeilidUpdate>,
    bargain: Arc<Mutex<Option<PassengerBargain>>>,
) {
    tokio::spawn(async move {
        while let Some(update) = updates.recv().await {
            let VeilidUpdate::AppCall(app_call) = update else {
                continue;
            };
            let Ok(message) = BargainMessage::decode(app_call.message()) else {
                println!("[web] app call with unknown payload, dropped");
                continue;
            };

            let mut bargain = bargain.lock().await;
            let Some(engine) = bargain.as_mut() else {
                println!("[web] bargain message before a ride is published, dropped");
                continue;
            };
            if let Some(reply) = engine.handle(message, now_secs()) {
                for log in reply.logs {
                    println!("[PASS] {}", log);
                }
                let encoded = match reply.message.encode() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        println!("[PASS] failed to encode reply: {}", e);
                        continue;
                    }
                };
                if let Err(e) = api.app_call_reply(app_call.id(), encoded).await {
                    println!("[PASS] app_call_reply failed: {}", e);
                }
            }
        }
    });
}

/// POST /api/ride: publish an obfuscated request from the pickup/target
/// clicked on the map.
#[derive(serde::Deserialize)]
struct PublishRide {
    from_lat: f64,
    from_lon: f64,
    to_lat: f64,
    to_lon: f64,
    price: u64,
    ttl_secs: u64,
    min_price: Option<u64>,
    min_rating: u32,
}

#[derive(serde::Serialize)]
struct PublishReply {
    request_id: String,
    board_label: String,
    record_key: String,
    subkey: u32,
    transcript: Vec<String>,
}

async fn publish_ride(
    State(state): State<Arc<AppState>>,
    body: Option<Json<PublishRide>>,
) -> ApiResult<Json<PublishReply>> {
    let Json(PublishRide {
        from_lat,
        from_lon,
        to_lat,
        to_lon,
        price,
        ttl_secs,
        min_price,
        min_rating,
    }) = body.ok_or_else(|| bad_request("missing JSON body"))?;
    let pickup = GeoPoint::new(from_lat, from_lon).map_err(bad_request)?;
    let target = GeoPoint::new(to_lat, to_lon).map_err(bad_request)?;

    let policy = RatingPolicy {
        min_average_milli: min_rating,
        max_claim_age_secs: 3600, // generous window for map-driven testing
        ..RatingPolicy::open_network()
    };

    let mut transcript = vec![format!(
        "publishing request (price {}, floor {}, rating floor {})",
        price,
        min_price.unwrap_or(price),
        min_rating
    )];

    let routing = state
        .passenger
        .routing_context()
        .await
        .map_err(bad_request)?;
    let route = state
        .passenger
        .api()
        .new_private_route()
        .await
        .map_err(bad_request)?;

    let expires_at = now_secs() + ttl_secs;
    let request = ObfuscatedRideRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        route_blob: route.blob.clone(),
        from_cell_res9: pickup.position_cell().into(),
        to_cell_res9: target.position_cell().into(),
        price_offered: price,
        expires_at,
    };

    let mut bargain = state.bargain.lock().await;
    *bargain = Some(PassengerBargain::new(
        pickup.clone(),
        target.clone(),
        min_price.unwrap_or(price),
        policy,
    ));

    let board_cell = pickup.search_cell();
    let (record, board_notes) = open_or_seed_board_record(&routing, board_cell)
        .await
        .map_err(bad_request)?;
    transcript.extend(board_notes);

    let subkey = subkey_for_request(&request.request_id);
    validate_subkey(subkey).map_err(bad_request)?;
    let bytes = request.encode().map_err(bad_request)?;
    routing
        .set_dht_value(record.key(), subkey, bytes, None)
        .await
        .map_err(bad_request)?;
    let flushed = routing
        .flush_dht_record(record.key(), Some(Duration::from_secs(30)))
        .await
        .map_err(bad_request)?;

    let board = board_label(board_cell);
    transcript.push(format!(
        "request {} published on board {} ({}), subkey {}, flushed {}",
        request.request_id,
        board,
        record.key(),
        subkey,
        flushed
    ));
    let record_key = record.key().to_string();

    state
        .requests
        .lock()
        .await
        .insert(request.request_id.clone(), request.clone());

    Ok(Json(PublishReply {
        request_id: request.request_id.clone(),
        board_label: board,
        record_key,
        subkey,
        transcript,
    }))
}

/// POST /api/driver/scan: zone search as the driver, distances from the
/// driver position supplied from the map.
#[derive(serde::Deserialize)]
struct DriverScan {
    lat: f64,
    lon: f64,
    max_zone: u32,
}

#[derive(serde::Serialize)]
struct RequestView {
    request_id: String,
    price: u64,
    distance_km: f64,
    zone: u32,
    from_point: PointView,
    to_point: PointView,
}

#[derive(serde::Serialize)]
struct PointView {
    latitude: f64,
    longitude: f64,
}

#[derive(serde::Serialize)]
struct ScanReply {
    requests: Vec<RequestView>,
    transcript: Vec<String>,
}

async fn driver_scan(
    State(state): State<Arc<AppState>>,
    body: Option<Json<DriverScan>>,
) -> ApiResult<Json<ScanReply>> {
    let Json(DriverScan { lat, lon, max_zone }) =
        body.ok_or_else(|| bad_request("missing JSON body"))?;
    let driver = GeoPoint::new(lat, lon).map_err(bad_request)?;
    let board_cell = driver.search_cell();

    let routing = state.driver.routing_context().await.map_err(bad_request)?;
    let t0 = std::time::Instant::now();
    let t0 = std::sync::Arc::new(t0);
    let mut requests = collect_board_requests(&routing, board_cell, max_zone, {
        let t0 = t0.clone();
        move |line| {
            println!("[scan {:?}] {}", t0.elapsed(), line);
        }
    })
    .await;
    println!("[scan total] {:?}", t0.elapsed());
    requests.retain(|request| request.expires_at > now_secs());
    {
        let mut known = state.requests.lock().await;
        for request in &requests {
            known
                .entry(request.request_id.clone())
                .or_insert(request.clone());
        }
    }

    let transcript = vec![format!("scan finished: {} request(s)", requests.len())];
    let mut views = Vec::new();
    for request in requests {
        let zone = zone_of(board_cell, request.from_point().search_cell(), MAX_ZONE)
            .ok_or_else(|| bad_request("request outside the scan plan"))?;
        views.push(RequestView {
            request_id: request.request_id.clone(),
            price: request.price_offered,
            distance_km: driver.distance_km(&request.from_point()),
            zone,
            from_point: PointView {
                latitude: request.from_point().latitude,
                longitude: request.from_point().longitude,
            },
            to_point: PointView {
                latitude: request.to_point().latitude,
                longitude: request.to_point().longitude,
            },
        });
    }

    Ok(Json(ScanReply {
        transcript,
        requests: views,
    }))
}

/// POST /api/driver/offer: run the full bargain chain over the imported
/// route blob and reply with the step transcript (plus exact details on
/// success).
#[derive(serde::Deserialize)]
struct DriverOffer {
    request_id: String,
    lat: f64,
    lon: f64,
    min_price: u64,
    rides: u32,
    average_rating: u32,
}

#[derive(serde::Serialize)]
struct OfferReply {
    transcript: Vec<String>,
    details: Option<serde_json::Value>,
}

async fn driver_offer(
    State(state): State<Arc<AppState>>,
    body: Option<Json<DriverOffer>>,
) -> ApiResult<Json<OfferReply>> {
    let Json(DriverOffer {
        request_id,
        lat,
        lon,
        min_price,
        rides,
        average_rating,
    }) = body.ok_or_else(|| bad_request("missing JSON body"))?;
    let driver = GeoPoint::new(lat, lon).map_err(bad_request)?;

    let request = state
        .requests
        .lock()
        .await
        .get(&request_id)
        .cloned()
        .ok_or_else(|| bad_request("unknown request id"))?;

    let routing = state.driver.routing_context().await.map_err(bad_request)?;
    let steps = bargain_request(
        state.driver.api(),
        &routing,
        &request,
        None,
        RatingClaim::now(rides, average_rating, now_secs()),
        min_price,
    )
    .await;

    let mut transcript = Vec::new();
    let mut details_bytes = None;
    for step in &steps {
        let (line, details) = describe_step(&driver, step);
        transcript.push(line);
        details_bytes = details;
    }

    Ok(Json(OfferReply {
        transcript,
        details: details_bytes,
    }))
}

fn describe_step(driver: &GeoPoint, step: &BargainStep) -> (String, Option<serde_json::Value>) {
    match step {
        BargainStep::OfferSent(offer) => (
            format!(
                "offer sent over route blob (price {}, claimed rides {}, rating {})",
                offer.counter_price, offer.rating.rides, offer.rating.average_milli
            ),
            None,
        ),
        BargainStep::AcceptReceived(_) => ("passenger accepted the offer".into(), None),
        BargainStep::CounterReceived(counter) => (
            format!("passenger countered with {}, driver agrees", counter.price),
            None,
        ),
        BargainStep::DeclineReceived(decline) => {
            (format!("passenger declined: {}", decline.reason), None)
        }
        BargainStep::DetailsRequestSent(_) => ("asking for the precise route".into(), None),
        BargainStep::DetailsReceived(details) => (
            format!(
                "PreciseRouteDetails received: pickup {:.6},{:.6} ({:.2} km away), target {:.6},{:.6}",
                details.from_point.latitude,
                details.from_point.longitude,
                driver.distance_km(&details.from_point),
                details.to_point.latitude,
                details.to_point.longitude
            ),
            Some(serde_json::json!({
                "from_point": {
                    "latitude": details.from_point.latitude,
                    "longitude": details.from_point.longitude
                },
                "to_point": {
                    "latitude": details.to_point.latitude,
                    "longitude": details.to_point.longitude
                }
            })),
        ),
        BargainStep::CallFailed(reason) => (reason.clone(), None),
    }
}

/// Computes cell label prefixes for the H3 overlay endpoints.

#[derive(serde::Serialize)]
struct CellView {
    id: String,
    res: u8,
    ring: Option<u32>,
    points: Vec<[f64; 2]>,
}

#[derive(serde::Serialize)]
struct CellsReply {
    zone_cells: Vec<CellView>,
    position_cells: Vec<CellView>,
}

#[derive(serde::Deserialize)]
struct CellsRequest {
    lat: f64,
    lon: f64,
    max_zone: u32,
    res9: Vec<[f64; 2]>,
}

/// GET-equivalent POST /api/cells: boundary polygons of every Res 8 board
/// cell around the driver plan plus the Res 9 position cells of the marked
/// points, for the H3 hex overlay in the GUI.
async fn cells(body: Option<Json<CellsRequest>>) -> ApiResult<Json<CellsReply>> {
    let Json(CellsRequest {
        lat,
        lon,
        max_zone,
        res9,
    }) = body.ok_or_else(|| bad_request("missing JSON body"))?;
    let driver = GeoPoint::new(lat, lon).map_err(bad_request)?;

    let mut zone_cells = Vec::new();
    for zone in scan_plan(driver.search_cell(), max_zone) {
        for cell in zone.cells() {
            zone_cells.push(CellView {
                id: board_label(*cell),
                res: 8,
                ring: Some(zone.id()),
                points: cell_points(*cell),
            });
        }
    }

    let mut position_cells = Vec::new();
    for point in res9 {
        let position = GeoPoint::new(point[0], point[1]).map_err(bad_request)?;
        let cell = position.position_cell();
        position_cells.push(CellView {
            id: cell_label(cell),
            res: 9,
            ring: None,
            points: cell_points(cell),
        });
    }

    Ok(Json(CellsReply {
        zone_cells,
        position_cells,
    }))
}

fn cell_points(cell: CellIndex) -> Vec<[f64; 2]> {
    cell.boundary()
        .iter()
        .map(|ll| [ll.lat(), ll.lng()])
        .collect()
}

/// Waits for attach while printing attachment state transitions, so a slow
/// or stalled node shows where it stops instead of a silent 180 s timeout.
async fn wait_ready(node: &Node, role: &'static str) {
    let timeout = Duration::from_secs(180);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::new();
    loop {
        let state = node.api().get_state().await.expect("veilid state");
        let rendered = format!(
            "{:?} public_ready={}",
            state.attachment.state, state.attachment.public_internet_ready
        );
        if rendered != last {
            println!("[web] {} attach: {}", role, rendered);
            last = rendered;
        }
        if state.attachment.public_internet_ready && state.attachment.state.is_attached() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{} attach: Timeout(180s), last state: {}", role, last);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock ok")
        .as_secs()
}

fn bad_request(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
}
