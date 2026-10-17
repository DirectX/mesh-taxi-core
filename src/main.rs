use h3o::{CellIndex, LatLng, Resolution};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;
use veilid_core::{CRYPTO_KIND_VLD0, DHTSchema, VeilidConfig, VeilidUpdate, api_startup};

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GeoPoint {
    latitude: f64,
    longitude: f64,
}

// 1. Ride request (hides exact geo coordinates)
#[derive(Serialize, Deserialize, Debug, Clone)]
struct ObfuscatedRideRequest {
    request_id: String,
    passenger_route_id: String, // Secure connection channel
    from_hex_res9: String,      // Approximate start point in Res 9
    to_hex_res9: String,        // Approximate target point in Res 9
    price_offered: u64,
}

// 2. Structure for precise geo exchange AFTER rating validation
#[derive(Serialize, Deserialize, Debug, Clone)]
struct PreciseRouteDetails {
    from_point: GeoPoint,
    to_point: GeoPoint,
    passenger_secret: String,
}

fn taxi_update_handler(_update: VeilidUpdate) {}

fn main_logic_demo() -> (bool, String, ObfuscatedRideRequest) {
    // --- Passenger: pub ---
    let start_latlng = LatLng::new(48.858246f64, 2.294495f64).unwrap();
    // Res 8 index for DHT search
    let search_hex_res8: CellIndex = start_latlng.to_cell(Resolution::Eight);
    // Res 9 index for more precise startingg position inside the offer
    let start_hex_res9: CellIndex = start_latlng.to_cell(Resolution::Nine);

    let target_latlng = LatLng::new(48.860641f64, 2.337618f64).unwrap();
    // Destination point also in Res 9
    let target_hex_res9: CellIndex = target_latlng.to_cell(Resolution::Nine);

    let obfuscated_order = ObfuscatedRideRequest {
        request_id: Uuid::new_v4().to_string(),
        passenger_route_id: "vld_route_abstract_id_abc123".to_string(),
        from_hex_res9: start_hex_res9.to_string(),
        to_hex_res9: target_hex_res9.to_string(),
        price_offered: 25000,
    };

    // String for indexation in DHT:
    let dht_index_string = format!("taxi_v1_res8_{}", search_hex_res8);
    println!(
        "Passenger publishes ride request with hexagone key in Res 8: {}",
        dht_index_string
    );

    // --- Driver: Search in redius ---
    // The driver is slightly outside but within the range
    // let driver_latlng = LatLng::new(48.862875f64, 2.287302f64).unwrap(); // zone 1
    // let driver_latlng = LatLng::new(48.869663f64, 2.285341f64).unwrap(); // zone 2
    let driver_latlng = LatLng::new(48.877989f64, 2.282461f64).unwrap(); // zone 3
    let driver_hex_res8: CellIndex = driver_latlng.to_cell(Resolution::Eight);

    println!("distance: {}", driver_latlng.distance_km(start_latlng));

    // Searching in radius 1 around hexagon (own + 6 neighbours around = 7 hexagons)
    let search_disk = driver_hex_res8.grid_disk::<Vec<_>>(1);

    println!(
        "\nDriver is scanning {} neighbour hexagons with resolution 8:",
        search_disk.len()
    );

    let mut found: bool = false;
    for hex in search_disk {
        let cell_dht_key = format!("taxi_v1_res8_{}", hex);
        // In real application here will be async loop:
        // routing_context.get_dht_value(calculated_from_cell_dht_key, ...).await

        if hex == search_hex_res8 {
            found = true;
            println!(
                "  -> [Found!] In hexagone {} there is active ride request {}",
                hex, obfuscated_order.request_id
            );
            println!(
                "     Route: {} -> {}",
                obfuscated_order.from_hex_res9, obfuscated_order.to_hex_res9
            );
            println!(
                "     Connection over RouteID: {}",
                obfuscated_order.passenger_route_id
            );
        } else {
            println!("  .. Hexagone {} is empty", hex);
        }
    }

    // Ride requests was not found - extending search area
    if found == false {
        let search_ring = driver_hex_res8.grid_ring::<Vec<_>>(2);

        println!(
            "Ride requests was not found in zone 1, getting extra ring of len = {} for zone 2...",
            search_ring.len()
        );

        for hex in search_ring {
            let cell_dht_key = format!("taxi_v1_res8_{}", hex);
            // In real application here will be async loop:
            // routing_context.get_dht_value(calculated_from_cell_dht_key, ...).await

            if hex == search_hex_res8 {
                found = true;
                println!(
                    "  -> [Found!] In hexagone {} there is active ride request {}",
                    hex, obfuscated_order.request_id
                );
                println!(
                    "     Route: {} -> {}",
                    obfuscated_order.from_hex_res9, obfuscated_order.to_hex_res9
                );
                println!(
                    "     Connection over RouteID: {}",
                    obfuscated_order.passenger_route_id
                );
            } else {
                println!("  .. Hexagone {} is empty", hex);
            }
        }
    }

    // Ride requests still was not found - last time extending search area
    if found == false {
        let search_ring = driver_hex_res8.grid_ring::<Vec<_>>(3);

        println!(
            "Ride requests was not found in zone 1 and 2, getting extra ring of len = {} for zone 3...",
            search_ring.len()
        );

        for hex in search_ring {
            let cell_dht_key = format!("taxi_v1_res8_{}", hex);
            // In real application here will be async loop:
            // routing_context.get_dht_value(calculated_from_cell_dht_key, ...).await

            if hex == search_hex_res8 {
                found = true;
                println!(
                    "  -> [Found!] In hexagone {} there is active ride request {}",
                    hex, obfuscated_order.request_id
                );
                println!(
                    "     Route: {} -> {}",
                    obfuscated_order.from_hex_res9, obfuscated_order.to_hex_res9
                );
                println!(
                    "     Connection over RouteID: {}",
                    obfuscated_order.passenger_route_id
                );
            } else {
                println!("  .. Hexagone {} is empty", hex);
            }
        }
    }

    return (found, search_hex_res8.to_string(), obfuscated_order);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting decentralized taxi service node (Veilid v0.5.7)...");

    // 1. Config initialization with p2p database isolation into temp folder
    let mut config = VeilidConfig::default();
    config.program_name = "veilid_taxi_proto".to_string();

    let target_dir = std::env::temp_dir().join("veilid_taxi_proto");
    let target_dir_str = target_dir.to_string_lossy().into_owned();

    let mut protected_store_config = veilid_core::VeilidConfigProtectedStore::default();
    protected_store_config.directory = target_dir_str.clone();
    config.protected_store = protected_store_config;

    let mut table_store_config = veilid_core::VeilidConfigTableStore::default();
    table_store_config.directory = target_dir_str;
    config.table_store = table_store_config;

    // 2. Running API
    let api = api_startup(Arc::new(taxi_update_handler), config).await?;
    api.attach().await?;

    // Cold start timeout
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let (found, client_hexagon_res8, my_order) = main_logic_demo();

    let routing_context = api.routing_context()?;

    // Client's hexagon received from h3o
    let h3_key_str = format!("mesh_taxi_v1_{}", client_hexagon_res8);
    println!("\n[Passenger] Order published to {}...", h3_key_str);

    if found {
        let schema = DHTSchema::default();

        // Creating the record. This is local method emitting RecordKey without network transactions!
        let dht_record = routing_context
            .create_dht_record(CRYPTO_KIND_VLD0, schema, None)
            .await?;
        let typed_key = dht_record.key();
        println!("🔑 Local RecordKey generated for offer: {:?}", typed_key);

        let order_bytes: Vec<u8> = postcard::to_allocvec(&my_order)?;

        // Writing struct to distributed database
        routing_context
            .set_dht_value(typed_key.clone(), 0, order_bytes, None)
            .await?;
        println!("[Passenger] Offer successfully published.");

        // --- 2. DRIVER SIDE: OFFER SEARCH ---
        println!("\n[Driver] Network cell query...");

        // Read data locally (force_refresh = false)
        match routing_context
            .get_dht_value(typed_key.clone(), 0, false)
            .await
        {
            Ok(Some(dht_value)) => {
                let raw_bytes = dht_value.data();

                if let Ok(passenger_order) =
                    postcard::from_bytes::<ObfuscatedRideRequest>(&raw_bytes)
                {
                    println!("🎉 [Driver] Active offer is found in local overlay!");
                    println!(
                        "   From: {}, To: {}",
                        passenger_order.from_hex_res9, passenger_order.to_hex_res9,
                    );
                    println!("   Price: {} satoshi", passenger_order.price_offered);
                    println!(
                        "   Connection route (Route ID): {}",
                        passenger_order.passenger_route_id
                    );
                }
            }
            Ok(None) => println!("[Driver] No active offers in the cell."),
            Err(e) => println!("[Driver] Offer search error: {:?}", e),
        }
    }

    api.shutdown().await;
    println!("\nNode has been gracefully stopped.");

    Ok(())
}
