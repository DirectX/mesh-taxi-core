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
/// `log` callback. Cells are probed concurrently (bounded pool) and each
/// cell probe is hard-capped at [`CELL_BUDGET`] so empty zones cannot stall
/// the whole scan for one DHT timeout per cell.
pub async fn collect_board_requests<F>(
    routing: &RoutingContext,
    center: CellIndex,
    max_zone: u32,
    mut log: F,
) -> Vec<ObfuscatedRideRequest>
where
    F: FnMut(String) + Send + 'static,
{
    use std::sync::Arc;
    use tokio::sync::{Semaphore, mpsc};

    let radar = Arc::new(Semaphore::new(SCAN_CONCURRENCY));
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // forward the streamed cell logs to the caller callback while the
    // concurrent probes are still running.
    let fwd = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            log(line);
        }
    });

    let probes = all_cells(center, max_zone)
        .into_iter()
        .map(|cell| {
            let routing = routing.clone();
            let radar = radar.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let _slot = radar.acquire().await;
                let fut = probe_cell(&routing, cell, |line| {
                    let _ = tx.send(line);
                });
                match tokio::time::timeout(CELL_BUDGET, fut).await {
                    Ok(requests) => requests,
                    Err(_) => {
                        let _ = tx.send(format!(
                            "zone scan of {} timed out after {:?}",
                            board_label(cell),
                            CELL_BUDGET
                        ));
                        Vec::new()
                    }
                }
            })
        })
        .collect::<Vec<_>>();

    // explicit so the rx sender can drop to end the drain
    drop(tx);

    let mut requests = Vec::new();
    for probe in probes {
        if let Ok(found) = probe.await {
            requests.extend(found);
        }
    }
    let _ = fwd.await;
    requests
}

/// One cell probe: open the board record, inspect live subkeys and decode.
/// Returns every decodable, still-live request on the board.
async fn probe_cell<F: FnMut(String)>(
    routing: &RoutingContext,
    cell: CellIndex,
    mut log: F,
) -> Vec<ObfuscatedRideRequest> {
    let record_key = board_record_key(cell);
    log(format!(
        "zone scan of {} ({})",
        board_label(cell),
        record_key
    ));

    if routing
        .open_dht_record(record_key.clone(), None)
        .await
        .is_err()
    {
        return Vec::new();
    }

    let report = match routing
        .inspect_dht_record(record_key.clone(), None, DHTReportScope::SyncGet)
        .await
    {
        Ok(report) => report,
        Err(e) => {
            log(format!("inspect failed: {}", e));
            let _ = routing.close_dht_record(record_key.clone()).await;
            return Vec::new();
        }
    };

    let mut requests = Vec::new();
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
    requests
}

/// Bound for one cell probe; the empty-cell DHT timeout is ~10s so this is a
/// safety net rather than the expected cost.
pub const CELL_BUDGET: std::time::Duration = std::time::Duration::from_secs(45);

/// How many cells to probe at once during a board scan.
pub const SCAN_CONCURRENCY: usize = 12;
