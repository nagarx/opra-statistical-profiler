//! Enriched options event type.
//!
//! `OptionsEvent` wraps a raw CMBP-1 record with parsed contract metadata,
//! moneyness classification, and the current underlying price.

use chrono::NaiveDate;

use crate::contract::{ContractInfo, ContractType};
use crate::options_math::moneyness::Moneyness;

/// Day-level context passed to trackers at the start of each trading day.
///
/// Provides information that is constant for the entire day: trading date,
/// UTC offset (DST-aware), day epoch, and underlying open/close prices.
/// This avoids hardcoding timezone assumptions in individual trackers.
#[derive(Debug, Clone)]
pub struct DayContext {
    pub trading_date: NaiveDate,
    /// UTC offset in hours (e.g., -5 for EST, -4 for EDT).
    pub utc_offset: i32,
    /// Midnight UTC in nanoseconds for the current trading day.
    pub day_epoch_ns: i64,
    /// Underlying stock opening price (USD).
    pub underlying_open: f64,
    /// Underlying stock closing price (USD).
    pub underlying_close: f64,
}

/// Action type for CMBP-1 records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Quote update (BBO change).
    Quote,
    /// Trade execution.
    Trade,
    /// Other (modify, cancel, clear — rare in CMBP-1).
    Other,
}

/// Side of the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
    None,
}

/// Enriched options event passed to all trackers.
///
/// Created by the profiler's ContractRouter from raw `CbboMsg` records
/// combined with `ContractMap` lookup and underlying price context.
pub struct OptionsEvent<'a> {
    /// Event timestamp (UTC nanoseconds since epoch).
    pub ts_event: i64,
    /// dbn instrument_id.
    pub instrument_id: u32,
    /// Parsed contract metadata (strike, expiry, call/put).
    pub contract: &'a ContractInfo,
    /// Whether this is a quote update or trade.
    pub action: Action,
    /// Aggressor side.
    pub side: Side,
    /// Trade price in USD. `f64::NAN` for quote-only events.
    pub trade_price: f64,
    /// Trade size in contracts. 0 for quote-only events.
    pub trade_size: u32,
    /// Best bid price in USD. `f64::NAN` if undefined (sentinel).
    pub bid_px: f64,
    /// Best ask price in USD. `f64::NAN` if undefined (sentinel).
    pub ask_px: f64,
    /// Best bid size in contracts.
    pub bid_sz: u32,
    /// Best ask size in contracts.
    pub ask_sz: u32,
    /// Days to expiration (0 = 0DTE).
    pub dte: i64,
    /// Moneyness classification.
    pub moneyness: Moneyness,
    /// Strike / underlying_price ratio.
    pub moneyness_ratio: f64,
    /// Current underlying stock price in USD.
    pub underlying_price: f64,
}

impl<'a> OptionsEvent<'a> {
    /// Mid-price of the option BBO.
    ///
    /// Returns `NaN` if either side is undefined OR if the quote is locked/crossed
    /// (`ask <= bid`). Strict-aligned with [`spread`](Self::spread) and
    /// [`has_valid_bbo`](Self::has_valid_bbo).
    pub fn option_mid(&self) -> f64 {
        if self.bid_px.is_finite() && self.ask_px.is_finite() && self.ask_px > self.bid_px {
            (self.bid_px + self.ask_px) / 2.0
        } else {
            f64::NAN
        }
    }

    /// BBO spread in USD.
    ///
    /// Returns `NaN` if either side is undefined OR if the quote is locked/crossed
    /// (`ask <= bid`). Strict-aligned with [`option_mid`](Self::option_mid) and
    /// [`has_valid_bbo`](Self::has_valid_bbo) per F-NEW-R5-1 (2026-05-08): a locked
    /// quote is not a valid spread observation.
    pub fn spread(&self) -> f64 {
        if self.bid_px.is_finite() && self.ask_px.is_finite() && self.ask_px > self.bid_px {
            self.ask_px - self.bid_px
        } else {
            f64::NAN
        }
    }

    /// Spread as percentage of mid-price.
    pub fn spread_pct(&self) -> f64 {
        let mid = self.option_mid();
        if mid > 0.0 && mid.is_finite() {
            self.spread() / mid * 100.0
        } else {
            f64::NAN
        }
    }

    /// Whether this is a 0DTE contract.
    pub fn is_zero_dte(&self) -> bool {
        self.dte == 0
    }

    /// Whether this event is a trade.
    pub fn is_trade(&self) -> bool {
        self.action == Action::Trade
    }

    /// Whether this contract is a call.
    pub fn is_call(&self) -> bool {
        self.contract.contract_type == ContractType::Call
    }

    /// Whether the BBO is valid (both sides defined, ask > bid > 0).
    pub fn has_valid_bbo(&self) -> bool {
        self.bid_px.is_finite()
            && self.ask_px.is_finite()
            && self.bid_px > 0.0
            && self.ask_px > self.bid_px
    }

    /// Whether this contract is near ATM (within the configured range).
    pub fn is_atm(&self) -> bool {
        self.moneyness == Moneyness::Atm
    }
}

#[cfg(test)]
mod tests {
    //! F-NEW-R5-1 boundary-predicate consistency tests (added 2026-05-08).
    //!
    //! Locks the cross-accessor agreement after aligning [`OptionsEvent::spread`]
    //! to STRICT (`ask > bid`). Post-fix the four BBO-derived accessors share a
    //! coherent gating policy:
    //!
    //! - [`OptionsEvent::option_mid`]     STRICT  `ask > bid`
    //! - [`OptionsEvent::spread`]         STRICT  `ask > bid`   (was `>=` pre-fix)
    //! - [`OptionsEvent::spread_pct`]     STRICT  via `option_mid` early-return
    //! - [`OptionsEvent::has_valid_bbo`]  STRICT  `ask > bid` AND `bid > 0`
    //!
    //! Invariant within `{option_mid, spread, spread_pct}`:
    //!   `option_mid().is_finite() <-> spread().is_finite() <-> spread_pct().is_finite()`
    //!
    //! `has_valid_bbo()` is a STRICTER predicate (also requires `bid > 0`), so the
    //! relationship is a forward implication only:
    //!   `has_valid_bbo() => option_mid().is_finite()`
    //!
    //! Any future drift between event.rs:91, :105, :114, :139 will surface here.

    use crate::options_math::moneyness::Moneyness;
    use crate::test_helpers::helpers::{make_contract_call, make_quote_event};

    #[test]
    fn test_bbo_normal_ask_gt_bid_all_accessors_finite() {
        let contract = make_contract_call(190.0);
        let event = make_quote_event(&contract, 2.00, 2.10, 0, Moneyness::Atm);

        assert!(
            event.option_mid().is_finite(),
            "option_mid should be finite for normal quote"
        );
        assert!(
            event.spread().is_finite(),
            "spread should be finite for normal quote"
        );
        assert!(
            event.spread_pct().is_finite(),
            "spread_pct should be finite for normal quote"
        );
        assert!(
            event.has_valid_bbo(),
            "has_valid_bbo should be true for normal quote"
        );
    }

    #[test]
    fn test_bbo_locked_ask_eq_bid_all_accessors_nan() {
        // Locked quote: ask == bid. Post-F-NEW-R5-1, all 4 accessors reject this.
        let contract = make_contract_call(190.0);
        let event = make_quote_event(&contract, 2.00, 2.00, 0, Moneyness::Atm);

        assert!(
            event.option_mid().is_nan(),
            "option_mid must be NaN for locked quote (ask==bid)"
        );
        assert!(
            event.spread().is_nan(),
            "spread must be NaN for locked quote (F-NEW-R5-1 fix)"
        );
        assert!(
            event.spread_pct().is_nan(),
            "spread_pct must be NaN for locked quote"
        );
        assert!(
            !event.has_valid_bbo(),
            "has_valid_bbo must be false for locked quote"
        );
    }

    #[test]
    fn test_bbo_crossed_ask_lt_bid_all_accessors_nan() {
        // Crossed quote: ask < bid. Strictly invalid for all 4 accessors.
        let contract = make_contract_call(190.0);
        let event = make_quote_event(&contract, 2.10, 2.00, 0, Moneyness::Atm);

        assert!(
            event.option_mid().is_nan(),
            "option_mid must be NaN for crossed quote (ask<bid)"
        );
        assert!(
            event.spread().is_nan(),
            "spread must be NaN for crossed quote"
        );
        assert!(
            event.spread_pct().is_nan(),
            "spread_pct must be NaN for crossed quote"
        );
        assert!(
            !event.has_valid_bbo(),
            "has_valid_bbo must be false for crossed quote"
        );
    }

    #[test]
    fn test_bbo_nan_bid_all_accessors_nan() {
        let contract = make_contract_call(190.0);
        let event = make_quote_event(&contract, f64::NAN, 2.10, 0, Moneyness::Atm);

        assert!(
            event.option_mid().is_nan(),
            "option_mid must be NaN for undefined bid"
        );
        assert!(
            event.spread().is_nan(),
            "spread must be NaN for undefined bid"
        );
        assert!(
            event.spread_pct().is_nan(),
            "spread_pct must be NaN for undefined bid"
        );
        assert!(
            !event.has_valid_bbo(),
            "has_valid_bbo must be false for undefined bid"
        );
    }

    #[test]
    fn test_bbo_nan_ask_all_accessors_nan() {
        let contract = make_contract_call(190.0);
        let event = make_quote_event(&contract, 2.00, f64::NAN, 0, Moneyness::Atm);

        assert!(
            event.option_mid().is_nan(),
            "option_mid must be NaN for undefined ask"
        );
        assert!(
            event.spread().is_nan(),
            "spread must be NaN for undefined ask"
        );
        assert!(
            event.spread_pct().is_nan(),
            "spread_pct must be NaN for undefined ask"
        );
        assert!(
            !event.has_valid_bbo(),
            "has_valid_bbo must be false for undefined ask"
        );
    }

    #[test]
    fn test_invariant_finiteness_agrees_across_option_mid_spread_spread_pct() {
        // KEYSTONE TEST: lock the cross-accessor invariant. If any future change
        // makes one of {option_mid, spread, spread_pct} accept a case the others
        // reject (or vice versa), this test fails. Prevents asymmetry recurrence.
        let contract = make_contract_call(190.0);
        let cases: &[(f64, f64, &str)] = &[
            (2.00, 2.10, "normal"),
            (2.00, 2.00, "locked"),
            (2.10, 2.00, "crossed"),
            (f64::NAN, 2.10, "nan_bid"),
            (2.00, f64::NAN, "nan_ask"),
        ];

        for (bid, ask, label) in cases {
            let event = make_quote_event(&contract, *bid, *ask, 0, Moneyness::Atm);
            let m = event.option_mid().is_finite();
            let s = event.spread().is_finite();
            let p = event.spread_pct().is_finite();
            assert_eq!(
                m, s,
                "[{}] option_mid.finite={} but spread.finite={} — F-NEW-R5-1 invariant broken",
                label, m, s
            );
            assert_eq!(
                m, p,
                "[{}] option_mid.finite={} but spread_pct.finite={} — F-NEW-R5-1 invariant broken",
                label, m, p
            );
        }
    }

    // ─── F-013 / FIND-UFO contract-lock test (2026-05-08) ───────────────────────
    //
    // Locks the EXPECTED post-fix behavior for the underlying-frozen-at-open bug
    // (profiler.rs:111 `let underlying_estimate = underlying_open;`). Today this
    // test FAILS by design — both synthetic events share the test_helpers hardcoded
    // `underlying_price=190.0`. Re-enabled (and helpers extended to be intraday-aware)
    // when Axis L underlying-feed merge ships (L.1 EQUS-embed / L.2 pre-pass /
    // L.3 k-way merge / L.4 60s snapshot / L.5 forward-fill).
    //
    // Track: HANDOFF_FOR_DESIGN_AND_IMPLEMENTATION.md §4 F-013, §5 Axis L, §10 Q7.

    #[test]
    #[ignore = "FIND-UFO — blocked on Axis L (L.1-L.5) decision; documents expected post-fix contract"]
    fn test_intraday_underlying_reflected_in_event_underlying_price() {
        // SCENARIO: NVDA opens at $190 (09:30 ET) and trades $200 by 15:00 ET. The
        // profiler currently freezes underlying_estimate at session open
        // (profiler.rs:111), so every event sees $190 regardless of ts_event.
        // Post-Axis-L the underlying updates intraday and distinct events at
        // distinct intraday times must have DISTINCT underlying_price.
        let contract = make_contract_call(190.0);
        let early_event = make_quote_event(&contract, 2.00, 2.10, 0, Moneyness::Atm);
        let late_event = make_quote_event(&contract, 2.00, 2.10, 0, Moneyness::Atm);

        // Minimal contract assertion: distinct events at distinct intraday times should
        // have DISTINCT underlying_price when the market moves. Today both events share
        // the test_helpers hardcoded `underlying_price=190.0` so this fails — by design.
        // When Axis L lands the implementer should: (a) extend test_helpers with an
        // intraday-aware variant, (b) rewrite this test using the new helper, (c) drop
        // the `#[ignore]` attribute.
        assert_ne!(
            early_event.underlying_price, late_event.underlying_price,
            "FIND-UFO contract: profiler.rs:111 should produce distinct underlying_price \
             for events at distinct intraday ts_event when the market moves. Currently \
             underlying_estimate is hardcoded to underlying_open. Fix requires Axis L \
             (L.1 EQUS-embed / L.2 pre-pass / L.3 k-way merge / L.4 60s snapshot / L.5 forward-fill)."
        );
    }
}
