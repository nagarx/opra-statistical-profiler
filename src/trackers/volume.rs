//! VolumeTracker: trade volume distribution by DTE, moneyness, and time-of-day.

use serde_json::json;

use hft_statistics::statistics::{
    IntradayCurveAccumulator, StreamingDistribution, WelfordAccumulator,
};

use crate::event::{DayContext, OptionsEvent};
use crate::report_utils::{DTE_LABELS, MONEYNESS_LABELS};
use crate::OptionsTracker;


pub struct VolumeTracker {
    utc_offset: i32,
    total_trades: u64,
    total_volume: u64,
    call_volume: u64,
    put_volume: u64,

    trade_size_dist: StreamingDistribution,
    call_trade_size: StreamingDistribution,
    put_trade_size: StreamingDistribution,

    // Per DTE bucket: [0dte, 1dte, weekly, other]
    dte_volume: [u64; 4],
    dte_trades: [u64; 4],

    // Per moneyness: [deep_itm, itm, atm, otm, deep_otm]
    moneyness_volume: [u64; 5],

    intraday_all_volume: IntradayCurveAccumulator,
    intraday_0dte_volume: IntradayCurveAccumulator,

    daily_volume: WelfordAccumulator,
    daily_trades: WelfordAccumulator,
    day_volume: u64,
    day_trades: u64,

    n_days: u32,
}

impl VolumeTracker {
    pub fn new(reservoir_capacity: usize) -> Self {
        Self {
            utc_offset: -5,
            total_trades: 0,
            total_volume: 0,
            call_volume: 0,
            put_volume: 0,
            trade_size_dist: StreamingDistribution::new(reservoir_capacity),
            call_trade_size: StreamingDistribution::new(reservoir_capacity),
            put_trade_size: StreamingDistribution::new(reservoir_capacity),
            dte_volume: [0; 4],
            dte_trades: [0; 4],
            moneyness_volume: [0; 5],
            intraday_all_volume: IntradayCurveAccumulator::new_rth_1min(),
            intraday_0dte_volume: IntradayCurveAccumulator::new_rth_1min(),
            daily_volume: WelfordAccumulator::new(),
            daily_trades: WelfordAccumulator::new(),
            day_volume: 0,
            day_trades: 0,
            n_days: 0,
        }
    }

}

impl OptionsTracker for VolumeTracker {
    fn begin_day(&mut self, ctx: &DayContext) {
        self.utc_offset = ctx.utc_offset;
    }

    fn process_event(&mut self, event: &OptionsEvent, _regime: u8) {
        if !event.is_trade() || event.trade_size == 0 {
            return;
        }

        let vol = event.trade_size as u64;
        let size_f = event.trade_size as f64;

        self.total_trades += 1;
        self.total_volume += vol;
        self.day_trades += 1;
        self.day_volume += vol;

        self.trade_size_dist.add(size_f);

        if event.is_call() {
            self.call_volume += vol;
            self.call_trade_size.add(size_f);
        } else {
            self.put_volume += vol;
            self.put_trade_size.add(size_f);
        }

        let di = crate::report_utils::dte_bucket_index(event.dte);
        self.dte_volume[di] += vol;
        self.dte_trades[di] += 1;

        let mi = crate::report_utils::moneyness_index(event.moneyness);
        self.moneyness_volume[mi] += vol;

        self.intraday_all_volume
            .add(event.ts_event, size_f, self.utc_offset);

        if event.is_zero_dte() {
            self.intraday_0dte_volume
                .add(event.ts_event, size_f, self.utc_offset);
        }
    }

    fn end_of_day(&mut self, _day_index: u32) {
        self.daily_volume.update(self.day_volume as f64);
        self.daily_trades.update(self.day_trades as f64);
        self.n_days += 1;
    }

    fn reset_day(&mut self) {
        self.day_volume = 0;
        self.day_trades = 0;
    }

    fn finalize(&self) -> serde_json::Value {
        let pcr_volume = if self.call_volume > 0 {
            self.put_volume as f64 / self.call_volume as f64
        } else {
            f64::NAN
        };

        let mut dte_map = serde_json::Map::new();
        for (i, label) in DTE_LABELS.iter().enumerate() {
            dte_map.insert(
                label.to_string(),
                json!({"volume": self.dte_volume[i], "trades": self.dte_trades[i]}),
            );
        }

        let mut money_map = serde_json::Map::new();
        for (i, label) in MONEYNESS_LABELS.iter().enumerate() {
            money_map.insert(label.to_string(), json!(self.moneyness_volume[i]));
        }

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
            "tracker": "VolumeTracker",
            "n_days": self.n_days,
            "total_trades": self.total_trades,
            "total_volume": self.total_volume,
            "call_volume": self.call_volume,
            "put_volume": self.put_volume,
            "put_call_ratio_volume": pcr_volume,
            "dte0_volume_share_pct": if self.total_volume > 0 {
                self.dte_volume[0] as f64 / self.total_volume as f64 * 100.0
            } else { 0.0 },
            "trade_size_distribution": self.trade_size_dist.summary(),
            "call_trade_size": self.call_trade_size.summary(),
            "put_trade_size": self.put_trade_size.summary(),
            "volume_by_dte": dte_map,
            "volume_by_moneyness": money_map,
            "daily_volume": {
                "mean": self.daily_volume.mean(),
                "std": self.daily_volume.std(),
            },
            "daily_trades": {
                "mean": self.daily_trades.mean(),
                "std": self.daily_trades.std(),
            },
            "intraday_all_volume_curve": make_curve(&self.intraday_all_volume, "mean_trade_size"),
            "intraday_0dte_volume_curve": make_curve(&self.intraday_0dte_volume, "mean_trade_size"),
        })
    }

    fn name(&self) -> &str {
        "VolumeTracker"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::helpers::*;
    use crate::options_math::moneyness::Moneyness;

    #[test]
    fn test_counts_trades_only() {
        let mut t = VolumeTracker::new(1000);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let q = make_quote_event(&c, 1.0, 1.05, 0, Moneyness::Atm);
        let tr = make_trade_event(&c, 1.02, 50, 1.0, 1.05, 0, Moneyness::Atm);
        t.process_event(&q, 3);
        t.process_event(&tr, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["total_trades"], 1);
        assert_eq!(r["total_volume"], 50);
    }

    #[test]
    fn test_pcr_zero_calls() {
        let mut t = VolumeTracker::new(1000);
        t.begin_day(&make_day_context());
        let p = make_contract_put(190.0);
        let tr = make_trade_event(&p, 0.80, 10, 0.75, 0.85, 0, Moneyness::Atm);
        t.process_event(&tr, 3);
        t.end_of_day(0);
        let r = t.finalize();
        let pcr = r["put_call_ratio_volume"].as_f64();
        assert!(pcr.is_none() || pcr.unwrap().is_nan() || pcr.unwrap().is_infinite(),
            "PCR should be NaN/null when no calls");
    }

    #[test]
    fn test_dte_bucketing() {
        let mut t = VolumeTracker::new(1000);
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let t0 = make_trade_event(&c, 1.0, 10, 0.95, 1.05, 0, Moneyness::Atm);
        let t7 = make_trade_event(&c, 1.0, 20, 0.95, 1.05, 7, Moneyness::Atm);
        t.process_event(&t0, 3);
        t.process_event(&t7, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["volume_by_dte"]["0dte"]["volume"], 10);
        assert_eq!(r["volume_by_dte"]["2_7dte"]["volume"], 20);
    }

    #[test]
    fn test_finalize_structure() {
        let t = VolumeTracker::new(1000);
        let r = t.finalize();
        assert_eq!(r["tracker"], "VolumeTracker");
        assert!(r.get("volume_by_dte").is_some());
        assert!(r.get("put_call_ratio_volume").is_some());
    }
}
