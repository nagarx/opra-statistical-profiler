//! Shared report utilities for OPRA trackers.
//!
//! Centralizes DTE bucketing, moneyness indexing, and intraday curve
//! finalization to eliminate copy-paste across tracker implementations.

use hft_statistics::statistics::IntradayCurveAccumulator;
use serde_json::json;

use crate::options_math::moneyness::Moneyness;

/// DTE bucket labels.
pub const DTE_LABELS: [&str; 4] = ["0dte", "1dte", "2_7dte", "other"];

/// Moneyness labels.
pub const MONEYNESS_LABELS: [&str; 5] = ["deep_itm", "itm", "atm", "otm", "deep_otm"];

/// Map a DTE value to a bucket index (0-3).
///
/// | DTE | Bucket | Label |
/// |-----|--------|-------|
/// | 0 | 0 | 0dte |
/// | 1 | 1 | 1dte |
/// | 2-7 | 2 | 2_7dte |
/// | 8+ | 3 | other |
#[inline]
pub fn dte_bucket_index(dte: i64) -> usize {
    match dte {
        0 => 0,
        1 => 1,
        2..=7 => 2,
        _ => 3,
    }
}

/// Map a `Moneyness` classification to an index (0-4).
#[inline]
pub fn moneyness_index(m: Moneyness) -> usize {
    match m {
        Moneyness::DeepItm => 0,
        Moneyness::Itm => 1,
        Moneyness::Atm => 2,
        Moneyness::Otm => 3,
        Moneyness::DeepOtm => 4,
    }
}

/// Convert an `IntradayCurveAccumulator` to a JSON array for output.
///
/// Filters to bins with count > 0 and uses `value_key` as the field name
/// for the mean value (e.g., "mean_spread", "mean_premium", "trade_count").
pub fn finalize_curve(acc: &IntradayCurveAccumulator, value_key: &str) -> Vec<serde_json::Value> {
    acc.finalize()
        .into_iter()
        .filter(|b| b.count > 0)
        .map(|b| {
            json!({
                "minutes_since_open": b.minutes_since_open,
                value_key: b.mean,
                "std": b.std,
                "count": b.count,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dte_bucket_index() {
        assert_eq!(dte_bucket_index(0), 0);
        assert_eq!(dte_bucket_index(1), 1);
        assert_eq!(dte_bucket_index(2), 2);
        assert_eq!(dte_bucket_index(7), 2);
        assert_eq!(dte_bucket_index(8), 3);
        assert_eq!(dte_bucket_index(365), 3);
        assert_eq!(dte_bucket_index(-1), 3);
    }

    #[test]
    fn test_moneyness_index() {
        assert_eq!(moneyness_index(Moneyness::DeepItm), 0);
        assert_eq!(moneyness_index(Moneyness::Itm), 1);
        assert_eq!(moneyness_index(Moneyness::Atm), 2);
        assert_eq!(moneyness_index(Moneyness::Otm), 3);
        assert_eq!(moneyness_index(Moneyness::DeepOtm), 4);
    }

    #[test]
    fn test_dte_labels_match_indices() {
        assert_eq!(DTE_LABELS[dte_bucket_index(0)], "0dte");
        assert_eq!(DTE_LABELS[dte_bucket_index(1)], "1dte");
        assert_eq!(DTE_LABELS[dte_bucket_index(5)], "2_7dte");
        assert_eq!(DTE_LABELS[dte_bucket_index(30)], "other");
    }

    #[test]
    fn test_moneyness_labels_match_indices() {
        assert_eq!(MONEYNESS_LABELS[moneyness_index(Moneyness::Atm)], "atm");
        assert_eq!(MONEYNESS_LABELS[moneyness_index(Moneyness::DeepOtm)], "deep_otm");
    }
}
