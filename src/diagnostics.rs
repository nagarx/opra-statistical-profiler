//! Dispatch-loop diagnostic counters for the OPRA statistical profiler.
//!
//! Every input record yielded by the CMBP-1 decoder lands in exactly one
//! terminal bucket. The conservation law is:
//!
//! ```text
//! total_decoded == unknown_instrument + dispatched
//! ```
//!
//! `decode_errors` and `unsupported_rtype` are counted at the iterator level in
//! `loader.rs` and never enter the dispatch loop — they are carried here for
//! co-located JSON emission only, and are excluded from `total_decoded`.

use serde::{Deserialize, Serialize};

/// SemVer for the OPRA profiler diagnostics JSON contract.
///
/// Policy (aligned with MBO `PRODUCER_DIAGNOSTICS_SCHEMA_VERSION`):
/// - MAJOR: field removed or semantically reinterpreted
/// - MINOR: new optional field added (forward-compat via `#[serde(default)]`)
/// - PATCH: docs-only
///
/// History: 1.0.0 -> 1.1.0 (2026-08-01) is additive — `unsupported_rtype` and
/// `cbbo_action_unavailable`, both introduced with the dbn v0.20.0 -> v0.64.0
/// bump (see `loader.rs`). Locked by `test_backward_compat_schema_1_0_0_blob`.
pub const PROFILER_DIAGNOSTICS_SCHEMA_VERSION: &str = "1.1.0";

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

    /// Records skipped because their rtype is neither CMBP-1/TCBBO nor
    /// CBBO-1s/1m (from `loader.rs`). Like `decode_errors`, these never enter
    /// the dispatch loop and are excluded from `total_decoded`, so the
    /// conservation law is unaffected.
    ///
    /// A non-zero value means the profiler was pointed at a file whose schema
    /// it does not decode — a configuration error, not a data property. It is
    /// the JSON-visible half of the loader's rtype catch-all, which must never
    /// drop a record silently (hft-rules §8). Added 2026-08-01 with the dbn
    /// v0.64.0 bump, which made the rtype dispatch explicit.
    pub unsupported_rtype: u64,

    /// Of the records that WERE dispatched, how many came from a CBBO-1s/1m
    /// file and therefore carry no `action` field (dbn 0.21.0 reclassified that
    /// byte as reserved on 0xC0/0xC1). Such records are all classified
    /// `Action::Other` and contribute zero trade volume.
    ///
    /// This is an OVERLAPPING observation like the `observability` block, not a
    /// terminal bucket: it equals `total_decoded` for a pure CBBO file and `0`
    /// for a pure CMBP-1 file. Added 2026-08-01 — see `loader.rs` module docs.
    pub cbbo_action_unavailable: u64,

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
            unsupported_rtype: 17,
            cbbo_action_unavailable: 1_000,
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
        assert_eq!(restored.unsupported_rtype, original.unsupported_rtype);
        assert_eq!(
            restored.cbbo_action_unavailable,
            original.cbbo_action_unavailable
        );
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

    /// A schema-1.0.0 diagnostics blob (written before the 2026-08-01 dbn
    /// v0.64.0 bump) must still deserialize, with the two fields that bump
    /// added defaulting to 0. This is what makes the 1.0.0 -> 1.1.0 change a
    /// MINOR bump under the policy documented above.
    #[test]
    fn test_backward_compat_schema_1_0_0_blob() {
        let v1 = r#"{
            "total_decoded": 2000000,
            "unknown_instrument": 0,
            "dispatched": 2000000,
            "decode_errors": 0,
            "observability": {"action_trade": 4128, "action_quote": 1995872, "action_other": 0}
        }"#;
        let d: ProfileDiagnostics = serde_json::from_str(v1).unwrap();
        assert!(d.conservation_check());
        assert_eq!(d.observability.action_trade, 4128);
        assert_eq!(
            d.unsupported_rtype, 0,
            "field absent in schema 1.0.0 must default to 0"
        );
        assert_eq!(
            d.cbbo_action_unavailable, 0,
            "field absent in schema 1.0.0 must default to 0"
        );
    }

    /// `unsupported_rtype` records are skipped by the loader and never reach
    /// the dispatch loop, so — like `decode_errors` — they must stay OUT of the
    /// `total_decoded == unknown_instrument + dispatched` conservation law.
    #[test]
    fn test_unsupported_rtype_excluded_from_conservation_law() {
        let d = ProfileDiagnostics {
            total_decoded: 1000,
            unknown_instrument: 150,
            dispatched: 850,
            decode_errors: 5,
            unsupported_rtype: 42,
            cbbo_action_unavailable: 0,
            ..Default::default()
        };
        assert!(
            d.conservation_check(),
            "unsupported_rtype must not participate in the conservation law"
        );
    }

    /// `cbbo_action_unavailable` counts records that WERE dispatched, so it is
    /// an overlapping observation (like the `observability` block) and is
    /// bounded above by `dispatched` — never a separate terminal bucket.
    #[test]
    fn test_cbbo_action_unavailable_is_overlapping_not_terminal() {
        // A pure CBBO file: every dispatched record lacks an `action`.
        let d = ProfileDiagnostics {
            total_decoded: 77_220,
            unknown_instrument: 0,
            dispatched: 77_220,
            cbbo_action_unavailable: 77_220,
            observability: DispatchObservability {
                action_other: 77_220,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(d.conservation_check());
        assert!(d.cbbo_action_unavailable <= d.dispatched);
        assert_eq!(
            d.observability.action_other, d.cbbo_action_unavailable,
            "records with no action field must all land in the Other bucket"
        );
    }

    #[test]
    fn test_default_is_zero() {
        let d = ProfileDiagnostics::default();
        assert!(d.conservation_check());
        assert_eq!(d.total_decoded, 0);
        assert_eq!(d.dispatched, 0);
    }

    #[test]
    fn test_action_sub_counters_partition_dispatched() {
        let d = ProfileDiagnostics {
            total_decoded: 1000,
            unknown_instrument: 50,
            dispatched: 950,
            decode_errors: 0,
            observability: DispatchObservability {
                action_trade: 200,
                action_quote: 740,
                action_other: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(d.conservation_check());
        assert_eq!(
            d.observability.action_trade
                + d.observability.action_quote
                + d.observability.action_other,
            d.dispatched,
            "Action sub-counters must partition dispatched"
        );
    }
}
