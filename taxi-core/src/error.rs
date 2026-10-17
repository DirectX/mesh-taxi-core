use thiserror::Error;

/// Core error type shared by the geo core and the passenger/driver nodes.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid coordinates: {0}")]
    InvalidCoordinates(String),
    #[error("invalid h3 cell index: {0}")]
    InvalidCellIndex(String),
    #[error("message encoding error: {0}")]
    Encoding(String),
    #[error("veilid api error: {0:?}")]
    Veilid(veilid_core::VeilidAPIError),
    #[error("veilid node did not become attached within {0:?}")]
    Timeout(std::time::Duration),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<h3o::error::InvalidLatLng> for CoreError {
    fn from(value: h3o::error::InvalidLatLng) -> Self {
        CoreError::InvalidCoordinates(value.to_string())
    }
}

impl From<h3o::error::InvalidCellIndex> for CoreError {
    fn from(value: h3o::error::InvalidCellIndex) -> Self {
        CoreError::InvalidCellIndex(value.to_string())
    }
}

impl From<veilid_core::VeilidAPIError> for CoreError {
    fn from(value: veilid_core::VeilidAPIError) -> Self {
        CoreError::Veilid(value)
    }
}

pub type CoreResult<T> = Result<T, CoreError>;
