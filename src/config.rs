//! Profiler configuration, TOML-driven.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Top-level profiler configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilerConfig {
    pub input: InputConfig,
    #[serde(default)]
    pub trackers: TrackerConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub buckets: BucketConfig,
    #[serde(default)]
    pub os_ratio: OsRatioConfig,
    #[serde(default = "default_reservoir_capacity")]
    pub reservoir_capacity: usize,
}

/// Input data source configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputConfig {
    /// Directory containing OPRA `.dbn.zst` files.
    pub data_dir: PathBuf,
    /// Filename pattern (e.g., "opra-pillar-{date}.cmbp-1.dbn.zst").
    pub filename_pattern: String,
    /// Underlying symbol (e.g., "NVDA").
    #[serde(default = "default_symbol")]
    pub symbol: String,
    /// Path to EQUS OHLCV file for underlying prices.
    pub underlying_prices_file: Option<PathBuf>,
    /// Annualized risk-free rate for BSM calculations.
    #[serde(default = "default_risk_free_rate")]
    pub risk_free_rate: f64,
    /// Optional date range filter (inclusive, YYYY-MM-DD).
    pub date_start: Option<String>,
    pub date_end: Option<String>,
}

/// Which trackers to enable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerConfig {
    #[serde(default = "default_true")]
    pub quality: bool,
    #[serde(default = "default_true")]
    pub spread: bool,
    #[serde(default = "default_true")]
    pub premium_decay: bool,
    #[serde(default = "default_true")]
    pub volume: bool,
    #[serde(default = "default_true")]
    pub greeks: bool,
    #[serde(default = "default_true")]
    pub zero_dte: bool,
    #[serde(default = "default_true")]
    pub put_call: bool,
    #[serde(default = "default_true")]
    pub effective_spread: bool,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            quality: true,
            spread: true,
            premium_decay: true,
            volume: true,
            greeks: true,
            zero_dte: true,
            put_call: true,
            effective_spread: true,
        }
    }
}

/// Output configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    #[serde(default = "default_output_dir")]
    pub output_dir: PathBuf,
    #[serde(default = "default_true")]
    pub write_summaries: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("output_opra"),
            write_summaries: true,
        }
    }
}

/// Bucketing configuration for DTE and moneyness analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketConfig {
    /// ATM range as fraction of underlying price. Default: 0.02 (+/- 2%).
    #[serde(default = "default_atm_range")]
    pub atm_range_pct: f64,
    /// Deep ITM/OTM boundary. Default: 0.10 (10% from ATM).
    #[serde(default = "default_deep_range")]
    pub deep_range_pct: f64,
}

impl Default for BucketConfig {
    fn default() -> Self {
        Self {
            atm_range_pct: 0.02,
            deep_range_pct: 0.10,
        }
    }
}

/// Option-to-stock volume ratio (O/S) configuration.
///
/// O/S = OPVOL / EQVOL (Johnson & So 2012, *Journal of Financial Economics*
/// 106(2):262-286, Eq. 9): total option-contract volume (calls + puts, UNSIGNED,
/// restricted to the `dte_min..=dte_max` band) divided by equity volume expressed
/// in round lots of 100 shares. ΔO/S is the deviation of O/S from its trailing
/// `rolling_window_days` mean (the single-name analog of Johnson-So's prior-6-month
/// average). See the wiki entry `theory:option_to_stock_volume_ratio`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OsRatioConfig {
    /// Whether to compute and emit the O/S ratio report (`NN_OsRatio.json`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Trailing window (in trading days) for the ΔO/S rolling-mean deviation.
    /// Johnson-So use a prior-6-MONTH average for monthly O/S; 6 trading days is
    /// the single-name daily-cadence analog. On an 8-day pilot this leaves only
    /// ~2 ΔO/S points (`n_warmup_null == min(window, n_days)`).
    #[serde(default = "default_os_rolling_window")]
    pub rolling_window_days: usize,
    /// Inclusive lower bound of the option DTE band for OPVOL. Johnson-So footnote 9:
    /// options expiring in the 30 trading days beginning 5 days after the trade date
    /// → the 5-35-DTE band (the nearest 5 days are excluded to avoid expiration-roll
    /// volume). Opposite end of the maturity spectrum from the 0DTE-dominated window.
    #[serde(default = "default_os_dte_min")]
    pub dte_min: i64,
    /// Inclusive upper bound of the option DTE band for OPVOL.
    #[serde(default = "default_os_dte_max")]
    pub dte_max: i64,
}

impl Default for OsRatioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rolling_window_days: 6,
            dte_min: 5,
            dte_max: 35,
        }
    }
}

fn default_os_rolling_window() -> usize {
    6
}

fn default_os_dte_min() -> i64 {
    5
}

fn default_os_dte_max() -> i64 {
    35
}

fn default_atm_range() -> f64 {
    0.02
}

fn default_deep_range() -> f64 {
    0.10
}

fn default_symbol() -> String {
    "NVDA".to_string()
}

fn default_risk_free_rate() -> f64 {
    0.05
}

fn default_true() -> bool {
    true
}

fn default_output_dir() -> PathBuf {
    PathBuf::from("output_opra")
}

fn default_reservoir_capacity() -> usize {
    10_000
}

impl ProfilerConfig {
    pub fn from_file(path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&contents)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal TOML with no `[os_ratio]` section. Locks backward-compat: existing
    /// configs (e.g. `configs/nvda_opra_8day.toml`) must parse clean and default
    /// O/S to ENABLED. Guards the `#[serde(default = "default_true")]` choice — a
    /// bare `#[serde(default)]` on the bool would silently default `enabled` to
    /// `false` and disable O/S for every pre-existing config.
    #[test]
    fn test_os_ratio_defaults_when_section_absent() {
        let toml_src = r#"
            [input]
            data_dir = "/tmp/opra"
            filename_pattern = "opra-{date}.dbn.zst"
        "#;
        let cfg: ProfilerConfig = toml::from_str(toml_src).expect("minimal config must parse");
        assert!(
            cfg.os_ratio.enabled,
            "O/S must DEFAULT to enabled when [os_ratio] is absent (serde-default-true), \
             got enabled={}",
            cfg.os_ratio.enabled
        );
        assert_eq!(cfg.os_ratio.rolling_window_days, 6);
        assert_eq!(cfg.os_ratio.dte_min, 5);
        assert_eq!(cfg.os_ratio.dte_max, 35);
    }

    #[test]
    fn test_os_ratio_explicit_disable() {
        let toml_src = r#"
            [input]
            data_dir = "/tmp/opra"
            filename_pattern = "opra-{date}.dbn.zst"
            [os_ratio]
            enabled = false
            rolling_window_days = 20
        "#;
        let cfg: ProfilerConfig = toml::from_str(toml_src).expect("config must parse");
        assert!(!cfg.os_ratio.enabled);
        assert_eq!(cfg.os_ratio.rolling_window_days, 20);
        // Unset band fields still default.
        assert_eq!(cfg.os_ratio.dte_min, 5);
        assert_eq!(cfg.os_ratio.dte_max, 35);
    }

    #[test]
    fn test_os_ratio_rejects_unknown_field() {
        let toml_src = r#"
            [input]
            data_dir = "/tmp/opra"
            filename_pattern = "opra-{date}.dbn.zst"
            [os_ratio]
            bogus_field = 1
        "#;
        let result: Result<ProfilerConfig, _> = toml::from_str(toml_src);
        assert!(
            result.is_err(),
            "deny_unknown_fields must reject an unknown [os_ratio] key"
        );
    }
}
