//! JNI bridge for the mesh-taxi Android apps.
//!
//! Each app instance hosts its own veilid node. The Kotlin side calls plain
//! synchronous functions that map to JSON strings; heavy work runs on an
//! in-process tokio runtime. Handles are opaque numbers into a global slot
//! map (class `dev.meshtaxi.android.MeshTaxiCore`). Failures are reported as
//! `{"error": "..."}` in the returned JSON instead of JVM exceptions, so the
//! Kotlin layer only ever deals with data.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use h3o::CellIndex;
use jni::JNIEnv;
use jni::objects::{JClass, JDoubleArray, JString};
use jni::sys::{jdouble, jint, jlong, jstring};
use taxi_core::{
    BargainMessage, BargainStep, GeoPoint, MAX_ZONE, Node, NodeConfig, ObfuscatedRideRequest,
    PassengerBargain, PassengerReply, RatingClaim, RatingPolicy, bargain_request, board_label,
    cell_label, collect_board_requests, open_or_seed_board_record, scan_plan, subkey_for_request,
    validate_subkey, zone_of,
};
use veilid_core::VeilidAPI;

/// Hard cap for the hex overlay scan plan, mirrors the web harness.
const OVERLAY_ZONE_CAP: u32 = 6;

/// One app-side node context (passenger or driver role).
struct Core {
    rt: tokio::runtime::Runtime,
    api: VeilidAPI,
    /// Passenger: bargained ride armed on publish, answers driver app calls.
    bargain: Arc<tokio::sync::Mutex<Option<PassengerBargain>>>,
    /// Driver: requests discovered by scans, keyed by request id.
    requests: Arc<tokio::sync::Mutex<HashMap<String, ObfuscatedRideRequest>>>,
    /// UI event stream drained by `pollEvents`.
    events: Arc<Mutex<Vec<serde_json::Value>>>,
    role: String,
}

impl Core {
    fn log(&self, kind: &str, text: impl AsRef<str>) {
        let mut events = self.events.lock().expect("events lock");
        events.push(serde_json::json!({ "kind": kind, "text": text.as_ref() }));
    }

    async fn state_json(&self) -> serde_json::Value {
        match self.api.get_state().await {
            Ok(state) => serde_json::json!({
                "state": format!("{:?}", state.attachment.state),
                "public_ready": state.attachment.public_internet_ready,
            }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        }
    }
}

static CORES: OnceLock<Mutex<HashMap<jlong, Arc<Core>>>> = OnceLock::new();
static NEXT_HANDLE: OnceLock<Mutex<jlong>> = OnceLock::new();

fn cores() -> &'static Mutex<HashMap<jlong, Arc<Core>>> {
    CORES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_handle() -> jlong {
    let mut next = NEXT_HANDLE
        .get_or_init(|| Mutex::new(1000))
        .lock()
        .expect("handle counter");
    *next += 1;
    *next
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock ok")
        .as_secs()
}

fn ok_json<T: serde::Serialize>(value: T) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{\"error\":\"json encode failed\"}".into())
}

fn err_json<E: std::fmt::Display>(error: E) -> String {
    ok_json(serde_json::json!({ "error": error.to_string() }))
}

fn lookup(handle: jlong) -> Result<Arc<Core>, String> {
    cores()
        .lock()
        .expect("cores map")
        .get(&handle)
        .cloned()
        .ok_or_else(|| format!("unknown core handle {handle}"))
}

fn return_json(env: &mut JNIEnv, json: String) -> jstring {
    env.new_string(json)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

// -- passenger background service ----------------------------------------------

/// Answers driver offers over the passenger's imported private routes,
/// logging every step into the UI event stream. Veilid log events surface
/// for every role so attachment progress is visible in the app transcript.
async fn run_update_pump(
    core: Arc<Core>,
    mut updates: tokio::sync::mpsc::UnboundedReceiver<veilid_core::VeilidUpdate>,
) {
    core.log("app", format!("update pump up (role {})", core.role));
    while let Some(update) = updates.recv().await {
        let app_call = match update {
            veilid_core::VeilidUpdate::Log(log) => {
                core.log("net", format!("{}: {}", log.log_level, log.message));
                continue;
            }
            veilid_core::VeilidUpdate::AppCall(app_call) => app_call,
            _ => continue,
        };
        let Ok(message) = BargainMessage::decode(app_call.message()) else {
            core.log("app", "app call with unknown payload, dropped");
            continue;
        };

        let reply = {
            let mut bargain = core.bargain.lock().await;
            match bargain.as_mut() {
                Some(engine) => engine.handle(message, now_secs()),
                None => {
                    core.log("app", "bargain message before a ride is published, dropped");
                    None
                }
            }
        };

        if let Some(PassengerReply { message, logs }) = reply {
            for log in logs {
                core.log("app", log);
            }
            if let Ok(encoded) = message.encode() {
                match core.api.app_call_reply(app_call.id(), encoded).await {
                    Ok(_) => core.log("app", "reply sent"),
                    Err(e) => core.log("app", format!("app_call_reply failed: {}", e)),
                }
            }
        }
    }
}

// -- JNI: node lifecycle ----------------------------------------------------------

/// Starts a node of `role` ("passenger" | "driver") with storage at
/// `storage_dir`; returns `{"handle": n, "role": ...}` or `{"error": ...}`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_initAndroid(
    env: jni::JNIEnv,
    _class: JClass,
    ctx: jni::sys::jobject,
) {
    use jni22::{sys as sys22, EnvUnowned as EnvUnowned22};

    let ptr: *mut sys22::JNIEnv = env.get_raw().cast::<sys22::JNIEnv>();
    let ctx22 = unsafe { <jni22::objects::JObject as jni22::refs::Reference>::kind_from_raw(ctx.cast()) };
    let mut unowned = unsafe { EnvUnowned22::from_raw(ptr) };
    veilid_core::veilid_core_setup_android(unowned, ctx22);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_startNode(
    mut env: JNIEnv,
    _class: JClass,
    role: JString,
    storage_dir: JString,
) -> jstring {
    let Ok(role) = env
        .get_string(&role)
        .map(|s| s.to_string_lossy().into_owned())
    else {
        return return_json(&mut env, err_json("role string missing"));
    };
    let Ok(storage) = env
        .get_string(&storage_dir)
        .map(|s| s.to_string_lossy().into_owned())
    else {
        return return_json(&mut env, err_json("storage dir string missing"));
    };

    let mut config = NodeConfig::new(&format!("mesh_taxi_{role}"));
    config.namespace = "android".into();
    config.storage_dir = Some(std::path::PathBuf::from(&storage));

    // Surface veilid log events through the node update stream on first start.
    let _ = veilid_core::VeilidTracing::try_init([veilid_core::LogOutput::api()
        .with_common_log_level(veilid_core::VeilidConfigLogLevel::Info)]);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return return_json(&mut env, err_json(e)),
    };

    let mut node = match rt.block_on(Node::start(config)) {
        Ok(node) => node,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    let updates = node
        .take_update_stream()
        .expect("fresh node has an update stream");

    let handle = next_handle();
    let core = Arc::new(Core {
        rt,
        api: node.api().clone(),
        bargain: Arc::new(tokio::sync::Mutex::new(None)),
        requests: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        events: Arc::new(Mutex::new(Vec::new())),
        role: role.clone(),
    });

    let pump = core.clone();
    core.rt.spawn(run_update_pump(pump, updates));

    cores().lock().expect("cores map").insert(handle, core);

    return_json(
        &mut env,
        ok_json(serde_json::json!({ "handle": handle, "role": role })),
    )
}

/// Blocking wait until the node is attached and public-internet ready.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_waitReady(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    timeout_ms: jlong,
) -> jstring {
    let core = match lookup(handle) {
        Ok(core) => core,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    let ready = core.rt.block_on(async {
        core.api
            .get_state()
            .await
            .ok()
            .map(|state| {
                state.attachment.public_internet_ready && state.attachment.state.is_attached()
            })
            .unwrap_or(false)
    });
    // Note: the first attachment usually needs tens of seconds; the blocking
    // loop below re-uses `wait_network_ready`-style polling on the runtime.
    if ready {
        return return_json(&mut env, ok_json(serde_json::json!({ "ready": true })));
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        let state = core.rt.block_on(core.state_json());
        let public_ready = state
            .get("public_ready")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let attached = state
            .get("state")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("Attached"))
            .unwrap_or(false);
        if public_ready && attached {
            ready = true;
            break;
        }
    }
    return_json(&mut env, ok_json(serde_json::json!({ "ready": ready })))
}

/// One-shot attachment snapshot (also pollable while another thread blocks).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_attachState(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let core = match lookup(handle) {
        Ok(core) => core,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    return_json(&mut env, ok_json(core.rt.block_on(core.state_json())))
}

/// Runs a veilid debug word (e.g. "peers", "dialinfo", "restartnetwork").
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_debugNet(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    args: JString,
) -> jstring {
    let core = match lookup(handle) {
        Ok(core) => core,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    let Ok(args) = env
        .get_string(&args)
        .map(|s| s.to_string_lossy().into_owned())
    else {
        return return_json(&mut env, err_json("debug args string missing"));
    };
    let result = core.rt.block_on(core.api.debug(args));
    match result {
        Ok(text) => return_json(
            &mut env,
            ok_json(serde_json::json!({ "text": text })),
        ),
        Err(e) => return_json(&mut env, err_json(e)),
    }
}

/// Shuts the node down and forgets the handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_shutdown(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let core = match lookup(handle) {
        Ok(core) => core,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    cores().lock().expect("cores map").remove(&handle);
    core.rt.block_on(async {
        let _ = core.api.detach().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        core.api.clone().shutdown().await;
    });
    return_json(&mut env, ok_json(serde_json::json!({ "shut": true })))
}

// -- JNI: passenger publish ---------------------------------------------------------

/// Publishes an obfuscated ride request and arms the bargain engine.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_publishRide(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    from_lat: jdouble,
    from_lon: jdouble,
    to_lat: jdouble,
    to_lon: jdouble,
    price: jlong,
    ttl_secs: jlong,
    min_price: jlong,
    min_rating: jint,
) -> jstring {
    let core = match lookup(handle) {
        Ok(core) => core,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    let pickup = match GeoPoint::new(from_lat, from_lon) {
        Ok(point) => point,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    let target = match GeoPoint::new(to_lat, to_lon) {
        Ok(point) => point,
        Err(e) => return return_json(&mut env, err_json(e)),
    };

    let policy = RatingPolicy {
        min_average_milli: min_rating.max(0) as u32,
        max_claim_age_secs: 3600,
        ..RatingPolicy::open_network()
    };

    let result = core.rt.block_on(async {
        let routing = core.api.routing_context()?;
        let route = core.api.new_private_route().await?;

        let request = ObfuscatedRideRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            route_blob: route.blob.clone(),
            from_cell_res9: pickup.position_cell().into(),
            to_cell_res9: target.position_cell().into(),
            price_offered: price.max(0) as u64,
            expires_at: now_secs() + ttl_secs.max(0) as u64,
        };

        {
            let mut bargain = core.bargain.lock().await;
            *bargain = Some(PassengerBargain::new(
                pickup.clone(),
                target.clone(),
                if min_price < 0 {
                    request.price_offered
                } else {
                    min_price as u64
                },
                policy,
            ));
        }

        let board_cell = pickup.search_cell();
        let (record, notes) = open_or_seed_board_record(&routing, board_cell).await?;

        let subkey = subkey_for_request(&request.request_id);
        validate_subkey(subkey)?;
        let bytes = request.encode()?;
        routing
            .set_dht_value(record.key(), subkey, bytes, None)
            .await?;
        let flushed = routing
            .flush_dht_record(record.key(), Some(Duration::from_secs(30)))
            .await?;

        Ok::<_, taxi_core::CoreError>((
            board_label(board_cell),
            record.key().to_string(),
            subkey,
            flushed,
            notes,
            request,
        ))
    });

    let (board, record_key, subkey, flushed, notes, request) = match result {
        Ok(parts) => parts,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    core.log(
        "app",
        format!(
            "request {} published on board {} subkey {}",
            request.request_id, board, subkey
        ),
    );

    return_json(
        &mut env,
        ok_json(serde_json::json!({
            "request_id": request.request_id,
            "board_label": board,
            "record_key": record_key,
            "subkey": subkey,
            "flushed": flushed,
            "notes": notes,
        })),
    )
}

// -- JNI: driver scan + bargain ------------------------------------------------------

/// Zone scan: live ride requests around the driver position.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_scan(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    lat: jdouble,
    lon: jdouble,
    max_zone: jint,
) -> jstring {
    let core = match lookup(handle) {
        Ok(core) => core,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    let driver = match GeoPoint::new(lat, lon) {
        Ok(point) => point,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    let board_cell = driver.search_cell();

    let scanned = core.rt.block_on(async {
        let routing = core.api.routing_context()?;
        let mut requests =
            collect_board_requests(&routing, board_cell, max_zone.max(1) as u32, |_| {}).await;
        requests.retain(|request| request.expires_at > now_secs());

        let mut known = core.requests.lock().await;
        let mut views = Vec::new();
        for request in requests {
            known
                .entry(request.request_id.clone())
                .or_insert(request.clone());
            let zone = zone_of(board_cell, request.from_point().search_cell(), MAX_ZONE)
                .unwrap_or(MAX_ZONE);
            views.push(serde_json::json!({
                "request_id": request.request_id,
                "price": request.price_offered,
                "distance_km": driver.distance_km(&request.from_point()),
                "zone": zone,
                "from_point": request.from_point(),
                "to_point": request.to_point(),
            }));
        }
        drop(known);
        Ok::<_, taxi_core::CoreError>(views)
    });

    let views = match scanned {
        Ok(views) => views,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    core.log("scan", format!("scan found {} request(s)", views.len()));

    return_json(&mut env, ok_json(serde_json::json!({ "requests": views })))
}

/// Runs the bargain chain for one request id, returns transcript + details.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_offer(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    request_id: JString,
    lat: jdouble,
    lon: jdouble,
    min_price: jlong,
    rides: jint,
    average_rating: jint,
) -> jstring {
    let core = match lookup(handle) {
        Ok(core) => core,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    let Ok(request_id) = env
        .get_string(&request_id)
        .map(|s| s.to_string_lossy().into_owned())
    else {
        return return_json(&mut env, err_json("request id string missing"));
    };
    let driver = match GeoPoint::new(lat, lon) {
        Ok(point) => point,
        Err(e) => return return_json(&mut env, err_json(e)),
    };

    let steps = core.rt.block_on(async {
        let request = core.requests.lock().await.get(&request_id).cloned();
        let Some(request) = request else {
            return Err(taxi_core::CoreError::Encoding(format!(
                "unknown request id {request_id}"
            )));
        };

        let routing = core.api.routing_context()?;
        Ok(bargain_request(
            &core.api,
            &routing,
            &request,
            None,
            RatingClaim::now(
                rides.max(0) as u32,
                average_rating.max(0) as u32,
                now_secs(),
            ),
            min_price.max(0) as u64,
        )
        .await)
    });

    let steps = match steps {
        Ok(steps) => steps,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    return_json(&mut env, ok_json(offer_reply(&driver, &steps)))
}

/// Streams activity logs to the UI and clears the buffer.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_pollEvents(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let core = match lookup(handle) {
        Ok(core) => core,
        Err(e) => return return_json(&mut env, err_json(e)),
    };
    let drained = std::mem::take(&mut *core.events.lock().expect("events lock"));
    return_json(&mut env, ok_json(serde_json::json!({ "events": drained })))
}

/// Hex overlay geometry: Res 8 zone plan + Res 9 position cells. Pure math, no node needed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_meshtaxi_android_MeshTaxiCore_overlayCells(
    mut env: JNIEnv,
    _class: JClass,
    lat: jdouble,
    lon: jdouble,
    max_zone: jint,
    res9: JDoubleArray,
) -> jstring {
    let driver = match GeoPoint::new(lat, lon) {
        Ok(point) => point,
        Err(e) => return return_json(&mut env, err_json(e)),
    };

    let mut res9_points = Vec::new();
    let res9_len = env.get_array_length(&res9).unwrap_or(0);
    if res9_len > 0 && res9_len % 2 == 0 && res9_len <= 128 {
        let mut buffer = vec![0.0f64; res9_len as usize];
        if env.get_double_array_region(&res9, 0, &mut buffer).is_ok() {
            for pair in buffer.chunks_exact(2) {
                if let Ok(point) = GeoPoint::new(pair[0], pair[1]) {
                    res9_points.push(point.position_cell());
                }
            }
        }
    }

    let mut zone_cells = Vec::new();
    for zone in scan_plan(
        driver.search_cell(),
        (max_zone.max(1) as u32).min(OVERLAY_ZONE_CAP),
    ) {
        for cell in zone.cells() {
            zone_cells.push(serde_json::json!({
                "id": board_label(*cell),
                "res": 8,
                "ring": zone.id(),
                "points": cell_points(*cell),
            }));
        }
    }

    let mut position_cells = Vec::new();
    for cell in res9_points {
        position_cells.push(serde_json::json!({
            "id": cell_label(cell),
            "res": 9,
            "ring": serde_json::Value::Null,
            "points": cell_points(cell),
        }));
    }

    return_json(
        &mut env,
        ok_json(serde_json::json!({
            "zone_cells": zone_cells,
            "position_cells": position_cells
        })),
    )
}

// -- shared copy of the web harness step rendering -------------------------------------

fn cell_points(cell: CellIndex) -> Vec<[f64; 2]> {
    cell.boundary()
        .iter()
        .map(|ll| [ll.lat(), ll.lng()])
        .collect()
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

fn offer_reply(driver: &GeoPoint, steps: &[BargainStep]) -> serde_json::Value {
    let mut transcript = Vec::new();
    let mut details = serde_json::Value::Null;
    for step in steps {
        let (line, step_details) = describe_step(driver, step);
        if let Some(step_details) = step_details {
            details = step_details;
        }
        transcript.push(line);
    }
    serde_json::json!({ "transcript": transcript, "details": details })
}
