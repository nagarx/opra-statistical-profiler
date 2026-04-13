//! OptionsEffectiveSpreadTracker: realized execution cost analysis.
//!
//! Computes the gap between *quoted* spread (what the BBO shows) and
//! *effective* spread (what traders actually pay). Key metrics:
//!
//! - **Effective spread**: 2 × |trade_price − mid| for each trade
//! - **Trade-through rate**: fraction of trades outside the BBO
//! - **Realized spread per size bucket**: cost varies with trade size
//! - **Intraday effective spread curve**: minute-level for the trading window
//!
//! All bucketed by DTE (0DTE focus) and moneyness (ATM focus).
//!
//! This tells us the gap between the OPRA-quoted spread ($0.03 median)
//! and actual execution cost — critical for limit vs market order decisions.

use serde_json::json;

use hft_statistics::statistics::{IntradayCurveAccumulator, WelfordAccumulator};

use crate::event::{DayContext, OptionsEvent};
use crate::report_utils::{finalize_curve, DTE_LABELS, MONEYNESS_LABELS, dte_bucket_index, moneyness_index};
use crate::OptionsTracker;

const SIZE_BUCKET_BOUNDS: [u32; 5] = [1, 5, 10, 50, 100];
const SIZE_BUCKET_LABELS: [&str; 6] = ["1", "2-5", "6-10", "11-50", "51-100", "100+"];
const N_SIZE_BUCKETS: usize = 6;

const N_DTE_BUCKETS: usize = 4;
const N_MONEYNESS_BUCKETS: usize = 5;

#[inline]
fn size_bucket_index(size: u32) -> usize {
    for (i, &bound) in SIZE_BUCKET_BOUNDS.iter().enumerate() {
        if size <= bound {
            return i;
        }
    }
    N_SIZE_BUCKETS - 1
}

pub struct OptionsEffectiveSpreadTracker {
    utc_offset: i32,

    effective_spread_by_dte_moneyness: [[WelfordAccumulator; N_MONEYNESS_BUCKETS]; N_DTE_BUCKETS],
    quoted_spread_by_dte_moneyness: [[WelfordAccumulator; N_MONEYNESS_BUCKETS]; N_DTE_BUCKETS],

    effective_spread_by_size: [WelfordAccumulator; N_SIZE_BUCKETS],

    trades_inside_bbo: u64,
    trades_at_bbo: u64,
    trades_outside_bbo: u64,

    trades_inside_bbo_0dte_atm: u64,
    trades_at_bbo_0dte_atm: u64,
    trades_outside_bbo_0dte_atm: u64,

    intraday_effective_spread_0dte_atm: IntradayCurveAccumulator,
    intraday_quoted_spread_0dte_atm: IntradayCurveAccumulator,

    total_trades: u64,
    n_days: u32,
}

impl Default for OptionsEffectiveSpreadTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl OptionsEffectiveSpreadTracker {
    pub fn new() -> Self {
        Self {
            utc_offset: -5,
            effective_spread_by_dte_moneyness: std::array::from_fn(|_| {
                std::array::from_fn(|_| WelfordAccumulator::new())
            }),
            quoted_spread_by_dte_moneyness: std::array::from_fn(|_| {
                std::array::from_fn(|_| WelfordAccumulator::new())
            }),
            effective_spread_by_size: std::array::from_fn(|_| WelfordAccumulator::new()),
            trades_inside_bbo: 0,
            trades_at_bbo: 0,
            trades_outside_bbo: 0,
            trades_inside_bbo_0dte_atm: 0,
            trades_at_bbo_0dte_atm: 0,
            trades_outside_bbo_0dte_atm: 0,
            intraday_effective_spread_0dte_atm: IntradayCurveAccumulator::new_rth_1min(),
            intraday_quoted_spread_0dte_atm: IntradayCurveAccumulator::new_rth_1min(),
            total_trades: 0,
            n_days: 0,
        }
    }
}

impl OptionsTracker for OptionsEffectiveSpreadTracker {
    fn begin_day(&mut self, ctx: &DayContext) {
        self.utc_offset = ctx.utc_offset;
    }

    fn process_event(&mut self, event: &OptionsEvent, _regime: u8) {
        if !event.is_trade() || event.trade_size == 0 || !event.has_valid_bbo() {
            return;
        }

        let trade_price = event.trade_price;
        if !trade_price.is_finite() || trade_price <= 0.0 {
            return;
        }

        let mid = event.option_mid();
        if !mid.is_finite() || mid <= 0.0 {
            return;
        }

        let bid = event.bid_px;
        let ask = event.ask_px;
        let quoted_spread = ask - bid;
        let effective_spread = 2.0 * (trade_price - mid).abs();

        let dte_idx = dte_bucket_index(event.dte);
        let mon_idx = moneyness_index(event.moneyness);
        let size_idx = size_bucket_index(event.trade_size);

        self.effective_spread_by_dte_moneyness[dte_idx][mon_idx].update(effective_spread);
        self.quoted_spread_by_dte_moneyness[dte_idx][mon_idx].update(quoted_spread);
        self.effective_spread_by_size[size_idx].update(effective_spread);
        self.total_trades += 1;

        let inside = trade_price > bid && trade_price < ask;
        let at_bbo = (trade_price - bid).abs() < 1e-9 || (trade_price - ask).abs() < 1e-9;

        if inside {
            self.trades_inside_bbo += 1;
        } else if at_bbo {
            self.trades_at_bbo += 1;
        } else {
            self.trades_outside_bbo += 1;
        }

        if event.is_zero_dte() && event.is_atm() {
            if inside {
                self.trades_inside_bbo_0dte_atm += 1;
            } else if at_bbo {
                self.trades_at_bbo_0dte_atm += 1;
            } else {
                self.trades_outside_bbo_0dte_atm += 1;
            }

            let ts = event.ts_event;
            self.intraday_effective_spread_0dte_atm
                .add(ts, effective_spread, self.utc_offset);
            self.intraday_quoted_spread_0dte_atm
                .add(ts, quoted_spread, self.utc_offset);
        }
    }

    fn end_of_day(&mut self, _day_index: u32) {
        self.n_days += 1;
    }

    fn reset_day(&mut self) {}

    fn finalize(&self) -> serde_json::Value {
        let mut dte_moneyness = serde_json::Map::new();
        for (d, dte_label) in DTE_LABELS.iter().enumerate() {
            let mut moneyness_map = serde_json::Map::new();
            for (m, mon_label) in MONEYNESS_LABELS.iter().enumerate() {
                let eff = &self.effective_spread_by_dte_moneyness[d][m];
                let quot = &self.quoted_spread_by_dte_moneyness[d][m];
                if eff.count() > 0 {
                    moneyness_map.insert(
                        mon_label.to_string(),
                        json!({
                            "effective_spread": {
                                "mean": eff.mean(),
                                "std": eff.std(),
                                "count": eff.count(),
                            },
                            "quoted_spread": {
                                "mean": quot.mean(),
                                "std": quot.std(),
                                "count": quot.count(),
                            },
                            "effective_vs_quoted_ratio": if quot.mean() > 1e-12 {
                                eff.mean() / quot.mean()
                            } else {
                                f64::NAN
                            },
                        }),
                    );
                }
            }
            if !moneyness_map.is_empty() {
                dte_moneyness.insert(dte_label.to_string(), serde_json::Value::Object(moneyness_map));
            }
        }

        let size_buckets: Vec<serde_json::Value> = self
            .effective_spread_by_size
            .iter()
            .enumerate()
            .filter(|(_, acc)| acc.count() > 0)
            .map(|(i, acc)| {
                json!({
                    "size_bucket": SIZE_BUCKET_LABELS[i],
                    "effective_spread_mean": acc.mean(),
                    "effective_spread_std": acc.std(),
                    "count": acc.count(),
                })
            })
            .collect();

        let total_all = self.trades_inside_bbo + self.trades_at_bbo + self.trades_outside_bbo;
        let total_0dte_atm =
            self.trades_inside_bbo_0dte_atm + self.trades_at_bbo_0dte_atm + self.trades_outside_bbo_0dte_atm;

        json!({
            "tracker": "OptionsEffectiveSpreadTracker",
            "n_days": self.n_days,
            "total_trades": self.total_trades,
            "trade_location": {
                "all": {
                    "inside_bbo": self.trades_inside_bbo,
                    "at_bbo": self.trades_at_bbo,
                    "outside_bbo": self.trades_outside_bbo,
                    "trade_through_rate": if total_all > 0 {
                        self.trades_outside_bbo as f64 / total_all as f64
                    } else { 0.0 },
                    "price_improvement_rate": if total_all > 0 {
                        self.trades_inside_bbo as f64 / total_all as f64
                    } else { 0.0 },
                },
                "0dte_atm": {
                    "inside_bbo": self.trades_inside_bbo_0dte_atm,
                    "at_bbo": self.trades_at_bbo_0dte_atm,
                    "outside_bbo": self.trades_outside_bbo_0dte_atm,
                    "trade_through_rate": if total_0dte_atm > 0 {
                        self.trades_outside_bbo_0dte_atm as f64 / total_0dte_atm as f64
                    } else { 0.0 },
                    "price_improvement_rate": if total_0dte_atm > 0 {
                        self.trades_inside_bbo_0dte_atm as f64 / total_0dte_atm as f64
                    } else { 0.0 },
                },
            },
            "by_dte_moneyness": dte_moneyness,
            "by_size_bucket": size_buckets,
            "intraday_effective_spread_0dte_atm": finalize_curve(
                &self.intraday_effective_spread_0dte_atm,
                "mean_effective_spread",
            ),
            "intraday_quoted_spread_0dte_atm": finalize_curve(
                &self.intraday_quoted_spread_0dte_atm,
                "mean_quoted_spread",
            ),
        })
    }

    fn name(&self) -> &str {
        "OptionsEffectiveSpreadTracker"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_helpers::helpers::*;
    use crate::options_math::moneyness::Moneyness;

    #[test]
    fn test_size_bucket_index() {
        assert_eq!(size_bucket_index(1), 0);   // "1"
        assert_eq!(size_bucket_index(3), 1);   // "2-5"
        assert_eq!(size_bucket_index(5), 1);   // "2-5"
        assert_eq!(size_bucket_index(10), 2);  // "6-10"
        assert_eq!(size_bucket_index(50), 3);  // "11-50"
        assert_eq!(size_bucket_index(100), 4); // "51-100"
        assert_eq!(size_bucket_index(500), 5); // "100+"
    }

    #[test]
    fn test_effective_spread_formula() {
        // Effective spread = 2 * |trade_price - mid| (Lee & Ready 1991)
        // bid=1.00, ask=1.10 => mid=1.05, trade=1.08 => eff=2*|1.08-1.05|=0.06
        let mut t = OptionsEffectiveSpreadTracker::new();
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let e = make_trade_event(&c, 1.08, 5, 1.00, 1.10, 0, Moneyness::Atm);
        t.process_event(&e, 0);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["total_trades"], 1);
        let atm = &r["by_dte_moneyness"]["0dte"]["atm"];
        let eff_mean = atm["effective_spread"]["mean"].as_f64().unwrap();
        let quot_mean = atm["quoted_spread"]["mean"].as_f64().unwrap();
        assert!(
            (eff_mean - 0.06).abs() < 1e-10,
            "Effective spread: 2*|1.08-1.05| = 0.06, got {}", eff_mean
        );
        assert!(
            (quot_mean - 0.10).abs() < 1e-10,
            "Quoted spread: 1.10-1.00 = 0.10, got {}", quot_mean
        );
    }

    #[test]
    fn test_trade_through_classification() {
        let mut t = OptionsEffectiveSpreadTracker::new();
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        // Inside BBO: trade at 1.05, between bid=1.00 and ask=1.10
        let inside = make_trade_event(&c, 1.05, 1, 1.00, 1.10, 0, Moneyness::Atm);
        // At BBO: trade at exactly bid=1.00
        let at_bbo = make_trade_event(&c, 1.00, 1, 1.00, 1.10, 0, Moneyness::Atm);
        // Outside BBO: trade at 1.15, above ask=1.10
        let outside = make_trade_event(&c, 1.15, 1, 1.00, 1.10, 0, Moneyness::Atm);
        t.process_event(&inside, 0);
        t.process_event(&at_bbo, 0);
        t.process_event(&outside, 0);
        t.end_of_day(0);
        let r = t.finalize();
        let loc = &r["trade_location"]["all"];
        assert_eq!(loc["inside_bbo"], 1);
        assert_eq!(loc["at_bbo"], 1);
        assert_eq!(loc["outside_bbo"], 1);
        let ttr = loc["trade_through_rate"].as_f64().unwrap();
        assert!(
            (ttr - 1.0 / 3.0).abs() < 1e-10,
            "Trade-through rate: 1/3, got {}", ttr
        );
    }

    #[test]
    fn test_quotes_ignored() {
        let mut t = OptionsEffectiveSpreadTracker::new();
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let q = make_quote_event(&c, 1.00, 1.10, 0, Moneyness::Atm);
        t.process_event(&q, 0);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["total_trades"], 0, "Quote events should not be counted as trades");
    }

    #[test]
    fn test_zero_size_trade_ignored() {
        let mut t = OptionsEffectiveSpreadTracker::new();
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let e = make_trade_event(&c, 1.05, 0, 1.00, 1.10, 0, Moneyness::Atm);
        t.process_event(&e, 0);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["total_trades"], 0, "Zero-size trades should be filtered");
    }

    #[test]
    fn test_finalize_structure() {
        let t = OptionsEffectiveSpreadTracker::new();
        let r = t.finalize();
        assert_eq!(r["tracker"], "OptionsEffectiveSpreadTracker");
        assert!(r.get("trade_location").is_some());
        assert!(r.get("by_dte_moneyness").is_some());
        assert!(r.get("by_size_bucket").is_some());
        assert!(r.get("intraday_effective_spread_0dte_atm").is_some());
        assert!(r.get("intraday_quoted_spread_0dte_atm").is_some());
    }
}
