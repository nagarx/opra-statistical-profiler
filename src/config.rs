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
