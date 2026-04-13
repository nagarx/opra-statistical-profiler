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
    /// Mid-price of the option BBO. Returns NAN if either side is undefined.
    pub fn option_mid(&self) -> f64 {
        if self.bid_px.is_finite() && self.ask_px.is_finite() && self.ask_px > self.bid_px {
            (self.bid_px + self.ask_px) / 2.0
        } else {
            f64::NAN
        }
    }

    /// BBO spread in USD. Returns NAN if either side is undefined.
    pub fn spread(&self) -> f64 {
        if self.bid_px.is_finite() && self.ask_px.is_finite() && self.ask_px >= self.bid_px {
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
