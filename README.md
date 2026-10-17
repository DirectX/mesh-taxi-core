# Decentralized p2p Taxi Aggregator

A geo-mesh taxi aggregator built on [Veilid](https://veilid.com) (p2p DHT + E2E
routes) and [h3o](https://crates.io/crates/h3o) (Uber H3 hexagons). Passenger
positions are never published in plaintext: the public network only ever sees
hexagon indices and Veilid opaque record keys.

## Workspace layout

| Crate            | Content                                                                 |
| ---------------- | ----------------------------------------------------------------------- |
| `taxi-core`      | Geo core: H3 cells, obfuscated wire messages, DHT boards, zone search, Veilid node helper |
| `taxi-passenger` | Bin: creates an E2E private route and publishes an obfuscated ride request |
| `taxi-driver`    | Bin: scans H3 zones around its position and finds ride requests          |

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

## Running the demo workflow (live Veilid network)

```bash
# Terminal 1: passenger at the Eiffel tower
cargo run -p taxi-passenger -- \
    --from-lat 48.858246 --from-lon 2.294495 \
    --to-lat 48.860641 --to-lon 2.337618 \
    --price 25000

# Terminal 2: driver ~2.5 km away (found in zone 3)
cargo run -p taxi-driver -- \
    --lat 48.877989 --lon 2.282461 \
    --max-zone 3 --scan-interval 20
```

The driver prints found requests with the deobfuscated distance to the pickup
cell; the passenger's route blob inside the request is the connection channel
for the precise geo exchange. Ctrl-C stops either node.

## Tests

```bash
cargo test --workspace
```

14 unit tests cover resolution mapping, key/subkey derivation, codec
roundtrips, zone geometry (no overlap, zone-3 reach) and the demo distances.

## Status / next steps

- [x] Geo core (this release)
- [ ] Driver -> passenger `app_call` over the imported route blob
- [ ] Offer/bargain protocol and `PreciseRouteDetails` exchange after rating validation
- [ ] Reputation ledger on per-rating subkeys
- [ ] Proper signature mobile PWA
