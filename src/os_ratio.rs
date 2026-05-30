//! Option-to-stock volume ratio (O/S) — Johnson & So (2012).
//!
//! Reference: Johnson, T. L. & So, E. C. (2012). "The Option to Stock Volume
//! Ratio and Future Returns." *Journal of Financial Economics* 106(2):262-286.
//! Wiki entry: `theory:option_to_stock_volume_ratio`.
//!
//! # Formula (Eq. 9)
//!
//! ```text
//! O/S_d = OPVOL_d / EQVOL_d
//! ```
//!
//! where `OPVOL_d` is the total option-contract volume (calls + puts, UNSIGNED,
//! restricted to the 5-35-DTE band per footnote 9) on day `d`, and `EQVOL_d` is
//! the equity volume expressed in **round lots of 100 shares** (`shares / 100`),
//! to make it commensurate with option contracts (each pertains to 100 shares).
//! O/S is stored as a dimensionless **fraction** (Johnson-So's sample mean is
//! 4.34% = 0.0434); multiply by 100 for a percentage at display time only.
//!
//! `ΔO/S_d` (the single-name analog; Johnson-So define it as the deviation of O/S
//! from its prior-6-month average) is the deviation from a trailing
//! `rolling_window_days` mean:
//!
//! ```text
//! ΔO/S_d = (O/S_d − mean(O/S over the prior `window` days)) / mean(...)
//! ```
//!
//! Days before the window is warm emit `null` (`n_warmup_null = min(window, n_days)`).
//!
//! # Honesty (the load-bearing caveats — also embedded in the emitted JSON)
//!
//! - **AD-OS-1 (CRITICAL):** Johnson-So's headline alpha is CROSS-SECTIONAL (a decile
//!   spread over ~1,700 firms). Single-name NVDA is an UNESTABLISHED extrapolation;
//!   the credible single-name analog is ΔO/S, magnitude UNCALIBRATED.
//! - **AD-OS-2 (HIGH):** NVDA is the WEAKEST tail on both the short-sale-cost (RI)
//!   and the option-leverage axes, so any single-name edge may be near zero.
//! - **No forward-IC claim** is made here. On an 8-day pilot, only `n_days − window`
//!   ΔO/S points exist; pairing with a next-day return gives N≈1-2, far below the
//!   `hft-rules §13` IC gate (`IC_COUNT ≥ 2`, `stability > 2.0`). This module emits
//!   the **instrument** (the per-day O/S/ΔO/S series), not a measured IC.

use chrono::NaiveDate;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The equity round-lot size: Johnson-So report EQVOL in lots of 100 shares so it
/// is commensurate with option contracts (each on 100 shares).
const ROUND_LOT_SIZE: f64 = 100.0;

/// Compute the per-day O/S and ΔO/S series as a JSON report object.
///
/// `opvol_per_day` is the per-day 5-35-DTE option-contract volume (the numerator),
/// `eqvol_per_day` is the per-day equity SHARE volume (the denominator, from the
/// EQUS `OhlcvMsg.volume` field). Both are `(date, volume)` pairs; the function
/// joins them by date, sorts by date (for ΔO/S window correctness + determinism),
/// and is otherwise pure (no profiler/dbn types) for golden-testability.
///
/// `dte_min`/`dte_max` are echoed into the report's `dte_band` for provenance
/// (the `opvol_per_day` is assumed already filtered to that band by the caller).
///
/// Sentinel policy:
/// - `eqvol_d == 0` (e.g. fallback-price mode) → `os_ratio = null`.
/// - `opvol_d == 0` → `os_ratio = 0.0` (FINITE — zero relative option activity,
///   realistic on a 0DTE-dominated window where the 5-35-DTE band is thin).
/// - `date` present in `opvol` but absent from `eqvol` → that day emits `null` and
///   increments `n_join_miss` (recorded, never silently dropped — `hft-rules §8`).
///   In the profiler path this is unreachable (the run hard-errors on a missing
///   underlying price, so eqvol ⊇ opvol by construction); reachable in unit tests.
pub fn compute_os_series(
    opvol_per_day: &[(NaiveDate, u64)],
    eqvol_per_day: &[(NaiveDate, u64)],
    rolling_window_days: usize,
    dte_min: i64,
    dte_max: i64,
) -> Value {
    let eqvol_map: BTreeMap<NaiveDate, u64> =
        eqvol_per_day.iter().map(|(d, v)| (*d, *v)).collect();

    // Sort opvol by date: the profiler emits date-sorted, but enforce it so the
    // ΔO/S trailing window is over true prior calendar days and the output is
    // deterministic regardless of input order.
    let mut days: Vec<(NaiveDate, u64)> = opvol_per_day.to_vec();
    days.sort_by_key(|(d, _)| *d);
    let n_days = days.len();

    let mut n_join_miss: usize = 0;

    // Per-day O/S as a fraction. `None` when eqvol is missing (join miss) or zero.
    let os_values: Vec<Option<f64>> = days
        .iter()
        .map(|(date, opvol)| match eqvol_map.get(date) {
            None => {
                n_join_miss += 1;
                None
            }
            Some(0) => None, // eqvol == 0 → O/S undefined (fallback-price mode)
            Some(&eqvol) => {
                let eqvol_round_lots = eqvol as f64 / ROUND_LOT_SIZE;
                // opvol == 0 → 0.0 (finite); this is correct, not a missing value.
                Some(*opvol as f64 / eqvol_round_lots)
            }
        })
        .collect();

    // ΔO/S = (O/S_d − mean(prior `window` O/S)) / mean. Warmup (d < window) → None.
    // Uses ONLY the strictly-prior `window` days (no look-ahead, hft-rules §9). If
    // any prior O/S is None (eqvol missing/zero) the mean is undefined → None.
    let delta_os: Vec<Option<f64>> = (0..n_days)
        .map(|d| {
            if rolling_window_days == 0 || d < rolling_window_days {
                return None;
            }
            let os_d = os_values[d]?;
            let prior: Option<Vec<f64>> =
                (d - rolling_window_days..d).map(|i| os_values[i]).collect();
            let prior = prior?;
            let mean = prior.iter().sum::<f64>() / prior.len() as f64;
            if mean == 0.0 {
                None
            } else {
                Some((os_d - mean) / mean)
            }
        })
        .collect();

    let per_day: Vec<Value> = days
        .iter()
        .enumerate()
        .map(|(i, (date, opvol))| {
            let eqvol = eqvol_map.get(date).copied();
            json!({
                "date": date.to_string(),
                "opvol": opvol,
                "eqvol_shares": eqvol,
                "eqvol_round_lots": eqvol.map(|v| v as f64 / ROUND_LOT_SIZE),
                "os_ratio": os_values[i],
                "delta_os": delta_os[i],
            })
        })
        .collect();

    let os_finite: Vec<f64> = os_values.iter().filter_map(|x| *x).collect();
    let delta_finite: Vec<f64> = delta_os.iter().filter_map(|x| *x).collect();
    let n_warmup_null = rolling_window_days.min(n_days);

    let mut delta_summary = summary_stats(&delta_finite);
    if let Some(obj) = delta_summary.as_object_mut() {
        obj.insert("n_warmup_null".to_string(), json!(n_warmup_null));
    }

    json!({
        "tracker": "OsRatio",
        "n_days": n_days,
        "rolling_window_days": rolling_window_days,
        "dte_band": [dte_min, dte_max],
        "eqvol_round_lot_size": ROUND_LOT_SIZE as u64,
        "n_join_miss": n_join_miss,
        "per_day": per_day,
        "os_ratio_summary": summary_stats(&os_finite),
        "delta_os_summary": delta_summary,
        "_caveats": "O/S = OPVOL/(EQVOL/100) stored as a FRACTION (Johnson-So 2012 mean 4.34% = 0.0434; x100 for percent). SINGLE-NAME NVDA is an UNESTABLISHED extrapolation of the CROSS-SECTIONAL Johnson-So result (AD-OS-1); NVDA is the WEAKEST short-sale-cost + leverage regime (AD-OS-2); the 5-35-DTE band is the opposite end of the maturity spectrum from the 0DTE-dominated window (AD-OS-4). NO forward-IC claim is made: on a short pilot only (n_days - rolling_window_days) DeltaO/S points exist, far below the hft-rules §13 IC gate. If os_ratio_summary.n_finite == 0 the underlying volume was unavailable (fallback-price mode) — provide an EQUS underlying_prices_file. See theory:option_to_stock_volume_ratio."
    })
}

/// Summary statistics over a finite-value slice. Empty → all-null with n_finite 0.
/// `std` is the population standard deviation.
fn summary_stats(values: &[f64]) -> Value {
    let n = values.len();
    if n == 0 {
        return json!({"mean": null, "std": null, "min": null, "max": null, "n_finite": 0});
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
    let std = var.sqrt();
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    json!({"mean": mean, "std": std, "min": min, "max": max, "n_finite": n})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2025, 11, day).unwrap()
    }

    /// Formula (Eq. 9), stored as a FRACTION: opvol=1000 contracts, eqvol=10M shares
    /// → eqvol_round_lots = 100,000 → O/S = 1000/100,000 = 0.01 (= 1.0%). Locks the
    /// /100 round-lot conversion AND the fraction-not-percent storage convention.
    #[test]
    fn test_os_formula_is_fraction_with_round_lot_conversion() {
        let opvol = vec![(d(13), 1000u64)];
        let eqvol = vec![(d(13), 10_000_000u64)];
        let report = compute_os_series(&opvol, &eqvol, 6, 5, 35);
        let os = report["per_day"][0]["os_ratio"].as_f64().unwrap();
        assert!(
            (os - 0.01).abs() < 1e-12,
            "O/S must be the fraction 0.01 (= 1000/(10M/100)), got {}",
            os
        );
        // Summary echoes the single finite value.
        assert_eq!(report["os_ratio_summary"]["n_finite"].as_u64().unwrap(), 1);
        assert!((report["os_ratio_summary"]["mean"].as_f64().unwrap() - 0.01).abs() < 1e-12);
        // Provenance fields.
        assert_eq!(report["dte_band"][0].as_i64().unwrap(), 5);
        assert_eq!(report["dte_band"][1].as_i64().unwrap(), 35);
        assert_eq!(report["eqvol_round_lot_size"].as_u64().unwrap(), 100);
    }

    /// ΔO/S worked series: eqvol = 100M shares (round_lots = 1M) every day; opvol = 1M
    /// for days 1-6 (O/S=1.0) and 2M for day 7 (O/S=2.0), window=6. day-7 ΔO/S =
    /// (2.0 − mean(1,1,1,1,1,1)) / 1.0 = (2−1)/1 = 1.0. Days 1-6 are warmup-null.
    #[test]
    fn test_delta_os_worked_series_and_warmup_nulls() {
        let mut opvol = Vec::new();
        let mut eqvol = Vec::new();
        for i in 0..6 {
            opvol.push((d(13 + i), 1_000_000u64)); // O/S = 1.0
            eqvol.push((d(13 + i), 100_000_000u64));
        }
        opvol.push((d(19), 2_000_000u64)); // O/S = 2.0
        eqvol.push((d(19), 100_000_000u64));

        let report = compute_os_series(&opvol, &eqvol, 6, 5, 35);
        let per_day = report["per_day"].as_array().unwrap();
        // Days 0..6 (the first `window` days) are warmup-null for ΔO/S.
        for (i, day) in per_day.iter().enumerate().take(6) {
            assert!(
                day["delta_os"].is_null(),
                "day {} must be ΔO/S warmup-null",
                i
            );
        }
        // Day 6 (index 6) is the first non-warmup ΔO/S = 1.0.
        let delta7 = per_day[6]["delta_os"].as_f64().unwrap();
        assert!(
            (delta7 - 1.0).abs() < 1e-12,
            "day-7 ΔO/S must be (2-1)/1 = 1.0, got {}",
            delta7
        );
        assert_eq!(
            report["delta_os_summary"]["n_warmup_null"].as_u64().unwrap(),
            6
        );
        assert_eq!(report["delta_os_summary"]["n_finite"].as_u64().unwrap(), 1);
    }

    /// Boundary: with window=6, index 5 (window-1) is the LAST warmup-null and
    /// index 6 (window) is the FIRST non-null ΔO/S. The classic off-by-one site.
    #[test]
    fn test_delta_os_boundary_first_nonnull_at_window_index() {
        let mut opvol = Vec::new();
        let mut eqvol = Vec::new();
        for i in 0..7 {
            opvol.push((d(13 + i), 1_000_000u64 * (i as u64 + 1)));
            eqvol.push((d(13 + i), 100_000_000u64));
        }
        let report = compute_os_series(&opvol, &eqvol, 6, 5, 35);
        let per_day = report["per_day"].as_array().unwrap();
        assert!(per_day[5]["delta_os"].is_null(), "index window-1 must be null");
        assert!(
            per_day[6]["delta_os"].is_f64(),
            "index window must be the first non-null ΔO/S"
        );
    }

    /// OPVOL_d = 0 → O/S_d = 0.0 (FINITE, not null). Realistic on a 0DTE-dominated
    /// window where the 5-35-DTE band can be empty on a given day.
    #[test]
    fn test_opvol_zero_yields_finite_zero_os() {
        let opvol = vec![(d(13), 0u64)];
        let eqvol = vec![(d(13), 10_000_000u64)];
        let report = compute_os_series(&opvol, &eqvol, 6, 5, 35);
        let os = &report["per_day"][0]["os_ratio"];
        assert!(!os.is_null(), "OPVOL=0 must give a FINITE O/S (0.0), not null");
        assert_eq!(os.as_f64().unwrap(), 0.0);
        assert_eq!(report["os_ratio_summary"]["n_finite"].as_u64().unwrap(), 1);
    }

    /// EQVOL_d = 0 (fallback-price mode) → O/S_d = null.
    #[test]
    fn test_eqvol_zero_yields_null_os() {
        let opvol = vec![(d(13), 1000u64)];
        let eqvol = vec![(d(13), 0u64)];
        let report = compute_os_series(&opvol, &eqvol, 6, 5, 35);
        assert!(report["per_day"][0]["os_ratio"].is_null());
        assert_eq!(report["os_ratio_summary"]["n_finite"].as_u64().unwrap(), 0);
    }

    /// All-zero OPVOL → O/S all 0.0; the rolling mean is 0 → ΔO/S null (no /0).
    #[test]
    fn test_all_zero_opvol_delta_null_on_zero_mean() {
        let mut opvol = Vec::new();
        let mut eqvol = Vec::new();
        for i in 0..7 {
            opvol.push((d(13 + i), 0u64));
            eqvol.push((d(13 + i), 100_000_000u64));
        }
        let report = compute_os_series(&opvol, &eqvol, 6, 5, 35);
        let per_day = report["per_day"].as_array().unwrap();
        assert_eq!(per_day[6]["os_ratio"].as_f64().unwrap(), 0.0);
        assert!(
            per_day[6]["delta_os"].is_null(),
            "ΔO/S must be null when the prior-window mean is 0 (no division by zero)"
        );
    }

    /// Date-join miss: an opvol day absent from eqvol emits null O/S, increments
    /// n_join_miss, and STILL appears in per_day (recorded, not dropped — §8).
    #[test]
    fn test_date_join_miss_recorded_not_dropped() {
        let opvol = vec![(d(13), 1000u64), (d(14), 2000u64)];
        let eqvol = vec![(d(13), 10_000_000u64)]; // d(14) missing
        let report = compute_os_series(&opvol, &eqvol, 6, 5, 35);
        assert_eq!(report["n_join_miss".to_string()].as_u64().unwrap(), 1);
        let per_day = report["per_day"].as_array().unwrap();
        assert_eq!(per_day.len(), 2, "the missing day must still appear in per_day");
        assert!(per_day[1]["os_ratio"].is_null());
        assert!(per_day[1]["eqvol_shares"].is_null());
    }

    /// Determinism (§7): identical inputs → byte-identical serialization, regardless
    /// of input order (the function sorts by date internally).
    #[test]
    fn test_determinism_and_date_sort() {
        let opvol_a = vec![(d(13), 1000u64), (d(14), 2000u64), (d(17), 3000u64)];
        let opvol_b = vec![(d(17), 3000u64), (d(13), 1000u64), (d(14), 2000u64)]; // shuffled
        let eqvol = vec![
            (d(13), 10_000_000u64),
            (d(14), 10_000_000u64),
            (d(17), 10_000_000u64),
        ];
        let ra = compute_os_series(&opvol_a, &eqvol, 6, 5, 35);
        let rb = compute_os_series(&opvol_b, &eqvol, 6, 5, 35);
        assert_eq!(
            serde_json::to_string(&ra).unwrap(),
            serde_json::to_string(&rb).unwrap(),
            "output must be order-independent (sorted by date) and deterministic"
        );
    }

    /// Shape (§6): per_day length == n_days; all required keys present.
    #[test]
    fn test_shape_per_day_keys() {
        let opvol = vec![(d(13), 1000u64), (d(14), 2000u64)];
        let eqvol = vec![(d(13), 10_000_000u64), (d(14), 10_000_000u64)];
        let report = compute_os_series(&opvol, &eqvol, 6, 5, 35);
        assert_eq!(report["n_days"].as_u64().unwrap(), 2);
        let per_day = report["per_day"].as_array().unwrap();
        assert_eq!(per_day.len(), 2);
        for day in per_day {
            for key in ["date", "opvol", "eqvol_shares", "eqvol_round_lots", "os_ratio", "delta_os"] {
                assert!(day.get(key).is_some(), "per_day entry missing key '{}'", key);
            }
        }
        assert!(report.get("_caveats").is_some());
        assert!(report["_caveats"].as_str().unwrap().contains("UNESTABLISHED"));
    }

    /// Invariant (§6): n_warmup_null == min(window, n_days) even when n_days < window.
    #[test]
    fn test_n_warmup_null_clamps_to_n_days() {
        let opvol = vec![(d(13), 1000u64), (d(14), 2000u64), (d(17), 3000u64)];
        let eqvol = vec![
            (d(13), 10_000_000u64),
            (d(14), 10_000_000u64),
            (d(17), 10_000_000u64),
        ];
        // window=6 > n_days=3 → every day is warmup; n_warmup_null = 3.
        let report = compute_os_series(&opvol, &eqvol, 6, 5, 35);
        assert_eq!(
            report["delta_os_summary"]["n_warmup_null"].as_u64().unwrap(),
            3
        );
        let per_day = report["per_day"].as_array().unwrap();
        for day in per_day {
            assert!(day["delta_os"].is_null());
        }
    }

    /// No look-ahead (§9): ΔO/S_d depends ONLY on O/S_d and the strictly-prior
    /// window. Two series identical through day d but differing at day d+1 must
    /// produce the SAME ΔO/S_d.
    #[test]
    fn test_no_lookahead_future_day_does_not_affect_delta() {
        let eqvol: Vec<(NaiveDate, u64)> = (0..8).map(|i| (d(13 + i), 100_000_000u64)).collect();
        let mut base: Vec<(NaiveDate, u64)> = (0..7).map(|i| (d(13 + i), 1_000_000u64)).collect();
        base.push((d(20), 2_000_000u64)); // day 7 (index 7)
        let mut alt = base.clone();
        alt[7] = (d(20), 9_000_000u64); // change ONLY the future day (index 7)

        let r_base = compute_os_series(&base, &eqvol, 6, 5, 35);
        let r_alt = compute_os_series(&alt, &eqvol, 6, 5, 35);
        // ΔO/S at index 6 must be identical (it depends only on indices 0..6).
        assert_eq!(
            r_base["per_day"][6]["delta_os"],
            r_alt["per_day"][6]["delta_os"],
            "ΔO/S_6 must not depend on the future day index 7 (no look-ahead)"
        );
    }
}
