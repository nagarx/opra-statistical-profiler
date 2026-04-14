//! QualityTracker: data quality and event count statistics.
//!
//! Tracks total events, quote/trade breakdown, unique contracts,
//! events per second, sentinel ratios, and per-DTE bucket counts.

use ahash::AHashSet;
use serde_json::json;

use hft_statistics::statistics::WelfordAccumulator;

use crate::event::{DayContext, OptionsEvent};
use crate::report_utils::{dte_bucket_index, DTE_LABELS};
use crate::OptionsTracker;

pub struct QualityTracker {
    total_events: u64,
    quote_events: u64,
    trade_events: u64,
    sentinel_events: u64,
    unique_contracts_day: AHashSet<u32>,
    unique_contracts_total: AHashSet<u32>,
    events_per_day: WelfordAccumulator,
    trades_per_day: WelfordAccumulator,
    contracts_per_day: WelfordAccumulator,
    day_events: u64,
    day_trades: u64,
    n_days: u32,
    /// Per-DTE-bucket event counts, indexed by `report_utils::dte_bucket_index`.
    /// Labels in `DTE_LABELS`: ["0dte", "1dte", "2_7dte", "other"].
    dte_events: [u64; 4],
}

impl Default for QualityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl QualityTracker {
    pub fn new() -> Self {
        Self {
            total_events: 0,
            quote_events: 0,
            trade_events: 0,
            sentinel_events: 0,
            unique_contracts_day: AHashSet::new(),
            unique_contracts_total: AHashSet::new(),
            events_per_day: WelfordAccumulator::new(),
            trades_per_day: WelfordAccumulator::new(),
            contracts_per_day: WelfordAccumulator::new(),
            day_events: 0,
            day_trades: 0,
            n_days: 0,
            dte_events: [0; 4],
        }
    }
}

impl OptionsTracker for QualityTracker {
    fn begin_day(&mut self, _ctx: &DayContext) {}

    fn process_event(&mut self, event: &OptionsEvent, _regime: u8) {
        self.total_events += 1;
        self.day_events += 1;
        self.unique_contracts_day.insert(event.instrument_id);
        self.unique_contracts_total.insert(event.instrument_id);

        if event.is_trade() {
            self.trade_events += 1;
            self.day_trades += 1;
        } else {
            self.quote_events += 1;
        }

        if !event.has_valid_bbo() && !event.is_trade() {
            self.sentinel_events += 1;
        }

        self.dte_events[dte_bucket_index(event.dte)] += 1;
    }

    fn end_of_day(&mut self, _day_index: u32) {
        self.events_per_day.update(self.day_events as f64);
        self.trades_per_day.update(self.day_trades as f64);
        self.contracts_per_day
            .update(self.unique_contracts_day.len() as f64);
        self.n_days += 1;
    }

    fn reset_day(&mut self) {
        self.day_events = 0;
        self.day_trades = 0;
        self.unique_contracts_day.clear();
    }

    fn finalize(&self) -> serde_json::Value {
        let trade_pct = if self.total_events > 0 {
            self.trade_events as f64 / self.total_events as f64 * 100.0
        } else {
            0.0
        };
        let sentinel_pct = if self.total_events > 0 {
            self.sentinel_events as f64 / self.total_events as f64 * 100.0
        } else {
            0.0
        };

        let quote_to_trade_ratio = if self.trade_events > 0 {
            self.quote_events as f64 / self.trade_events as f64
        } else {
            f64::NAN
        };

        json!({
            "tracker": "QualityTracker",
            "n_days": self.n_days,
            "total_events": self.total_events,
            "quote_events": self.quote_events,
            "trade_events": self.trade_events,
            "trade_pct": trade_pct,
            "quote_to_trade_ratio": quote_to_trade_ratio,
            "sentinel_events": self.sentinel_events,
            "sentinel_pct": sentinel_pct,
            "unique_contracts_total": self.unique_contracts_total.len(),
            "events_per_day": {
                "mean": self.events_per_day.mean(),
                "std": self.events_per_day.std(),
                "min": self.events_per_day.min(),
                "max": self.events_per_day.max(),
            },
            "trades_per_day": {
                "mean": self.trades_per_day.mean(),
                "std": self.trades_per_day.std(),
            },
            "contracts_per_day": {
                "mean": self.contracts_per_day.mean(),
                "std": self.contracts_per_day.std(),
            },
            "dte_distribution": {
                DTE_LABELS[0]: self.dte_events[0],
                DTE_LABELS[1]: self.dte_events[1],
                DTE_LABELS[2]: self.dte_events[2],
                DTE_LABELS[3]: self.dte_events[3],
            },
        })
    }

    fn name(&self) -> &str {
        "QualityTracker"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::helpers::*;
    use crate::options_math::moneyness::Moneyness;

    #[test]
    fn test_counts_quotes_and_trades() {
        let mut t = QualityTracker::new();
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let q = make_quote_event(&c, 1.0, 1.05, 0, Moneyness::Atm);
        t.process_event(&q, 3);
        t.process_event(&q, 3);
        let tr = make_trade_event(&c, 1.02, 10, 1.0, 1.05, 0, Moneyness::Atm);
        t.process_event(&tr, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["total_events"], 3);
        assert_eq!(r["quote_events"], 2);
        assert_eq!(r["trade_events"], 1);
    }

    #[test]
    fn test_dte_bucketing() {
        let mut t = QualityTracker::new();
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let e0 = make_quote_event(&c, 1.0, 1.05, 0, Moneyness::Atm);
        let e5 = make_quote_event(&c, 1.0, 1.05, 5, Moneyness::Atm);
        let e30 = make_quote_event(&c, 1.0, 1.05, 30, Moneyness::Atm);
        t.process_event(&e0, 3);
        t.process_event(&e5, 3);
        t.process_event(&e30, 3);
        t.end_of_day(0);
        let r = t.finalize();
        assert_eq!(r["dte_distribution"]["0dte"], 1);
        assert_eq!(r["dte_distribution"]["2_7dte"], 1);
        assert_eq!(r["dte_distribution"]["other"], 1);
    }

    #[test]
    fn test_day_rollover() {
        let mut t = QualityTracker::new();
        t.begin_day(&make_day_context());
        let c = make_contract_call(190.0);
        let e = make_quote_event(&c, 1.0, 1.05, 0, Moneyness::Atm);
        t.process_event(&e, 3);
        t.end_of_day(0);
        t.reset_day();
        t.begin_day(&make_day_context());
        t.process_event(&e, 3);
        t.process_event(&e, 3);
        t.end_of_day(1);
        let r = t.finalize();
        assert_eq!(r["n_days"], 2);
        assert_eq!(r["total_events"], 3);
    }

    #[test]
    fn test_finalize_structure() {
        let t = QualityTracker::new();
        let r = t.finalize();
        assert_eq!(r["tracker"], "QualityTracker");
        assert!(r.get("total_events").is_some());
        assert!(r.get("dte_distribution").is_some());
        assert!(r.get("events_per_day").is_some());
    }
}
