//! PutCallRatioTracker: put-call volume and quote activity ratios.

use serde_json::json;

use hft_statistics::statistics::{IntradayCurveAccumulator, WelfordAccumulator};

use crate::event::{DayContext, OptionsEvent};
use crate::OptionsTracker;


pub struct PutCallRatioTracker {
    utc_offset: i32,
    // Per-day put-call ratio tracking
    day_call_vol: u64,
    day_put_vol: u64,
    day_call_trades: u64,
    day_put_trades: u64,

    // 0DTE specific
    day_0dte_call_vol: u64,
    day_0dte_put_vol: u64,

    // Cross-day statistics
    pcr_volume_daily: WelfordAccumulator,
    pcr_trades_daily: WelfordAccumulator,
    pcr_0dte_daily: WelfordAccumulator,

    // Intraday put-call ratio curves (390-bin)
    // Track call and put volume separately, compute ratio in finalize
    intraday_call_vol: IntradayCurveAccumulator,
    intraday_put_vol: IntradayCurveAccumulator,
    intraday_0dte_call_vol: IntradayCurveAccumulator,
    intraday_0dte_put_vol: IntradayCurveAccumulator,

    n_days: u32,
}

impl Default for PutCallRatioTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PutCallRatioTracker {
    pub fn new() -> Self {
        Self {
            utc_offset: -5,
            day_call_vol: 0,
            day_put_vol: 0,
            day_call_trades: 0,
            day_put_trades: 0,
            day_0dte_call_vol: 0,
            day_0dte_put_vol: 0,
            pcr_volume_daily: WelfordAccumulator::new(),
            pcr_trades_daily: WelfordAccumulator::new(),
            pcr_0dte_daily: WelfordAccumulator::new(),
            intraday_call_vol: IntradayCurveAccumulator::new_rth_1min(),
            intraday_put_vol: IntradayCurveAccumulator::new_rth_1min(),
            intraday_0dte_call_vol: IntradayCurveAccumulator::new_rth_1min(),
            intraday_0dte_put_vol: IntradayCurveAccumulator::new_rth_1min(),
            n_days: 0,
        }
    }
}

impl OptionsTracker for PutCallRatioTracker {
    fn begin_day(&mut self, ctx: &DayContext) {
        self.utc_offset = ctx.utc_offset;
    }

    fn process_event(&mut self, event: &OptionsEvent, _regime: u8) {
        if !event.is_trade() || event.trade_size == 0 {
            return;
        }

        let vol = event.trade_size as f64;
        let ts = event.ts_event;

        if event.is_call() {
            self.day_call_vol += event.trade_size as u64;
            self.day_call_trades += 1;
            self.intraday_call_vol.add(ts, vol, self.utc_offset);
            if event.is_zero_dte() {
                self.day_0dte_call_vol += event.trade_size as u64;
                self.intraday_0dte_call_vol.add(ts, vol, self.utc_offset);
            }
        } else {
            self.day_put_vol += event.trade_size as u64;
            self.day_put_trades += 1;
            self.intraday_put_vol.add(ts, vol, self.utc_offset);
            if event.is_zero_dte() {
                self.day_0dte_put_vol += event.trade_size as u64;
                self.intraday_0dte_put_vol.add(ts, vol, self.utc_offset);
            }
        }
    }

    fn end_of_day(&mut self, _day_index: u32) {
        if self.day_call_vol > 0 {
            let pcr_vol = self.day_put_vol as f64 / self.day_call_vol as f64;
            self.pcr_volume_daily.update(pcr_vol);
        }
        if self.day_call_trades > 0 {
            let pcr_trades = self.day_put_trades as f64 / self.day_call_trades as f64;
            self.pcr_trades_daily.update(pcr_trades);
        }
        if self.day_0dte_call_vol > 0 {
            let pcr_0dte = self.day_0dte_put_vol as f64 / self.day_0dte_call_vol as f64;
            self.pcr_0dte_daily.update(pcr_0dte);
        }
        self.n_days += 1;
    }

    fn reset_day(&mut self) {
        self.day_call_vol = 0;
        self.day_put_vol = 0;
        self.day_call_trades = 0;
        self.day_put_trades = 0;
        self.day_0dte_call_vol = 0;
        self.day_0dte_put_vol = 0;
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
                            "count": b.count,
                        })
                    })
                    .collect()
            };

        json!({
            "tracker": "PutCallRatioTracker",
            "n_days": self.n_days,
            "pcr_volume_daily": {
                "mean": self.pcr_volume_daily.mean(),
                "std": self.pcr_volume_daily.std(),
                "count": self.pcr_volume_daily.count(),
            },
            "pcr_trades_daily": {
                "mean": self.pcr_trades_daily.mean(),
                "std": self.pcr_trades_daily.std(),
            },
            "pcr_0dte_daily": {
                "mean": self.pcr_0dte_daily.mean(),
                "std": self.pcr_0dte_daily.std(),
                "count": self.pcr_0dte_daily.count(),
            },
            "intraday_call_volume": make_curve(&self.intraday_call_vol, "mean_volume"),
            "intraday_put_volume": make_curve(&self.intraday_put_vol, "mean_volume"),
            "intraday_0dte_call_volume": make_curve(&self.intraday_0dte_call_vol, "mean_volume"),
            "intraday_0dte_put_volume": make_curve(&self.intraday_0dte_put_vol, "mean_volume"),
        })
    }

    fn name(&self) -> &str {
        "PutCallRatioTracker"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::helpers::*;
    use crate::options_math::moneyness::Moneyness;

    #[test]
    fn test_pcr_computation() {
        let mut t = PutCallRatioTracker::new();
        t.begin_day(&make_day_context());
        let call = make_contract_call(190.0);
        let put = make_contract_put(190.0);
        let tc = make_trade_event(&call, 1.0, 100, 0.95, 1.05, 0, Moneyness::Atm);
        let tp = make_trade_event(&put, 0.8, 50, 0.75, 0.85, 0, Moneyness::Atm);
        t.process_event(&tc, 3);
        t.process_event(&tp, 3);
        t.end_of_day(0);
        let r = t.finalize();
        let pcr = r["pcr_volume_daily"]["mean"].as_f64().unwrap();
        assert!((pcr - 0.5).abs() < 1e-10, "PCR: 50/100 = 0.5, got {}", pcr);
    }

    #[test]
    fn test_no_calls_no_pcr() {
        let mut t = PutCallRatioTracker::new();
        t.begin_day(&make_day_context());
        let put = make_contract_put(190.0);
        let tp = make_trade_event(&put, 0.8, 50, 0.75, 0.85, 0, Moneyness::Atm);
        t.process_event(&tp, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["pcr_volume_daily"]["count"], 0, "No PCR when no call volume");
    }

    #[test]
    fn test_day_rollover() {
        let mut t = PutCallRatioTracker::new();
        t.begin_day(&make_day_context());
        let call = make_contract_call(190.0);
        let tc = make_trade_event(&call, 1.0, 100, 0.95, 1.05, 0, Moneyness::Atm);
        t.process_event(&tc, 3);
        t.end_of_day(0);
        t.reset_day();
        t.begin_day(&make_day_context());
        t.end_of_day(1);
        let r = t.finalize();
        assert_eq!(r["n_days"], 2);
    }

    #[test]
    fn test_finalize_structure() {
        let t = PutCallRatioTracker::new();
        let r = t.finalize();
        assert_eq!(r["tracker"], "PutCallRatioTracker");
        assert!(r.get("pcr_volume_daily").is_some());
        assert!(r.get("pcr_0dte_daily").is_some());
    }
}
