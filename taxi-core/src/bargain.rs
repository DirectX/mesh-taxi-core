use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use veilid_core::{RoutingContext, Target, VeilidAPI};

use crate::error::{CoreError, CoreResult};
use crate::geo::GeoPoint;
use crate::proto::{ObfuscatedRideRequest, PreciseRouteDetails};

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

/// Reusable passenger-side bargain service over [`BargainMessage`] traffic:
/// the CLI passenger node and the web test GUI both route inbound app calls
/// through this engine so the handshake rules exist in exactly one place.
pub struct PassengerBargain {
    from_point: GeoPoint,
    to_point: GeoPoint,
    floor: u64,
    policy: RatingPolicy,
    bargains: HashMap<String, BargainState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BargainState {
    rating_validated: bool,
}

/// The reply a passenger must send back to a driver, with log lines for the
/// host application's transcript.
#[derive(Debug, Clone)]
pub struct PassengerReply {
    pub message: BargainMessage,
    pub logs: Vec<String>,
}

impl PassengerBargain {
    pub fn new(from_point: GeoPoint, to_point: GeoPoint, floor: u64, policy: RatingPolicy) -> Self {
        Self {
            from_point,
            to_point,
            floor,
            policy,
            bargains: HashMap::new(),
        }
    }

    /// Updates the exact points served by later `Details` replies.
    pub fn set_points(&mut self, from_point: GeoPoint, to_point: GeoPoint) {
        self.from_point = from_point;
        self.to_point = to_point;
    }

    /// Drops per-driver bargain state (fresh run of the app).
    pub fn reset(&mut self) {
        self.bargains.clear();
    }

    pub fn validated(&self, request_id: &str) -> bool {
        self.bargains
            .get(request_id)
            .is_some_and(|state| state.rating_validated)
    }

    /// Processes one inbound [`BargainMessage`]; `None` when no reply is
    /// needed (unexpected driver replies are just logged).
    pub fn handle(&mut self, message: BargainMessage, now_secs: u64) -> Option<PassengerReply> {
        match message {
            BargainMessage::Offer(offer) => {
                let decision = decide_offer(
                    self.floor,
                    offer.counter_price,
                    &offer.rating,
                    &self.policy,
                    now_secs,
                );
                let state = match &decision {
                    BargainDecision::Accept | BargainDecision::Counter { .. } => BargainState {
                        rating_validated: true,
                    },
                    BargainDecision::Decline { .. } => BargainState {
                        rating_validated: false,
                    },
                };
                self.bargains
                    .entry(offer.request_id.clone())
                    .or_insert(state);

                let logs = vec![match &decision {
                    BargainDecision::Accept => format!(
                        "passenger: offer accepted at price {} (rating validated)",
                        offer.counter_price
                    ),
                    BargainDecision::Counter { floor } => format!(
                        "passenger: driver rating ok, price {} below floor {}",
                        offer.counter_price, floor
                    ),
                    BargainDecision::Decline { reason } => {
                        format!("passenger: offer declined ({})", reason)
                    }
                }];

                let reply = match decision {
                    BargainDecision::Accept => BargainMessage::Accept(BargainAccept {
                        request_id: offer.request_id.clone(),
                    }),
                    BargainDecision::Counter { floor } => BargainMessage::Counter(BargainCounter {
                        request_id: offer.request_id.clone(),
                        price: floor,
                        reason: "my price floor".into(),
                    }),
                    BargainDecision::Decline { reason } => {
                        BargainMessage::Decline(BargainDecline {
                            request_id: offer.request_id.clone(),
                            reason,
                        })
                    }
                };
                Some(PassengerReply {
                    message: reply,
                    logs,
                })
            }
            BargainMessage::DetailsRequest(DetailsRequest { request_id }) => {
                if !self.validated(&request_id) {
                    return Some(PassengerReply {
                        message: BargainMessage::Decline(BargainDecline {
                            request_id,
                            reason: "no accepted offer".into(),
                        }),
                        logs: vec![
                            "passenger: details request without a validated offer, declined".into(),
                        ],
                    });
                }
                Some(PassengerReply {
                    message: BargainMessage::Details(PreciseRouteDetails {
                        from_point: self.from_point.clone(),
                        to_point: self.to_point.clone(),
                    }),
                    logs: vec![format!(
                        "passenger: PreciseRouteDetails revealed to a validated driver for {}",
                        request_id
                    )],
                })
            }
            BargainMessage::Accept(_) => Some(PassengerReply {
                message: BargainMessage::Decline(BargainDecline {
                    request_id: String::new(),
                    reason: "unexpected accept from a driver".into(),
                }),
                logs: vec!["passenger: unexpected accept from a driver, ignored".into()],
            }),
            BargainMessage::Counter(counter) => Some(PassengerReply {
                message: BargainMessage::Decline(BargainDecline {
                    request_id: String::new(),
                    reason: "unexpected counter from a driver".into(),
                }),
                logs: vec![format!(
                    "passenger: unexpected counter {} from a driver, ignored",
                    counter.price
                )],
            }),
            BargainMessage::Decline(_) => None,
            BargainMessage::Details(_) => None,
        }
    }
}

/// One completed move of the driver-to-passenger `app_call` chain, suitable
/// as a transcript entry for the CLI nodes and the web test GUI.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum BargainStep {
    OfferSent(DriverOffer),
    AcceptReceived(BargainAccept),
    CounterReceived(BargainCounter),
    DeclineReceived(BargainDecline),
    DetailsRequestSent(DetailsRequest),
    DetailsReceived(PreciseRouteDetails),
    CallFailed(String),
}

impl BargainStep {
    /// Short label used by all loggers.
    pub fn label(&self) -> &str {
        match self {
            BargainStep::OfferSent(_) => "OfferSent",
            BargainStep::AcceptReceived(_) => "AcceptReceived",
            BargainStep::CounterReceived(_) => "CounterReceived",
            BargainStep::DeclineReceived(_) => "DeclineReceived",
            BargainStep::DetailsRequestSent(_) => "DetailsRequestSent",
            BargainStep::DetailsReceived(_) => "DetailsReceived",
            BargainStep::CallFailed(reason) => reason,
        }
    }

    /// The exchanged exact points, `Some` only on a successful bargain.
    pub fn details(&self) -> Option<&PreciseRouteDetails> {
        match self {
            BargainStep::DetailsReceived(details) => Some(details),
            _ => None,
        }
    }
}
/// Full driver-side bargain chain over the imported route blob of
/// `request`: offer over `app_call`, then the details request once the
/// passenger accepted (or countered satisfactorily). Every completed move
/// is appended to the returned transcript, ordered chronologically.
pub async fn bargain_request(
    api: &VeilidAPI,
    routing: &RoutingContext,
    request: &ObfuscatedRideRequest,
    counter_price: Option<u64>,
    rating: RatingClaim,
    min_price: u64,
) -> Vec<BargainStep> {
    let mut steps = Vec::new();
    let offer = DriverOffer {
        request_id: request.request_id.clone(),
        counter_price: counter_price.unwrap_or(request.price_offered),
        rating,
    };

    let route_id = match api.import_remote_private_route(request.route_blob.clone()) {
        Ok(route_id) => route_id,
        Err(e) => {
            steps.push(BargainStep::CallFailed(format!(
                "driver: route import failed: {}",
                e
            )));
            return steps;
        }
    };

    steps.push(BargainStep::OfferSent(offer.clone()));
    let offer_sent = BargainMessage::Offer(offer);
    let staged = match send_call(routing, &route_id, &offer_sent).await {
        Ok(reply) => reply,
        Err(e) => {
            steps.push(BargainStep::CallFailed(format!(
                "driver: offer call failed: {}",
                e
            )));
            release_route(api, &route_id);
            return steps;
        }
    };

    match staged {
        BargainMessage::Accept(accept) => {
            steps.push(BargainStep::AcceptReceived(accept.clone()));
            push_details_exchange(routing, &route_id, request, &mut steps).await;
        }
        BargainMessage::Counter(counter) => {
            steps.push(BargainStep::CounterReceived(counter.clone()));
            if counter.price >= min_price {
                push_details_exchange(routing, &route_id, request, &mut steps).await;
            } else {
                let decline = BargainMessage::Decline(BargainDecline {
                    request_id: request.request_id.clone(),
                    reason: "counter below driver floor".into(),
                });
                if let Err(e) = send_call(routing, &route_id, &decline).await {
                    steps.push(BargainStep::CallFailed(format!(
                        "driver: decline call failed: {}",
                        e
                    )));
                }
                steps.push(BargainStep::CallFailed(format!(
                    "driver: counter {} below driver floor {}, declined",
                    counter.price, min_price
                )));
            }
        }
        BargainMessage::Decline(decline) => {
            steps.push(BargainStep::DeclineReceived(decline.clone()));
            steps.push(BargainStep::CallFailed(format!(
                "driver: passenger declined: {}",
                decline.reason
            )));
        }
        other => {
            steps.push(BargainStep::CallFailed(format!(
                "driver: unexpected reply {}",
                other.label()
            )));
        }
    }

    release_route(api, &route_id);
    steps
}

impl BargainMessage {
    /// Short label for the transcript of unexpected traffic.
    pub fn label(&self) -> &'static str {
        match self {
            BargainMessage::Offer(_) => "Offer",
            BargainMessage::Counter(_) => "Counter",
            BargainMessage::Accept(_) => "Accept",
            BargainMessage::Decline(_) => "Decline",
            BargainMessage::DetailsRequest(_) => "DetailsRequest",
            BargainMessage::Details(_) => "Details",
        }
    }
}

async fn push_details_exchange(
    routing: &RoutingContext,
    route_id: &veilid_core::RouteId,
    request: &ObfuscatedRideRequest,
    steps: &mut Vec<BargainStep>,
) {
    let ask = BargainMessage::DetailsRequest(DetailsRequest {
        request_id: request.request_id.clone(),
    });
    steps.push(BargainStep::DetailsRequestSent(DetailsRequest {
        request_id: request.request_id.clone(),
    }));

    match send_call(routing, route_id, &ask).await {
        Ok(BargainMessage::Details(details)) => {
            steps.push(BargainStep::DetailsReceived(details));
        }
        Ok(BargainMessage::Decline(decline)) => {
            steps.push(BargainStep::DeclineReceived(decline.clone()));
            steps.push(BargainStep::CallFailed(format!(
                "driver: details declined: {}",
                decline.reason
            )));
        }
        Ok(other) => steps.push(BargainStep::CallFailed(format!(
            "driver: unexpected details reply {}",
            other.label()
        ))),
        Err(e) => steps.push(BargainStep::CallFailed(format!(
            "driver: details call failed: {}",
            e
        ))),
    }
}

async fn send_call(
    routing: &RoutingContext,
    route_id: &veilid_core::RouteId,
    message: &BargainMessage,
) -> CoreResult<BargainMessage> {
    let bytes = routing
        .app_call(Target::RouteId(route_id.clone()), message.encode()?)
        .await?;
    BargainMessage::decode(&bytes)
}

fn release_route(api: &VeilidAPI, route_id: &veilid_core::RouteId) {
    if let Err(e) = api.release_private_route(route_id.clone()) {
        eprintln!("driver: route release failed: {}", e);
    }
}
