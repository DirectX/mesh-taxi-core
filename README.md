# Decentralized p2p Taxi Aggregator

A geo-mesh taxi aggregator built on [Veilid](https://veilid.com) (p2p DHT + E2E
routes) and [h3o](https://crates.io/crates/h3o) (Uber H3 hexagons). Passenger
positions are never published in plaintext: the public network only ever sees
hexagon indices and Veilid opaque record keys.

## Workspace layout

| Crate            | Content                                                                 |
| ---------------- | ----------------------------------------------------------------------- |
| `taxi-core`      | Geo core: H3 cells, obfuscated wire messages, DHT boards, zone search, Veilid node helper, bargain handshake |
| `taxi-passenger` | Bin: creates an E2E private route and publishes an obfuscated ride request |
| `taxi-driver`    | Bin: scans H3 zones around its position, finds ride requests and bargains over their route blobs |
| `taxi-web`       | Axum harness + OpenStreetMap (Leaflet) GUI for clickable end-to-end testing on one machine |

## How the geo core works

1. **Resolution split** (`taxi-core/src/geo.rs`)
   - `Resolution::Eight` (~460 m cells) is the **board grid**: the pickup hexagon
     addresses the DHT record that holds requests for a neighbourhood.
   - `Resolution::Nine` (~174 m cells) is the **position grid**: exact
     coordinates stay on the client, the network sees only Res-9 indices.

2. **Deterministic boards** (`taxi-core/src/board.rs`)
   Every node derives the same board keypair and record key from a Res-8
   cell index (`sha256` seed -> ed25519 keypair, blake3 record key), so boards
   exist without pre-provisioning. Boards are shared aggregators; ride requests
   carry their own E2E privacy via the private route blob.

3. **Obfuscated wire format** (`taxi-core/src/proto.rs`, postcard-encoded)
   `ObfuscatedRideRequest { request_id, route_blob, from/to Res-9 cells,
   price_offered, expires_at }` — one request occupies one subkey on the
   board. `PreciseRouteDetails` exchanges exact points only after a driver is
   accepted, over the imported private route.

4. **Zone search** (`taxi-core/src/search.rs`)
   The driver expands ring-by-ring around its own cell: zone 1 is the own
   hexagon + 6 neighbours (`grid_disk(1)`), then rings 2..=k. Each discovered
   cell maps to a board record; `inspect_dht_record(SyncGet)` lists occupied
   subkeys and each value is fetched, decoded and checked against `expires_at`.

5. **Bargain handshake** (`taxi-core/src/bargain.rs`)
   After discovering a request the driver imports its `route_blob`
   (`VeilidAPI::import_remote_private_route`) and runs the RPC chain:

   1. `app_call(Offer)` over the imported route: `DriverOffer
      { request_id, counter_price, rating: RatingClaim }`.
   2. The passenger applies a `RatingPolicy` (claim freshness, minimum
      rides, rating floor) — only validated offers may continue.
   3. Passenger reply: `Accept`, `Counter { price }` (counter still opens
      the rating gate) or `Decline { reason }`.
   4. Driver (having agreed to the counter) sends `app_call(DetailsRequest)`
      and receives `PreciseRouteDetails { from_point, to_point }` — the exact
      pickup/target coordinates, only revealed past rating validation.

## Running the demo workflow (live Veilid network)

```bash
# Terminal 1: passenger at the Eiffel tower
cargo run -p taxi-passenger -- \
    --from-lat 48.858246 --from-lon 2.294495 \
    --to-lat 48.860641 --to-lon 2.337618 \
    --price 25000

# Terminal 2: driver ~2.5 km away (found in zone 3); offers over the
# imported route blob, bargain, then fetches PreciseRouteDetails
cargo run -p taxi-driver -- \
    --lat 48.877989 --lon 2.282461 \
    --max-zone 3 --scan-interval 20 \
    --offer --rides 40 --average-rating 4600
```

The driver prints found requests with the deobfuscated distance to the pickup
cell, bargains with `DriverOffer` over the imported route blob and, once the
passenger's rating validation passes, prints the exact `PreciseRouteDetails`
pickup/target coordinates. The passenger stays online serving
`app_call`s (offer -> accept/counter/decline -> details). Ctrl-C stops either
node.

## Clickable testing with the OSM GUI

The `taxi-web` crate runs the same workflow on one machine: it starts the
passenger and the driver as two in-process Veilid nodes and serves a
Leaflet/OpenStreetMap UI where you click pickup and target, publish the ride
request, scan zones as the driver and run the bargain until the precise
route appears on the map.

```bash
cargo run -p taxi-web             # then open http://localhost:8088
PORT=9000 cargo run -p taxi-web   # alternate port
```

## Tests

```bash
cargo test --workspace
```

20 unit tests cover resolution mapping, key/subkey derivation, codec
roundtrips (requests, bargain messages, precise details), zone geometry (no
overlap, zone-3 reach), the demo distances and the rating/bargain decisions.

## Status / next steps

- [x] Geo core (this release)
- [x] Driver -> passenger `app_call` over the imported route blob
- [x] Offer/bargain protocol and `PreciseRouteDetails` exchange after rating validation
- [ ] Reputation ledger on per-rating subkeys (beyond the self-reported `RatingClaim`)
- [x] OSM web GUI for one-machine end-to-end testing (`taxi-web`)
- [ ] Proper signature mobile PWA
