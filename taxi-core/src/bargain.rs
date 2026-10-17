use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::proto::PreciseRouteDetails;

/// Driver self-reported reputation, exchanged inside an offer.
///
/// The claim is validated against the passenger [`RatingPolicy`]; the
/// reputation ledger on per-rating subkeys (next milestone) will replace the
/// self-reported value with an auditable one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RatingClaim {
    /// Completed rides reported by the driver.
    pub rides: u32,
    /// Reported average rating in milli-units (5000 = 5.00 / 5).
    pub average_milli: u32,
    /// Unix timestamp (seconds) the claim was generated at.
    pub claimed_at: u64,
}

impl RatingClaim {
    /// Claim generated "now", fresh enough for the bargain window.
    pub fn now(rides: u32, average_milli: u32, now_secs: u64) -> Self {
        Self {
            rides,
            average_milli,
            claimed_at: now_secs,
        }
    }
}

/// Passenger-side acceptance rules an offer must pass before the exact
/// [`PreciseRouteDetails`] geo points are revealed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RatingPolicy {
    /// Claim must be fresher than this many seconds (0 disables freshness).
    pub max_claim_age_secs: u64,
    /// Driver must report this many completed rides at minimum.
    pub min_rides: u32,
    /// Average rating floor in milli-units; `0` accepts unknown drivers.
    pub min_average_milli: u32,
    /// Average rating ceiling in milli-units (`10000` disables the sanity cap).
    pub max_average_milli: u32,
}

impl RatingPolicy {
    /// Policy used by the CLI defaults: fresh claims only, no trust floor yet
    /// until the reputation ledger exists.
    pub fn open_network() -> Self {
        Self {
            max_claim_age_secs: 600,
            min_rides: 0,
            min_average_milli: 0,
            max_average_milli: 10000,
        }
    }

    /// Validates the freshness of the claim against the current time.
    pub fn check_claim(&self, claim: &RatingClaim, now_secs: u64) -> CoreResult<()> {
        if claim.rides < self.min_rides {
            return Err(CoreError::InvalidCoordinates(format!(
                "driver claims {} rides, policy requires at least {}",
                claim.rides, self.min_rides
            )));
        }
        if claim.average_milli < self.min_average_milli {
            return Err(CoreError::InvalidCoordinates(format!(
                "driver rating {} milli-cents below the policy floor {}",
                claim.average_milli, self.min_average_milli
            )));
        }
        if claim.average_milli > self.max_average_milli {
            return Err(CoreError::InvalidCoordinates(format!(
                "driver rating {} ms out of the sanity cap {}",
                claim.average_milli, self.max_average_milli
            )));
        }
        if self.max_claim_age_secs > 0 && claim.claimed_at + self.max_claim_age_secs < now_secs {
            return Err(CoreError::InvalidCoordinates(format!(
                "rating claim is stale: claimed_at {}, now {}",
                claim.claimed_at, now_secs
            )));
        }
        Ok(())
    }
}

/// Stage 1: driver offer sent as an `app_call` over the imported private
/// route of a published [`ObfuscatedRideRequest`](crate::proto::ObfuscatedRideRequest).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DriverOffer {
    pub request_id: String,
    /// Driver counter-price for the ride (starts at the offered price).
    pub counter_price: u64,
    pub rating: RatingClaim,
}

/// Stage 2: passenger reaction to the offer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BargainCounter {
    pub request_id: String,
    /// The lowest price the passenger settles for.
    pub price: u64,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BargainAccept {
    pub request_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BargainDecline {
    pub request_id: String,
    pub reason: String,
}

/// Stage 3: driver asks for the exact route after agreeing on a price.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DetailsRequest {
    pub request_id: String,
}

/// 4: passenger reply stage: exact geo points of the ride, now that the
/// driver passed rating validation and the price is settled.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum BargainMessage {
    Offer(DriverOffer),
    Counter(BargainCounter),
    Accept(BargainAccept),
    Decline(BargainDecline),
    DetailsRequest(DetailsRequest),
    Details(PreciseRouteDetails),
}

impl BargainMessage {
    pub fn encode(&self) -> CoreResult<Vec<u8>> {
        postcard::to_allocvec(self).map_err(|e| CoreError::Encoding(e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> CoreResult<Self> {
        postcard::from_bytes(bytes).map_err(|e| CoreError::Encoding(e.to_string()))
    }
}

/// Pure passenger decision for a stage-1 offer, used by the passenger node
/// and unit tests. Rating validation gates stage 2; the price floor gates
/// `Accept` vs `Counter`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum BargainDecision {
    /// Price is at or above the floor and the rating passed.
    Accept,
    /// Rating passed but the price is below the passenger floor.
    Counter { floor: u64 },
    /// Rating validation failed.
    Decline { reason: String },
}

pub fn decide_offer(
    floor: u64,
    counter_price: u64,
    claim: &RatingClaim,
    policy: &RatingPolicy,
    now_secs: u64,
) -> BargainDecision {
    if let Err(e) = policy.check_claim(claim, now_secs) {
        return BargainDecision::Decline {
            reason: e.to_string(),
        };
    }
    if counter_price >= floor {
        BargainDecision::Accept
    } else {
        BargainDecision::Counter { floor }
    }
}
