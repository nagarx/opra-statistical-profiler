//! ZeroDteTracker: 0DTE ATM-focused analysis for strategy validation.
//!
//! The most critical tracker for the 0DTE options strategy. Produces
//! per-minute intraday curves for ATM 0DTE contracts:
//! - Spread (bid-ask) by minute
//! - Premium (option mid) by minute → theta decay curve
//! - Trade volume by minute
//! - Trade count by minute

use serde_json::json;

use hft_statistics::statistics::{
    IntradayCurveAccumulator, StreamingDistribution, WelfordAccumulator,
};

use crate::event::{DayContext, OptionsEvent};
use crate::OptionsTracker;


pub struct ZeroDteTracker {
    utc_offset: i32,
    // Per-minute intraday curves (390 bins, RTH 09:30-16:00)
    atm_call_spread_curve: IntradayCurveAccumulator,
    atm_put_spread_curve: IntradayCurveAccumulator,
    atm_call_mid_curve: IntradayCurveAccumulator,
    atm_put_mid_curve: IntradayCurveAccumulator,
    atm_trade_volume_curve: IntradayCurveAccumulator,
    atm_trade_count_curve: IntradayCurveAccumulator,

    // Aggregate statistics
    atm_call_spread: StreamingDistribution,
    atm_put_spread: StreamingDistribution,
    atm_call_premium: StreamingDistribution,
    atm_put_premium: StreamingDistribution,
    atm_trade_size: StreamingDistribution,
    atm_bid_ask_imbalance: StreamingDistribution,

    // Per-day aggregates
    day_trade_count: u64,
    day_trade_volume: u64,
    day_0dte_atm_events: u64,
    trades_per_day: WelfordAccumulator,
    volume_per_day: WelfordAccumulator,

    n_days: u32,
    n_0dte_days: u32,
    total_0dte_atm_events: u64,
    total_0dte_atm_trades: u64,
}

impl ZeroDteTracker {
    pub fn new(reservoir_capacity: usize) -> Self {
        Self {
            utc_offset: -5,
            atm_call_spread_curve: IntradayCurveAccumulator::new_rth_1min(),
            atm_put_spread_curve: IntradayCurveAccumulator::new_rth_1min(),
            atm_call_mid_curve: IntradayCurveAccumulator::new_rth_1min(),
            atm_put_mid_curve: IntradayCurveAccumulator::new_rth_1min(),
            atm_trade_volume_curve: IntradayCurveAccumulator::new_rth_1min(),
            atm_trade_count_curve: IntradayCurveAccumulator::new_rth_1min(),
            atm_call_spread: StreamingDistribution::new(reservoir_capacity),
            atm_put_spread: StreamingDistribution::new(reservoir_capacity),
            atm_call_premium: StreamingDistribution::new(reservoir_capacity),
            atm_put_premium: StreamingDistribution::new(reservoir_capacity),
            atm_trade_size: StreamingDistribution::new(reservoir_capacity),
            atm_bid_ask_imbalance: StreamingDistribution::new(reservoir_capacity),
            day_trade_count: 0,
            day_trade_volume: 0,
            day_0dte_atm_events: 0,
            trades_per_day: WelfordAccumulator::new(),
            volume_per_day: WelfordAccumulator::new(),
            n_days: 0,
            n_0dte_days: 0,
            total_0dte_atm_events: 0,
            total_0dte_atm_trades: 0,
        }
    }
}

impl OptionsTracker for ZeroDteTracker {
    fn begin_day(&mut self, ctx: &DayContext) {
        self.utc_offset = ctx.utc_offset;
    }

    fn process_event(&mut self, event: &OptionsEvent, _regime: u8) {
        if !event.is_zero_dte() || !event.is_atm() {
            return;
        }

        self.total_0dte_atm_events += 1;
        self.day_0dte_atm_events += 1;

        let ts = event.ts_event;
        let is_call = event.is_call();

        if event.has_valid_bbo() {
            let spread = event.spread();
            let mid = event.option_mid();

            let total_sz = event.bid_sz as f64 + event.ask_sz as f64;
            if total_sz > 0.0 {
                let imbalance = (event.bid_sz as f64 - event.ask_sz as f64) / total_sz;
                self.atm_bid_ask_imbalance.add(imbalance);
            }

            if spread.is_finite() {
                if is_call {
                    self.atm_call_spread.add(spread);
                    self.atm_call_spread_curve.add(ts, spread, self.utc_offset);
                } else {
                    self.atm_put_spread.add(spread);
                    self.atm_put_spread_curve.add(ts, spread, self.utc_offset);
                }
            }

            if mid.is_finite() && mid > 0.0 {
                if is_call {
                    self.atm_call_premium.add(mid);
                    self.atm_call_mid_curve.add(ts, mid, self.utc_offset);
                } else {
                    self.atm_put_premium.add(mid);
                    self.atm_put_mid_curve.add(ts, mid, self.utc_offset);
                }
            }
        }

        if event.is_trade() && event.trade_size > 0 {
            self.total_0dte_atm_trades += 1;
            self.day_trade_count += 1;
            self.day_trade_volume += event.trade_size as u64;
            self.atm_trade_size.add(event.trade_size as f64);
            self.atm_trade_volume_curve
                .add(ts, event.trade_size as f64, self.utc_offset);
            self.atm_trade_count_curve.add(ts, 1.0, self.utc_offset);
        }
    }

    fn end_of_day(&mut self, _day_index: u32) {
        if self.day_0dte_atm_events > 0 {
            self.n_0dte_days += 1;
            self.trades_per_day.update(self.day_trade_count as f64);
            self.volume_per_day.update(self.day_trade_volume as f64);
        }
        self.n_days += 1;
    }

    fn reset_day(&mut self) {
        self.day_trade_count = 0;
        self.day_trade_volume = 0;
        self.day_0dte_atm_events = 0;
    }

    fn finalize(&self) -> serde_json::Value {
        let make_curve = |acc: &IntradayCurveAccumulator, value_key: &str| -> Vec<serde_json::Value> {
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
        };

        json!({
            "tracker": "ZeroDteTracker",
            "n_days": self.n_days,
            "n_0dte_days": self.n_0dte_days,
            "total_0dte_atm_events": self.total_0dte_atm_events,
            "total_0dte_atm_trades": self.total_0dte_atm_trades,
            "atm_call_spread": self.atm_call_spread.summary(),
            "atm_put_spread": self.atm_put_spread.summary(),
            "atm_call_premium": self.atm_call_premium.summary(),
            "atm_put_premium": self.atm_put_premium.summary(),
            "atm_trade_size": self.atm_trade_size.summary(),
            "atm_bid_ask_imbalance": self.atm_bid_ask_imbalance.summary(),
            "trades_per_0dte_day": {
                "mean": self.trades_per_day.mean(),
                "std": self.trades_per_day.std(),
                "count": self.trades_per_day.count(),
            },
            "volume_per_0dte_day": {
                "mean": self.volume_per_day.mean(),
                "std": self.volume_per_day.std(),
            },
            "intraday_atm_call_spread": make_curve(&self.atm_call_spread_curve, "mean_spread"),
            "intraday_atm_put_spread": make_curve(&self.atm_put_spread_curve, "mean_spread"),
            "intraday_atm_call_premium": make_curve(&self.atm_call_mid_curve, "mean_premium"),
            "intraday_atm_put_premium": make_curve(&self.atm_put_mid_curve, "mean_premium"),
            "intraday_atm_trade_volume": make_curve(&self.atm_trade_volume_curve, "mean_volume"),
            "intraday_atm_trade_count": make_curve(&self.atm_trade_count_curve, "trade_count"),
        })
    }

    fn name(&self) -> &str {
        "ZeroDteTracker"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::helpers::*;
    use crate::options_math::moneyness::Moneyness;

    #[test]
    fn test_filters_non_0dte() {
        let mut t = ZeroDteTracker::new(1000);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let weekly = make_quote_event(&c, 5.0, 5.10, 7, Moneyness::Atm);
        t.process_event(&weekly, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["total_0dte_atm_events"], 0);
    }

    #[test]
    fn test_filters_non_atm() {
        let mut t = ZeroDteTracker::new(1000);
        t.begin_day(&make_day_context());
        let c = make_contract_call(250.0);
        let otm = make_quote_event(&c, 0.01, 0.02, 0, Moneyness::DeepOtm);
        t.process_event(&otm, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["total_0dte_atm_events"], 0);
    }

    #[test]
    fn test_counts_0dte_atm() {
        let mut t = ZeroDteTracker::new(1000);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let e = make_quote_event(&c, 1.00, 1.05, 0, Moneyness::Atm);
        t.process_event(&e, 3);
        t.process_event(&e, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["total_0dte_atm_events"], 2);
        assert_eq!(r["n_0dte_days"], 1);
    }

    #[test]
    fn test_call_put_separation() {
        let mut t = ZeroDteTracker::new(1000);
        t.begin_day(&make_day_context());
        let call = make_contract_call(190.0);
        let put = make_contract_put(190.0);
        let qc = make_quote_event(&call, 1.00, 1.05, 0, Moneyness::Atm);
        let qp = make_quote_event(&put, 0.80, 0.85, 0, Moneyness::Atm);
        t.process_event(&qc, 3);
        t.process_event(&qp, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["atm_call_spread"]["count"], 1);
        assert_eq!(r["atm_put_spread"]["count"], 1);
    }
}
