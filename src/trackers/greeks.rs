//! GreeksTracker: implied volatility and Greek computation for ATM options.
//!
//! Computes IV from BSM for ATM contracts by DTE bucket,
//! and tracks delta/gamma distributions for 0DTE ATM.

use serde_json::json;

use hft_statistics::statistics::{
    IntradayCurveAccumulator, StreamingDistribution, WelfordAccumulator,
};

use crate::event::{DayContext, OptionsEvent};
use crate::options_math::bsm;
use crate::report_utils::DTE_LABELS;
use crate::OptionsTracker;


pub struct GreeksTracker {
    utc_offset: i32,
    // 0DTE ATM IV
    dte0_atm_call_iv: StreamingDistribution,
    dte0_atm_put_iv: StreamingDistribution,

    // Intraday IV curve (390-bin) for 0DTE ATM contracts (both calls and puts contribute)
    dte0_atm_iv_curve: IntradayCurveAccumulator,

    // Delta/gamma for 0DTE ATM
    dte0_atm_delta: StreamingDistribution,
    dte0_atm_gamma: StreamingDistribution,
    dte0_atm_vega: StreamingDistribution,

    // IV by DTE bucket: [0dte, 1dte, weekly, other]
    dte_bucket_iv: [WelfordAccumulator; 4],

    risk_free_rate: f64,
    /// Counts every qualifying ATM event (used for sampling gate).
    atm_quote_count: u64,
    /// How often to compute IV (every Nth qualifying ATM quote).
    iv_sample_interval: u64,
    iv_computed: u64,
    iv_failed: u64,
    n_days: u32,
}

impl GreeksTracker {
    pub fn new(risk_free_rate: f64, reservoir_capacity: usize) -> Self {
        Self::with_sample_interval(risk_free_rate, reservoir_capacity, 100)
    }

    pub fn with_sample_interval(
        risk_free_rate: f64,
        reservoir_capacity: usize,
        iv_sample_interval: u64,
    ) -> Self {
        Self {
            utc_offset: -5,
            dte0_atm_call_iv: StreamingDistribution::new(reservoir_capacity),
            dte0_atm_put_iv: StreamingDistribution::new(reservoir_capacity),
            dte0_atm_iv_curve: IntradayCurveAccumulator::new_rth_1min(),
            dte0_atm_delta: StreamingDistribution::new(reservoir_capacity),
            dte0_atm_gamma: StreamingDistribution::new(reservoir_capacity),
            dte0_atm_vega: StreamingDistribution::new(reservoir_capacity),
            dte_bucket_iv: std::array::from_fn(|_| WelfordAccumulator::new()),
            risk_free_rate,
            atm_quote_count: 0,
            iv_sample_interval: iv_sample_interval.max(1),
            iv_computed: 0,
            iv_failed: 0,
            n_days: 0,
        }
    }

}

impl OptionsTracker for GreeksTracker {
    fn begin_day(&mut self, ctx: &DayContext) {
        self.utc_offset = ctx.utc_offset;
    }

    fn process_event(&mut self, event: &OptionsEvent, _regime: u8) {
        if !event.is_atm() || !event.has_valid_bbo() {
            return;
        }

        let mid = event.option_mid();
        if !mid.is_finite() || mid <= 0.0 || event.underlying_price <= 0.0 {
            return;
        }

        // IV computation is expensive — only compute for a sample of events.
        // Sample every Nth qualifying ATM quote to keep throughput high.
        self.atm_quote_count += 1;
        if self.atm_quote_count % self.iv_sample_interval != 0 {
            return;
        }

        let s = event.underlying_price;
        let k = event.contract.strike;
        let is_call = event.is_call();

        // Time to expiry: for 0DTE, estimate from RTH minutes remaining.
        // For longer DTE, use calendar days / 365.
        let t = if event.dte == 0 {
            // Estimate minutes remaining from timestamp
            // RTH closes at 16:00 ET. Convert to UTC using stored offset.
            let close_et_hour: i64 = 16;
            let close_utc_ns: i64 = (close_et_hour - self.utc_offset as i64) * 3600 * 1_000_000_000;
            let event_tod_ns = event.ts_event % (24 * 3600 * 1_000_000_000_i64);
            let remaining_ns = (close_utc_ns - event_tod_ns).max(0);
            // Clamp to RTH session length (390 min). Pre-market events (e.g., 04:00 ET)
            // would otherwise compute 720 min via calendar arithmetic, but trading-time
            // remaining is at most a full RTH session.
            let remaining_minutes = (remaining_ns as f64 / 60_000_000_000.0).min(390.0);
            bsm::minutes_to_years(remaining_minutes)
        } else {
            (event.dte as f64 / 365.0).max(1e-6)
        };

        let iv = bsm::implied_vol(mid, s, k, t, self.risk_free_rate, is_call);
        match iv {
            Some(sigma) if sigma.is_finite() && sigma > 0.0 && sigma < 5.0 => {
                self.iv_computed += 1;

                let di = crate::report_utils::dte_bucket_index(event.dte);
                self.dte_bucket_iv[di].update(sigma);

                if event.is_zero_dte() {
                    if is_call {
                        self.dte0_atm_call_iv.add(sigma);
                    } else {
                        self.dte0_atm_put_iv.add(sigma);
                    }
                    self.dte0_atm_iv_curve
                        .add(event.ts_event, sigma, self.utc_offset);

                    let d = bsm::delta(s, k, t, self.risk_free_rate, sigma, is_call);
                    let g = bsm::gamma(s, k, t, self.risk_free_rate, sigma);
                    if d.is_finite() {
                        self.dte0_atm_delta.add(d.abs());
                    }
                    if g.is_finite() {
                        self.dte0_atm_gamma.add(g);
                    }
                    let v = bsm::vega(s, k, t, self.risk_free_rate, sigma);
                    if v.is_finite() {
                        self.dte0_atm_vega.add(v);
                    }
                }
            }
            _ => {
                self.iv_failed += 1;
            }
        }
    }

    fn end_of_day(&mut self, _day_index: u32) {
        self.n_days += 1;
    }

    fn reset_day(&mut self) {}

    fn finalize(&self) -> serde_json::Value {
        let mut iv_by_dte = serde_json::Map::new();
        for (i, label) in DTE_LABELS.iter().enumerate() {
            iv_by_dte.insert(
                label.to_string(),
                json!({
                    "mean": self.dte_bucket_iv[i].mean(),
                    "std": self.dte_bucket_iv[i].std(),
                    "count": self.dte_bucket_iv[i].count(),
                }),
            );
        }

        let iv_curve: Vec<serde_json::Value> = self
            .dte0_atm_iv_curve
            .finalize()
            .into_iter()
            .filter(|b| b.count > 0)
            .map(|b| {
                json!({
                    "minutes_since_open": b.minutes_since_open,
                    "mean_iv": b.mean,
                    "std_iv": b.std,
                    "count": b.count,
                })
            })
            .collect();

        json!({
            "tracker": "GreeksTracker",
            "n_days": self.n_days,
            "atm_quote_count": self.atm_quote_count,
            "iv_sample_interval": self.iv_sample_interval,
            "iv_computed": self.iv_computed,
            "iv_failed": self.iv_failed,
            "iv_success_rate": if self.iv_computed + self.iv_failed > 0 {
                self.iv_computed as f64 / (self.iv_computed + self.iv_failed) as f64
            } else { 0.0 },
            "dte0_atm_call_iv": self.dte0_atm_call_iv.summary(),
            "dte0_atm_put_iv": self.dte0_atm_put_iv.summary(),
            "dte0_atm_delta": self.dte0_atm_delta.summary(),
            "dte0_atm_gamma": self.dte0_atm_gamma.summary(),
            "dte0_atm_vega": self.dte0_atm_vega.summary(),
            "iv_by_dte": iv_by_dte,
            "intraday_dte0_atm_iv_curve": iv_curve,
        })
    }

    fn name(&self) -> &str {
        "GreeksTracker"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::helpers::*;
    use crate::options_math::moneyness::Moneyness;

    #[test]
    fn test_iv_computation_fires_for_atm() {
        let mut t = GreeksTracker::with_sample_interval(0.05, 1000, 1);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let e = make_quote_event(&c, 1.50, 1.60, 0, Moneyness::Atm);
        t.process_event(&e, 3);
        t.end_of_day(0);
        let r = t.finalize();
        let computed = r["iv_computed"].as_u64().unwrap();
        assert!(computed >= 1 || r["iv_failed"].as_u64().unwrap() >= 1,
            "Should attempt at least 1 IV computation");
    }

    #[test]
    fn test_multiple_iv_computations_with_sampling() {
        let mut t = GreeksTracker::with_sample_interval(0.05, 1000, 10);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        for _ in 0..500 {
            let e = make_quote_event(&c, 1.50, 1.60, 0, Moneyness::Atm);
            t.process_event(&e, 3);
        }
        t.end_of_day(0);
        let r = t.finalize();
        let computed = r["iv_computed"].as_u64().unwrap();
        let failed = r["iv_failed"].as_u64().unwrap();
        let total_attempts = computed + failed;
        assert_eq!(r["atm_quote_count"].as_u64().unwrap(), 500);
        assert_eq!(total_attempts, 50, "500 events / interval 10 = 50 attempts, got {}", total_attempts);
    }

    #[test]
    fn test_non_atm_ignored() {
        let mut t = GreeksTracker::new(0.05, 1000);
        t.begin_day(&make_day_context());
        let c = make_contract_call(250.0);
        let e = make_quote_event(&c, 0.01, 0.02, 0, Moneyness::DeepOtm);
        t.process_event(&e, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["iv_computed"].as_u64().unwrap() + r["iv_failed"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_finalize_structure() {
        let t = GreeksTracker::new(0.05, 1000);
        let r = t.finalize();
        assert_eq!(r["tracker"], "GreeksTracker");
        assert!(r.get("iv_by_dte").is_some());
        assert!(r.get("dte0_atm_call_iv").is_some());
        assert!(r.get("intraday_dte0_atm_iv_curve").is_some());
    }
}
