use h3o::CellIndex;
use veilid_core::{DHTReportScope, RoutingContext};

use crate::board::board_record_key;
use crate::geo::board_label;
use crate::proto::ObfuscatedRideRequest;

/// Default search reach for the driver, in H3 grid rings around the own cell.
/// Matches the demo zones: ring 1 ~ own cell + 6 neighbours, ring 2, ring 3.
pub const MAX_ZONE: u32 = 3;

/// One zone of the growing search strategy of the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// Zone id; 1 is the own cell plus its 6 neighbours, then rings 2..=k.
    id: u32,
    cells: Vec<CellIndex>,
}

impl Zone {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn cells(&self) -> &[CellIndex] {
        &self.cells
    }
}

/// The cells belonging to one zone around `center`.
///
/// Zone 1 is `grid_disk(1)` (own cell + 6 neighbours); zones 2.. are rings so
/// that cells are never rescanned between zones.
pub fn zone_cells(center: CellIndex, zone: u32) -> Zone {
    let cells = if zone == 1 {
        center.grid_disk::<Vec<_>>(1)
    } else {
        center.grid_ring::<Vec<_>>(zone)
    };
    Zone { id: zone, cells }
}

/// Full search plan: zones 1..=max expanded from `center`.
pub fn scan_plan(center: CellIndex, max_zone: u32) -> Vec<Zone> {
    (1..=max_zone.max(1))
        .map(|zone| zone_cells(center, zone))
        .collect()
}

/// Flat, ordered list of all cells to query for a max zone.
pub fn all_cells(center: CellIndex, max_zone: u32) -> Vec<CellIndex> {
    let mut cells = Vec::new();
    for zone in scan_plan(center, max_zone) {
        cells.extend(zone.cells().iter().copied());
    }
    cells
}

/// Finds if a target cell is reachable within `max_zone` zones and its zone id.
pub fn zone_of(center: CellIndex, target: CellIndex, max_zone: u32) -> Option<u32> {
    center
        .grid_disk_distances::<Vec<_>>(max_zone)
        .into_iter()
        .find(|(cell, _)| *cell == target)
        .map(|(_, distance)| distance.max(1))
}

/// Scans the board record of every cell within `max_zone` of `center` and
/// returns all decodable, still-live ride requests. Board records of
/// unrelated or empty cells are skipped; progress is streamed through the
/// `log` callback.
pub async fn collect_board_requests<F>(
    routing: &RoutingContext,
    center: CellIndex,
    max_zone: u32,
    mut log: F,
) -> Vec<ObfuscatedRideRequest>
where
    F: FnMut(String),
{
    let mut requests = Vec::new();

    for zone in scan_plan(center, max_zone) {
        for cell in zone.cells() {
            let record_key = board_record_key(*cell);
            log(format!(
                "zone {} scanning {} ({})",
                zone.id(),
                board_label(*cell),
                record_key
            ));

            if routing
                .open_dht_record(record_key.clone(), None)
                .await
                .is_err()
            {
                continue;
            }

            let report = match routing
                .inspect_dht_record(record_key.clone(), None, DHTReportScope::SyncGet)
                .await
            {
                Ok(report) => report,
                Err(e) => {
                    log(format!("inspect failed: {}", e));
                    let _ = routing.close_dht_record(record_key.clone()).await;
                    continue;
                }
            };

            for (subkey, local_seq, network_seq) in report
                .subkeys()
                .iter()
                .zip(report.local_seqs().iter())
                .zip(report.network_seqs().iter())
                .map(|((sk, lseq), nseq)| (sk, lseq, nseq))
            {
                if network_seq.is_none() && local_seq.is_none() {
                    continue;
                }
                let Some(dht_value) = routing
                    .get_dht_value(record_key.clone(), subkey, false)
                    .await
                    .ok()
                    .flatten()
                else {
                    continue;
                };
                if let Ok(request) = ObfuscatedRideRequest::decode(dht_value.data()) {
                    requests.push(request);
                }
            }

            let _ = routing.close_dht_record(record_key).await;
        }
    }

    requests
}
