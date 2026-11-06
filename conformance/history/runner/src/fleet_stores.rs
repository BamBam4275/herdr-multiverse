//! `breaker-state.json` / `seat-state.json` / `gauge-state.json` shape
//! checks — the three fleet-level stores this lane's brief names
//! explicitly ("breaker/gauge/seat stores").
//!
//! Deserialize entries with the production `mv-state` types so optional
//! fields, nulls, and unknown fields have exactly the same tolerance.
//! Unlike the fail-open store loaders, report malformed entries rather
//! than silently replacing the whole store with an empty map.

use std::path::Path;

use mv_state::{breaker_store::BreakerEntry, gauge::GaugeEntry, seatstate_store::SeatEntry};
use serde_json::Value;

use crate::finding::Report;

fn require_object<'a>(
    v: &'a Value,
    file: &Path,
    report: &mut Report,
) -> Option<&'a serde_json::Map<String, Value>> {
    match v.as_object() {
        Some(m) => Some(m),
        None => {
            report.fail(file, None, "top-level JSON value is not an object");
            None
        }
    }
}

pub fn check_breaker_state(file: &Path, report: &mut Report) {
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            report.fail(file, Some(e.line()), format!("breaker-state.json: {e}"));
            return;
        }
    };
    let Some(pools) = require_object(&parsed, file, report) else {
        return;
    };
    for (pool, entry) in pools {
        if let Err(e) = serde_json::from_value::<BreakerEntry>(entry.clone()) {
            report.fail(file, None, format!("pool {pool:?}: {e}"));
            continue;
        }
        if pool.matches(':').count() > 1 {
            report.note("fleet_stores.breaker_pool_shape_unexpected");
        } else {
            report.note("fleet_stores.breaker_pool_ok");
        }
    }
}

pub fn check_seat_state(file: &Path, report: &mut Report) {
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            report.fail(file, Some(e.line()), format!("seat-state.json: {e}"));
            return;
        }
    };
    let Some(seats) = require_object(&parsed, file, report) else {
        return;
    };
    for (seat, entry) in seats {
        if let Err(e) = serde_json::from_value::<SeatEntry>(entry.clone()) {
            report.fail(file, None, format!("seat {seat:?}: {e}"));
            continue;
        }
        report.note("fleet_stores.seat_entry_ok");
    }
}

pub fn check_gauge_state(file: &Path, report: &mut Report) {
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            report.fail(file, Some(e.line()), format!("gauge-state.json: {e}"));
            return;
        }
    };
    let Some(pools) = require_object(&parsed, file, report) else {
        return;
    };
    for (pool, entry) in pools {
        if let Err(e) = serde_json::from_value::<GaugeEntry>(entry.clone()) {
            report.fail(file, None, format!("pool {pool:?}: {e}"));
            continue;
        }
        if entry.as_object().is_some_and(|obj| {
            obj.keys()
                .any(|k| k.starts_with("secondary_") || k.ends_with("_secondary"))
        }) {
            report.note("fleet_stores.gauge_has_secondary_window");
        }
        report.note("fleet_stores.gauge_pool_ok");
    }
}
