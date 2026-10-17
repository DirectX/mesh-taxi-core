//! Geo core for the decentralized p2p taxi aggregator.
//!
//! The core implements the private-by-default geo mesh network workflow:
//!
//! 1. **Obfuscated discovery** — passenger positions are published as H3
//!    Res-9 hexagons on a "board" DHT record keyed by the Res-8 hexagon of
//!    the departure point. Exact coordinates never hit the public network.
//! 2. **Zone search** — drivers expand circles around their own Res-8 cell
//!    (own hexagon + 6 neighbours, then rings 2..=k) and query the board
//!    record of every discovered cell.
//! 3. **Private connection** — a ride request carries a Veilid private route
//!    blob; drivers import it and negotiate precise `PreciseRouteDetails`
//!    end-to-end, only after the rating handshake.

pub mod bargain;
pub mod board;
pub mod error;
pub mod geo;
pub mod node;
pub mod proto;
pub mod search;

pub use bargain::{
    BargainAccept, BargainCounter, BargainDecision, BargainDecline, BargainMessage, DetailsRequest,
    DriverOffer, RatingClaim, RatingPolicy, decide_offer,
};
pub use board::{
    BOARD_SCHEMA_O_CNT, board_owner_keypair, board_record_key, board_schema, subkey_for_request,
    validate_subkey,
};
pub use error::{CoreError, CoreResult};
pub use geo::{GeoPoint, POSITION_RESOLUTION, SEARCH_RESOLUTION, board_label};
pub use node::{Node, NodeConfig};
pub use proto::{ObfuscatedRideRequest, PreciseRouteDetails};
pub use search::{MAX_ZONE, Zone, all_cells, scan_plan, zone_cells, zone_of};

#[cfg(test)]
mod tests;
