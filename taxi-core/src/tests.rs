use super::*;

const TEST_CELL_RES9: &str = "88283082abfffff";
// Passenger demo points (Paris, near the Eiffel tower)
const PICKUP: (f64, f64) = (48.858246, 2.294495);
const TARGET: (f64, f64) = (48.860641, 2.337618);
// Driver zones from the demo, in increasing distance from the pickup
const DRIVER_ZONE_1: (f64, f64) = (48.862875, 2.287302);
const DRIVER_ZONE_3: (f64, f64) = (48.877989, 2.282461);

fn res9(lat: f64, lng: f64) -> h3o::CellIndex {
    h3o::LatLng::new(lat, lng)
        .unwrap()
        .to_cell(geo::POSITION_RESOLUTION)
}

fn res8(lat: f64, lng: f64) -> h3o::CellIndex {
    h3o::LatLng::new(lat, lng)
        .unwrap()
        .to_cell(geo::SEARCH_RESOLUTION)
}

#[test]
fn geo_point_resolution_split() {
    let point = geo::GeoPoint::new(PICKUP.0, PICKUP.1).unwrap();

    assert_eq!(point.search_cell(), res8(PICKUP.0, PICKUP.1));
    assert_eq!(point.position_cell(), res9(PICKUP.0, PICKUP.1));
}

#[test]
fn geo_point_rejects_invalid() {
    assert!(geo::GeoPoint::new(91.0, 0.0).is_err());
    assert!(geo::GeoPoint::new(0.0, 190.0).is_err());
}

#[test]
fn geo_roundtrip_from_cell_center() {
    let point = geo::GeoPoint::new(PICKUP.0, PICKUP.1);
    let cell = point.unwrap().position_cell();
    let center = geo::GeoPoint::from_cell(cell);
    let cell_again = center.position_cell();
    assert_eq!(cell, cell_again);
}

#[test]
fn board_label_formats_prefix() {
    let cell = res8(PICKUP.0, PICKUP.1);
    let label = geo::board_label(cell);
    assert!(label.starts_with(geo::BOARD_LABEL_PREFIX));
    assert!(label.contains(&cell.to_string()));
}

#[test]
fn derive_board_keypair_is_deterministic() {
    let hex = TEST_CELL_RES9.parse::<h3o::CellIndex>().unwrap();

    let kp1 = board::board_owner_keypair(hex);
    let kp2 = board::board_owner_keypair(hex);

    assert_eq!(kp1.key(), kp2.key(), "same hex must derive same public key");
    assert_eq!(
        kp1.bare_secret(),
        kp2.bare_secret(),
        "same hex must derive same secret"
    );
}

#[test]
fn derive_board_keypair_differs_between_cells() {
    let cell_a = res9(PICKUP.0, PICKUP.1);
    let cell_b = res9(TARGET.0, TARGET.1);
    assert_ne!(cell_a, cell_b);

    let kp_a = board::board_owner_keypair(cell_a);
    let kp_b = board::board_owner_keypair(cell_b);

    assert_ne!(kp_a.key(), kp_b.key());
}

#[test]
fn board_record_key_is_stable_and_covers_board_secret() {
    let hex = res8(PICKUP.0, PICKUP.1);

    let key1 = board::board_record_key(hex);
    let key2 = board::board_record_key(hex);
    let key_other = board::board_record_key(res8(TARGET.0, TARGET.1));

    assert_eq!(key1, key2, "record key must be derived deterministically");
    assert_eq!(key1.kind(), veilid_core::CRYPTO_KIND_VLD0);
    assert_ne!(key1, key_other, "different cells must map to different boards");
}

#[test]
fn subkey_fits_board_schema() {
    let subkey = board::subkey_for_request("11111111-2222-3333-4444-555555555555");
    board::validate_subkey(subkey).unwrap();

    assert_eq!(
        subkey,
        board::subkey_for_request("11111111-2222-3333-4444-555555555555")
    );
    assert_ne!(
        subkey,
        board::subkey_for_request("99999999-2222-3333-4444-555555555555")
    );
}

#[test]
fn ride_request_codec_roundtrip() {
    let pickup = geo::GeoPoint::new(PICKUP.0, PICKUP.1).unwrap();
    let target = geo::GeoPoint::new(TARGET.0, TARGET.1).unwrap();

    let request = proto::ObfuscatedRideRequest {
        request_id: "demo-request".to_string(),
        route_blob: vec![0x01, 0x02, 0x03],
        from_cell_res9: pickup.position_cell().into(),
        to_cell_res9: target.position_cell().into(),
        price_offered: 25000,
        expires_at: 1700000000,
    };

    let bytes = request.encode().unwrap();
    let decoded = proto::ObfuscatedRideRequest::decode(&bytes).unwrap();

    assert_eq!(decoded.request_id, "demo-request");
    assert_eq!(decoded.route_blob, vec![0x01, 0x02, 0x03]);
    assert_eq!(decoded.from_cell(), pickup.position_cell());
    assert_eq!(decoded.to_cell(), target.position_cell());
    assert_eq!(decoded.price_offered, 25000);
}

#[test]
fn zone1_contains_own_cell_and_six_neighbours() {
    let center = res8(PICKUP.0, PICKUP.1);
    let zone = search::zone_cells(center, 1);

    assert_eq!(zone.id(), 1);
    assert_eq!(zone.cells().len(), 7);
    assert!(zone.cells().contains(&center));
}

#[test]
fn zones_do_not_overlap() {
    let center = res8(PICKUP.0, PICKUP.1);

    let zone1: Vec<_> = search::zone_cells(center, 1).cells().to_vec();
    let zone2: Vec<_> = search::zone_cells(center, 2).cells().to_vec();
    let zone3: Vec<_> = search::zone_cells(center, 3).cells().to_vec();

    assert!(!zone2.iter().any(|c| zone1.contains(c)));
    assert!(!zone3.iter().any(|c| zone1.contains(c) || zone2.contains(c)));
}

#[test]
fn zone3_reaches_far_driver_cell() {
    let pickup_cell = res8(PICKUP.0, PICKUP.1);

    let driver_zone1 = res8(DRIVER_ZONE_1.0, DRIVER_ZONE_1.1);
    let driver_zone3 = res8(DRIVER_ZONE_3.0, DRIVER_ZONE_3.1);

    assert_eq!(
        search::zone_of(driver_zone1, pickup_cell, search::MAX_ZONE),
        Some(1),
        "zone 1 driver discovers pickup directly"
    );
    assert_eq!(
        search::zone_of(driver_zone3, pickup_cell, search::MAX_ZONE),
        Some(3),
        "zone 3 driver discovers pickup only in the third ring"
    );
    assert_eq!(
        search::zone_of(driver_zone3, res8(10.0, 10.0), search::MAX_ZONE),
        None,
        "unrelated cell is unreached"
    );
}

#[test]
fn scan_plan_covers_all_zones() {
    let center = res8(PICKUP.0, PICKUP.1);
    let plan = search::scan_plan(center, search::MAX_ZONE);

    assert_eq!(plan.len(), 3);
    assert_eq!(search::all_cells(center, search::MAX_ZONE).len(), 7 + 12 + 18);
}

#[test]
fn distance_matches_demo() {
    let pickup = geo::GeoPoint::new(PICKUP.0, PICKUP.1).unwrap();
    let driver = geo::GeoPoint::new(DRIVER_ZONE_3.0, DRIVER_ZONE_3.1).unwrap();

    let dist = pickup.distance_km(&driver);
    assert!(dist > 1.0 && dist < 4.0, "distance was {}", dist);
}
