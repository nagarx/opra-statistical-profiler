//! OPRA Statistical Profiler
//!
//! High-performance statistical profiler for options microstructure analysis.
//! Processes raw OPRA CMBP-1 `.dbn.zst` files in a single pass through
//! composable analysis trackers, producing JSON statistical profiles.
//!
//! # Architecture
//!
//! ```text
//! .dbn.zst (CMBP-1) → Cmbp1Loader → SymbologyParser → ContractRouter
//!                                                           │
//!                                              ┌────────────┼────────────┐
//!                                              ▼            ▼            ▼
//!                                       QualityTracker  SpreadTracker  ...
//!                                              │            │            │
//!                                              └────────────┼────────────┘
//!                                                           ▼
//!                                                     JSON profiles
//! ```
//!
//! # Design Principles
//!
//! - **Single pass**: All trackers process events simultaneously
//! - **Bounded memory**: Streaming accumulators (Welford, reservoir sampling)
//! - **Composable**: Each tracker is independent, enable/disable via config
//! - **Reuses statistical primitives** from `hft-statistics`
//! - **Options-specific**: OCC parsing, Greek computation, DTE/moneyness classification

pub mod config;
pub mod contract;
pub mod event;
pub mod loader;
pub mod options_math;
pub mod profiler;
pub mod report_utils;
#[cfg(test)]
pub mod test_helpers;
pub mod trackers;

use event::{DayContext, OptionsEvent};

/// Trait implemented by all options analysis trackers.
///
/// Each tracker processes OPRA CMBP-1 events enriched with contract metadata
/// (strike, expiration, DTE, moneyness, underlying price), accumulates
/// statistics across days, and produces a JSON report at finalization.
///
/// # Lifecycle
///
/// 1. `begin_day()` — called once at the start of each trading day with day-level context
/// 2. `process_event()` — called for every enriched options event
/// 3. `end_of_day()` — called once when a day boundary is detected
/// 4. `reset_day()` — called to prepare for the next day
/// 5. `finalize()` — called once after all data to produce the report
pub trait OptionsTracker: Send {
    /// Called at the start of each trading day with day-level context.
    ///
    /// Provides UTC offset, trading date, underlying prices, and day epoch.
    /// Trackers should store whatever day-level state they need.
    fn begin_day(&mut self, ctx: &DayContext);

    /// Process a single enriched options event.
    ///
    /// The `regime` parameter is reserved for future time-of-day regime analysis.
    /// Currently unused by all trackers (passed as 0).
    fn process_event(&mut self, event: &OptionsEvent, regime: u8);

    /// Called when a day boundary is detected.
    fn end_of_day(&mut self, day_index: u32);

    /// Reset day-level state in preparation for the next day.
    fn reset_day(&mut self);

    /// Produce the final JSON report after all data has been processed.
    fn finalize(&self) -> serde_json::Value;

    /// Human-readable name for logging and report identification.
    fn name(&self) -> &str;
}
