use h3o::{CellIndex, LatLng, Resolution};
use serde::{Deserialize, Serialize};

use crate::error::CoreResult;

/// Hexagon resolution used for the "board" DHT grid (cell search / aggregation).
/// Res 8 cells are ~460 m across, giving a ~500 m neighbourhood of one DHT record.
pub const SEARCH_RESOLUTION: Resolution = Resolution::Eight;
/// Hexagon resolution used to exchange positions inside a ride request.
/// Res 9 cells are ~174 m across, which keeps the exact coordinates private.
pub const POSITION_RESOLUTION: Resolution = Resolution::Nine;

/// Human-readable key prefix for logging board cells.
pub const BOARD_LABEL_PREFIX: &str = "taxi_v1_res8_";

/// An exact geographic point, kept private by the owner.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct GeoPoint {
    pub latitude: f64,
    pub longitude: f64,
}

impl GeoPoint {
    /// Validates ranges and finiteness, then builds the point.
    /// Errors with [`CoreError::InvalidCoordinates`] if the coordinates are not valid.
    pub fn new(latitude: f64, longitude: f64) -> CoreResult<Self> {
        if !(-90.0..=90.0).contains(&latitude) {
            return Err(crate::error::CoreError::InvalidCoordinates(format!(
                "latitude out of range: {latitude}"
            )));
        }
        if !(-180.0..=180.0).contains(&longitude) {
            return Err(crate::error::CoreError::InvalidCoordinates(format!(
                "longitude out of range: {longitude}"
            )));
        }
        LatLng::new(latitude, longitude)?;
        Ok(Self {
            latitude,
            longitude,
        })
    }

    pub fn lat(&self) -> LatLng {
        LatLng::new(self.latitude, self.longitude)
            .expect("coordinates validated in GeoPoint::new")
    }

    /// Res 8 cell whose DHT board this point publishes into.
    pub fn search_cell(&self) -> CellIndex {
        self.lat().to_cell(SEARCH_RESOLUTION)
    }

    /// Res 9 cell for the obfuscated position published inside a ride request.
    pub fn position_cell(&self) -> CellIndex {
        self.lat().to_cell(POSITION_RESOLUTION)
    }

    /// Great-circle distance to another point, in kilometres.
    pub fn distance_km(&self, other: &GeoPoint) -> f64 {
        self.lat().distance_km(other.lat())
    }

    /// The point at the center of an H3 cell.
    pub fn from_cell(cell: CellIndex) -> Self {
        let lat = LatLng::from(cell);
        Self {
            latitude: lat.lat(),
            longitude: lat.lng(),
        }
    }
}

/// Logs-friendly label for a board cell: `taxi_v1_res8_88283082abfffff`.
pub fn board_label(cell: CellIndex) -> String {
    format!("{}{}", BOARD_LABEL_PREFIX, cell)
}
