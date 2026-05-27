//! Dispatch-loop diagnostic counters for the OPRA statistical profiler.
//!
//! Every input record yielded by the CMBP-1 decoder lands in exactly one
//! terminal bucket. The conservation law is:
//!
//! ```text
//! total_decoded == unknown_instrument + dispatched
//! ```
//!
//! `decode_errors` are counted at the iterator level in `loader.rs` and
//! never enter the dispatch loop — they are carried here for co-located
//! JSON emission only.

use serde::{Deserialize, Serialize};

/// SemVer for the OPRA profiler diagnostics JSON contract.
///
/// Policy (aligned with MBO `PRODUCER_DIAGNOSTICS_SCHEMA_VERSION`):
/// - MAJOR: field removed or semantically reinterpreted
/// - MINOR: new optional field added (forward-compat via `#[serde(default)]`)
/// - PATCH: docs-only
pub const PROFILER_DIAGNOSTICS_SCHEMA_VERSION: &str = "1.0.0";

/// Dispatch-loop diagnostic counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ProfileDiagnostics {
    /// Records yielded by the decoder (post-decode, pre-dispatch).
    /// Does NOT include `decode_errors`.
    pub total_decoded: u64,

    /// Records skipped because `contract_map.get(instrument_id)` returned None.
    pub unknown_instrument: u64,

    /// Records enriched into `OptionsEvent` and dispatched to all trackers.
    pub dispatched: u64,

    /// Records whose CMBP-1 decoding failed (from `loader.rs`).
    /// These never enter the dispatch loop.
    pub decode_errors: u64,

    /// Informational sub-counters partitioning subsets of `dispatched`.
    /// These are OVERLAPPING observations, NOT filter gates — every counted
    /// event was still dispatched to trackers.
    pub observability: DispatchObservability,
}

impl ProfileDiagnostics {
    /// Verify the conservation law holds. Returns `true` if valid.
    pub fn conservation_check(&self) -> bool {
        self.total_decoded == self.unknown_instrument + self.dispatched
    }
}

/// Observability sub-counters for dispatched events.
///
/// An event may be counted by multiple sub-counters simultaneously
/// (e.g., a trade with `MAYBE_BAD_BOOK` flag increments both
/// `action_trade` and `bad_book_flagged`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct DispatchObservability {
    /// Dispatched events with `action` byte `'T'` (Trade).
    pub action_trade: u64,
    /// Dispatched events with `action` byte `'A'` (Quote).
    pub action_quote: u64,
    /// Dispatched events with `action` byte not in `{'T', 'A'}`.
    pub action_other: u64,
    /// Dispatched events where moneyness classify returned `None`
    /// and the `Otm` fallback was applied.
    pub moneyness_fallback: u64,
    /// Dispatched events with `MAYBE_BAD_BOOK` flag (0x04).
    pub bad_book_flagged: u64,
    /// Dispatched events with `BAD_TS_RECV` flag (0x08).
    pub bad_ts_flagged: u64,
    /// Dispatched events with `SNAPSHOT` flag (0x20).
    pub snapshot_flagged: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conservation_law_valid() {
        let d = ProfileDiagnostics {
            total_decoded: 1000,
            unknown_instrument: 150,
            dispatched: 850,
            decode_errors: 5,
            ..Default::default()
        };
        assert!(d.conservation_check());
    }

    #[test]
    fn test_conservation_law_violated() {
        let d = ProfileDiagnostics {
            total_decoded: 1000,
            unknown_instrument: 150,
            dispatched: 849, // off by 1
            decode_errors: 5,
            ..Default::default()
        };
        assert!(!d.conservation_check());
    }

    #[test]
    fn test_serde_roundtrip() {
        let original = ProfileDiagnostics {
            total_decoded: 1_000_000,
            unknown_instrument: 50_000,
            dispatched: 950_000,
            decode_errors: 3,
            observability: DispatchObservability {
                action_trade: 100_000,
                action_quote: 849_500,
                action_other: 500,
                moneyness_fallback: 12,
                bad_book_flagged: 7,
                bad_ts_flagged: 0,
                snapshot_flagged: 233,
            },
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: ProfileDiagnostics = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.total_decoded, original.total_decoded);
        assert_eq!(restored.dispatched, original.dispatched);
        assert_eq!(restored.unknown_instrument, original.unknown_instrument);
        assert_eq!(restored.decode_errors, original.decode_errors);
        assert_eq!(
            restored.observability.action_trade,
            original.observability.action_trade
        );
        assert_eq!(
            restored.observability.moneyness_fallback,
            original.observability.moneyness_fallback
        );
    }

    #[test]
    fn test_forward_compat_partial_json() {
        let partial = r#"{"total_decoded": 100, "dispatched": 80, "unknown_instrument": 20}"#;
        let d: ProfileDiagnostics = serde_json::from_str(partial).unwrap();
        assert_eq!(d.total_decoded, 100);
        assert_eq!(d.dispatched, 80);
        assert_eq!(d.unknown_instrument, 20);
        assert_eq!(d.decode_errors, 0);
        assert_eq!(d.observability.action_trade, 0);
    }

    #[test]
    fn test_default_is_zero() {
        let d = ProfileDiagnostics::default();
        assert!(d.conservation_check());
        assert_eq!(d.total_decoded, 0);
        assert_eq!(d.dispatched, 0);
    }
}
