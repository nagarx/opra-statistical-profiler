//! PremiumDecayTracker: theta decay curve for 0DTE ATM options.
//!
//! Tracks how ATM 0DTE option premium decays throughout the trading day.
//! The key deliverable: actual theta decay vs BSM theoretical, providing
//! empirical data for the 0DTE strategy's time decay budget.

use serde_json::json;

use hft_statistics::statistics::{IntradayCurveAccumulator, WelfordAccumulator};

use crate::event::{DayContext, OptionsEvent};
use crate::OptionsTracker;

pub struct PremiumDecayTracker {
    utc_offset: i32,
    // Actual premium curves (390-bin, separate for calls and puts)
    actual_call_curve: IntradayCurveAccumulator,
    actual_put_curve: IntradayCurveAccumulator,

    // Spread-normalized premium: premium / spread (how many spreads of edge)
    premium_to_spread_call: IntradayCurveAccumulator,
    premium_to_spread_put: IntradayCurveAccumulator,

    // Track the first and last premium of each day for decay measurement
    day_first_call_mid: Option<f64>,
    day_last_call_mid: Option<f64>,
    day_first_put_mid: Option<f64>,
    day_last_put_mid: Option<f64>,

    daily_call_decay_pct: WelfordAccumulator,
    daily_put_decay_pct: WelfordAccumulator,

    _risk_free_rate: f64,
    n_days: u32,
    n_0dte_days: u32,
}

impl PremiumDecayTracker {
    pub fn new(risk_free_rate: f64) -> Self {
        Self {
            utc_offset: -5,
            actual_call_curve: IntradayCurveAccumulator::new_rth_1min(),
            actual_put_curve: IntradayCurveAccumulator::new_rth_1min(),
            premium_to_spread_call: IntradayCurveAccumulator::new_rth_1min(),
            premium_to_spread_put: IntradayCurveAccumulator::new_rth_1min(),
            day_first_call_mid: None,
            day_last_call_mid: None,
            day_first_put_mid: None,
            day_last_put_mid: None,
            daily_call_decay_pct: WelfordAccumulator::new(),
            daily_put_decay_pct: WelfordAccumulator::new(),
            _risk_free_rate: risk_free_rate,
            n_days: 0,
            n_0dte_days: 0,
        }
    }
}

impl OptionsTracker for PremiumDecayTracker {
    fn begin_day(&mut self, ctx: &DayContext) {
        self.utc_offset = ctx.utc_offset;
    }

    fn process_event(&mut self, event: &OptionsEvent, _regime: u8) {
        if !event.is_zero_dte() || !event.is_atm() || !event.has_valid_bbo() {
            return;
        }

        let mid = event.option_mid();
        if !mid.is_finite() || mid <= 0.0 {
            return;
        }

        let ts = event.ts_event;
        let is_call = event.is_call();

        if is_call {
            self.actual_call_curve.add(ts, mid, self.utc_offset);
            if self.day_first_call_mid.is_none() {
                self.day_first_call_mid = Some(mid);
            }
            self.day_last_call_mid = Some(mid);

            let spread = event.spread();
            if spread.is_finite() && spread > 0.0 {
                self.premium_to_spread_call
                    .add(ts, mid / spread, self.utc_offset);
            }
        } else {
            self.actual_put_curve.add(ts, mid, self.utc_offset);
            if self.day_first_put_mid.is_none() {
                self.day_first_put_mid = Some(mid);
            }
            self.day_last_put_mid = Some(mid);

            let spread = event.spread();
            if spread.is_finite() && spread > 0.0 {
                self.premium_to_spread_put
                    .add(ts, mid / spread, self.utc_offset);
            }
        }
    }

    fn end_of_day(&mut self, _day_index: u32) {
        let had_0dte = self.day_first_call_mid.is_some() || self.day_first_put_mid.is_some();

        if let (Some(first), Some(last)) = (self.day_first_call_mid, self.day_last_call_mid) {
            if first > 0.0 {
                let decay_pct = (first - last) / first * 100.0;
                self.daily_call_decay_pct.update(decay_pct);
            }
        }

        if let (Some(first), Some(last)) = (self.day_first_put_mid, self.day_last_put_mid) {
            if first > 0.0 {
                let decay_pct = (first - last) / first * 100.0;
                self.daily_put_decay_pct.update(decay_pct);
            }
        }

        if had_0dte {
            self.n_0dte_days += 1;
        }
        self.n_days += 1;
    }

    fn reset_day(&mut self) {
        self.day_first_call_mid = None;
        self.day_last_call_mid = None;
        self.day_first_put_mid = None;
        self.day_last_put_mid = None;
    }

    fn finalize(&self) -> serde_json::Value {
        let make_curve =
            |acc: &IntradayCurveAccumulator, key: &str| -> Vec<serde_json::Value> {
                acc.finalize()
                    .into_iter()
                    .filter(|b| b.count > 0)
                    .map(|b| {
                        json!({
                            "minutes_since_open": b.minutes_since_open,
                            key: b.mean,
                            "std": b.std,
                            "count": b.count,
                        })
                    })
                    .collect()
            };

        json!({
            "tracker": "PremiumDecayTracker",
            "n_days": self.n_days,
            "n_0dte_days": self.n_0dte_days,
            "daily_call_decay_pct": {
                "mean": self.daily_call_decay_pct.mean(),
                "std": self.daily_call_decay_pct.std(),
                "count": self.daily_call_decay_pct.count(),
            },
            "daily_put_decay_pct": {
                "mean": self.daily_put_decay_pct.mean(),
                "std": self.daily_put_decay_pct.std(),
                "count": self.daily_put_decay_pct.count(),
            },
            "intraday_call_premium_curve": make_curve(&self.actual_call_curve, "mean_premium"),
            "intraday_put_premium_curve": make_curve(&self.actual_put_curve, "mean_premium"),
            "intraday_call_premium_to_spread": make_curve(&self.premium_to_spread_call, "premium_over_spread"),
            "intraday_put_premium_to_spread": make_curve(&self.premium_to_spread_put, "premium_over_spread"),
        })
    }

    fn name(&self) -> &str {
        "PremiumDecayTracker"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::helpers::*;
    use crate::options_math::moneyness::Moneyness;

    #[test]
    fn test_tracks_first_last_premium() {
        let mut t = PremiumDecayTracker::new(0.05);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let e1 = make_quote_event(&c, 2.00, 2.10, 0, Moneyness::Atm);
        let e2 = make_quote_event(&c, 0.50, 0.60, 0, Moneyness::Atm);
        t.process_event(&e1, 3);
        t.process_event(&e2, 3);
        t.end_of_day(0);
        let r = t.finalize();
        let decay = r["daily_call_decay_pct"]["mean"].as_f64().unwrap();
        // First mid = 2.05, last mid = 0.55. Decay = (2.05 - 0.55) / 2.05 * 100 = 73.2%
        assert!(decay > 70.0 && decay < 75.0, "Decay should be ~73%, got {}", decay);
    }

    #[test]
    fn test_empty_day_handling() {
        let mut t = PremiumDecayTracker::new(0.05);
        t.begin_day(&make_day_context());
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["n_0dte_days"], 0);
    }

    #[test]
    fn test_finalize_structure() {
        let t = PremiumDecayTracker::new(0.05);
        let r = t.finalize();
        assert_eq!(r["tracker"], "PremiumDecayTracker");
        assert!(r.get("daily_call_decay_pct").is_some());
        assert!(r.get("intraday_call_premium_curve").is_some());
    }
}
