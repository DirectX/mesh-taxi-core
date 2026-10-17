use h3o::CellIndex;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::geo::GeoPoint;

/// 1: ride request published on the H3 board without exact geo coordinates.
/// The exact points stay with the passenger; the board only shows Res 9 cells.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ObfuscatedRideRequest {
    pub request_id: String,
    /// Private route blob from `VeilidAPI::new_private_route()`.
    /// The driver imports it to reach the passenger E2E without a tracker.
    pub route_blob: Vec<u8>,
    /// Approximate start point (Res 9 cell).
    pub from_cell_res9: u64,
    /// Approximate target point (Res 9 cell).
    pub to_cell_res9: u64,
    pub price_offered: u64,
    /// Unix timestamp (seconds) after which the request is stale.
    pub expires_at: u64,
}

impl ObfuscatedRideRequest {
    pub fn from_cell(&self) -> CellIndex {
        CellIndex::try_from(self.from_cell_res9).expect("cell index was validated on publish")
    }

    pub fn to_cell(&self) -> CellIndex {
        CellIndex::try_from(self.to_cell_res9).expect("cell index was validated on publish")
    }

    pub fn from_point(&self) -> GeoPoint {
        GeoPoint::from_cell(self.from_cell())
    }

    pub fn to_point(&self) -> GeoPoint {
        GeoPoint::from_cell(self.to_cell())
    }

    pub fn encode(&self) -> CoreResult<Vec<u8>> {
        postcard::to_allocvec(self).map_err(|e| CoreError::Encoding(e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> CoreResult<Self> {
        postcard::from_bytes(bytes).map_err(|e| CoreError::Encoding(e.to_string()))
    }
}

/// 2: exact geo exchange AFTER a driver offer is accepted and validated.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PreciseRouteDetails {
    pub from_point: GeoPoint,
    pub to_point: GeoPoint,
}

impl PreciseRouteDetails {
    pub fn encode(&self) -> CoreResult<Vec<u8>> {
        postcard::to_allocvec(self).map_err(|e| CoreError::Encoding(e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> CoreResult<Self> {
        postcard::from_bytes(bytes).map_err(|e| CoreError::Encoding(e.to_string()))
    }
}
