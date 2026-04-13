//! SpreadTracker: BBO spread distribution by DTE and moneyness.
//!
//! Tracks option bid-ask spread in USD and as percentage of mid-price,
//! with breakdown by DTE bucket, moneyness, and intraday curve.

use serde_json::json;

use hft_statistics::statistics::{
    IntradayCurveAccumulator, StreamingDistribution, WelfordAccumulator,
};

use crate::event::{DayContext, OptionsEvent};
use crate::report_utils;
use crate::OptionsTracker;

pub struct SpreadTracker {
    all_spread_usd: StreamingDistribution,
    all_spread_pct: StreamingDistribution,
    dte0_atm_spread_usd: StreamingDistribution,
    dte0_atm_spread_pct: StreamingDistribution,
    dte0_atm_intraday_spread: IntradayCurveAccumulator,
    dte0_atm_intraday_mid: IntradayCurveAccumulator,
    per_moneyness_spread: [WelfordAccumulator; 5],
    per_dte_spread: [WelfordAccumulator; 4],
    utc_offset: i32,
    n_days: u32,
}

impl SpreadTracker {
    pub fn new(reservoir_capacity: usize) -> Self {
        Self {
            all_spread_usd: StreamingDistribution::new(reservoir_capacity),
            all_spread_pct: StreamingDistribution::new(reservoir_capacity),
            dte0_atm_spread_usd: StreamingDistribution::new(reservoir_capacity),
            dte0_atm_spread_pct: StreamingDistribution::new(reservoir_capacity),
            dte0_atm_intraday_spread: IntradayCurveAccumulator::new_rth_1min(),
            dte0_atm_intraday_mid: IntradayCurveAccumulator::new_rth_1min(),
            per_moneyness_spread: std::array::from_fn(|_| WelfordAccumulator::new()),
            per_dte_spread: std::array::from_fn(|_| WelfordAccumulator::new()),
            utc_offset: -5,
            n_days: 0,
        }
    }

}

impl OptionsTracker for SpreadTracker {
    fn begin_day(&mut self, ctx: &DayContext) {
        self.utc_offset = ctx.utc_offset;
    }

    fn process_event(&mut self, event: &OptionsEvent, _regime: u8) {
        if !event.has_valid_bbo() {
            return;
        }

        let spread = event.spread();
        let spread_pct = event.spread_pct();

        if spread.is_finite() {
            self.all_spread_usd.add(spread);
            let mi = report_utils::moneyness_index(event.moneyness);
            self.per_moneyness_spread[mi].update(spread);
            let di = report_utils::dte_bucket_index(event.dte);
            self.per_dte_spread[di].update(spread);
        }

        if spread_pct.is_finite() {
            self.all_spread_pct.add(spread_pct);
        }

        if event.is_zero_dte() && event.is_atm() {
            if spread.is_finite() {
                self.dte0_atm_spread_usd.add(spread);
                self.dte0_atm_intraday_spread
                    .add(event.ts_event, spread, self.utc_offset);
            }
            if spread_pct.is_finite() {
                self.dte0_atm_spread_pct.add(spread_pct);
            }
            let mid = event.option_mid();
            if mid.is_finite() {
                self.dte0_atm_intraday_mid
                    .add(event.ts_event, mid, self.utc_offset);
            }
        }
    }

    fn end_of_day(&mut self, _day_index: u32) {
        self.n_days += 1;
    }

    fn reset_day(&mut self) {}

    fn finalize(&self) -> serde_json::Value {
        let moneyness_labels = report_utils::MONEYNESS_LABELS;
        let mut moneyness_spreads = serde_json::Map::new();
        for (i, label) in moneyness_labels.iter().enumerate() {
            moneyness_spreads.insert(
                label.to_string(),
                json!({
                    "mean": self.per_moneyness_spread[i].mean(),
                    "std": self.per_moneyness_spread[i].std(),
                    "count": self.per_moneyness_spread[i].count(),
                }),
            );
        }

        let dte_labels = report_utils::DTE_LABELS;
        let mut dte_spreads = serde_json::Map::new();
        for (i, label) in dte_labels.iter().enumerate() {
            dte_spreads.insert(
                label.to_string(),
                json!({
                    "mean": self.per_dte_spread[i].mean(),
                    "std": self.per_dte_spread[i].std(),
                    "count": self.per_dte_spread[i].count(),
                }),
            );
        }

        let intraday_spread: Vec<serde_json::Value> = self
            .dte0_atm_intraday_spread
            .finalize()
            .into_iter()
            .filter(|b| b.count > 0)
            .map(|b| {
                json!({
                    "minutes_since_open": b.minutes_since_open,
                    "mean_spread_usd": b.mean,
                    "std": b.std,
                    "count": b.count,
                })
            })
            .collect();

        let intraday_mid: Vec<serde_json::Value> = self
            .dte0_atm_intraday_mid
            .finalize()
            .into_iter()
            .filter(|b| b.count > 0)
            .map(|b| {
                json!({
                    "minutes_since_open": b.minutes_since_open,
                    "mean_mid": b.mean,
                    "count": b.count,
                })
            })
            .collect();

        json!({
            "tracker": "SpreadTracker",
            "n_days": self.n_days,
            "all_spread_usd": self.all_spread_usd.summary(),
            "all_spread_pct": self.all_spread_pct.summary(),
            "dte0_atm_spread_usd": self.dte0_atm_spread_usd.summary(),
            "dte0_atm_spread_pct": self.dte0_atm_spread_pct.summary(),
            "spread_by_moneyness": moneyness_spreads,
            "spread_by_dte": dte_spreads,
            "dte0_atm_intraday_spread_curve": intraday_spread,
            "dte0_atm_intraday_mid_curve": intraday_mid,
        })
    }

    fn name(&self) -> &str {
        "SpreadTracker"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::helpers::*;
    use crate::options_math::moneyness::Moneyness;

    #[test]
    fn test_spread_computation() {
        let mut t = SpreadTracker::new(1000);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let e = make_quote_event(&c, 1.00, 1.05, 0, Moneyness::Atm);
        t.process_event(&e, 3);
        t.end_of_day(0);
        let r = t.finalize();
        let mean = r["all_spread_usd"]["mean"].as_f64().unwrap();
        assert!((mean - 0.05).abs() < 1e-10, "Spread: expected 0.05, got {}", mean);
    }

    #[test]
    fn test_sentinel_filtered() {
        let mut t = SpreadTracker::new(1000);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let e = make_quote_event(&c, f64::NAN, f64::NAN, 0, Moneyness::Atm);
        t.process_event(&e, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["all_spread_usd"]["count"], 0);
    }

    #[test]
    fn test_dte0_atm_tracking() {
        let mut t = SpreadTracker::new(1000);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let atm = make_quote_event(&c, 1.00, 1.03, 0, Moneyness::Atm);
        let otm = make_quote_event(&c, 0.10, 0.15, 0, Moneyness::Otm);
        t.process_event(&atm, 3);
        t.process_event(&otm, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["dte0_atm_spread_usd"]["count"], 1, "Only ATM 0DTE should be counted");
    }

    #[test]
    fn test_finalize_structure() {
        let t = SpreadTracker::new(1000);
        let r = t.finalize();
        assert_eq!(r["tracker"], "SpreadTracker");
        assert!(r.get("spread_by_moneyness").is_some());
        assert!(r.get("spread_by_dte").is_some());
        assert!(r.get("dte0_atm_intraday_spread_curve").is_some());
    }
}
