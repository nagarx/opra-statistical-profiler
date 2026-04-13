//! CLI for OPRA statistical profiling.

use std::path::PathBuf;

use chrono::NaiveDate;
use opra_statistical_profiler::config::ProfilerConfig;
use opra_statistical_profiler::profiler::{self, DailyUnderlyingPrice};
use opra_statistical_profiler::trackers::*;
use opra_statistical_profiler::OptionsTracker;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config_path = parse_args();
    let config = match ProfilerConfig::from_file(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config from {}: {}", config_path.display(), e);
            std::process::exit(1);
        }
    };

    log::info!(
        "OPRA Statistical Profiler v{}",
        env!("CARGO_PKG_VERSION")
    );
    log::info!("Symbol: {}", config.input.symbol);

    let underlying_prices = match &config.input.underlying_prices_file {
        Some(path) => {
            match profiler::load_underlying_prices_from_equs(path) {
                Ok(prices) => prices,
                Err(e) => {
                    log::warn!("Failed to load EQUS prices from {}: {}. Using fallback.", path.display(), e);
                    load_nvda_fallback_prices()
                }
            }
        }
        None => {
            log::info!("No underlying_prices_file configured; using built-in NVDA fallback prices");
            load_nvda_fallback_prices()
        }
    };

    let mut trackers: Vec<Box<dyn OptionsTracker>> = Vec::new();

    if config.trackers.quality {
        trackers.push(Box::new(QualityTracker::new()));
    }
    if config.trackers.spread {
        trackers.push(Box::new(SpreadTracker::new(config.reservoir_capacity)));
    }
    if config.trackers.zero_dte {
        trackers.push(Box::new(ZeroDteTracker::new(config.reservoir_capacity)));
    }
    if config.trackers.premium_decay {
        trackers.push(Box::new(PremiumDecayTracker::new(config.input.risk_free_rate)));
    }
    if config.trackers.volume {
        trackers.push(Box::new(VolumeTracker::new(config.reservoir_capacity)));
    }
    if config.trackers.greeks {
        trackers.push(Box::new(GreeksTracker::new(
            config.input.risk_free_rate,
            config.reservoir_capacity,
        )));
    }
    if config.trackers.put_call {
        trackers.push(Box::new(PutCallRatioTracker::new()));
    }
    if config.trackers.effective_spread {
        trackers.push(Box::new(OptionsEffectiveSpreadTracker::new()));
    }

    match profiler::run(&config, &mut trackers, &underlying_prices) {
        Ok(result) => {
            if let Err(e) = profiler::write_output(&config, &result) {
                eprintln!("Failed to write output: {}", e);
                std::process::exit(1);
            }
            log::info!(
                "Done: {} days, {} events, {:.1}s ({:.0} evt/s)",
                result.n_days,
                result.total_events,
                result.elapsed_secs,
                result.total_events as f64 / result.elapsed_secs.max(0.001),
            );
        }
        Err(e) => {
            eprintln!("Profiler error: {}", e);
            std::process::exit(1);
        }
    }
}

fn parse_args() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    for i in 1..args.len() {
        if args[i] == "--config" && i + 1 < args.len() {
            return PathBuf::from(&args[i + 1]);
        }
    }
    eprintln!("Usage: profile_opra --config <path>");
    std::process::exit(1);
}

/// Fallback NVDA prices for the 8-day OPRA window (Nov 13-24 2025).
/// Used when no underlying_prices_file is configured. Values from EQUS OHLCV.
fn load_nvda_fallback_prices() -> Vec<DailyUnderlyingPrice> {
    vec![
        dp(2025, 11, 13, 191.05, 186.86),
        dp(2025, 11, 14, 182.86, 190.17),
        dp(2025, 11, 17, 185.97, 186.60),
        dp(2025, 11, 18, 183.38, 181.36),
        dp(2025, 11, 19, 184.79, 186.52),
        dp(2025, 11, 20, 195.95, 180.64),
        dp(2025, 11, 21, 181.24, 178.88),
        dp(2025, 11, 24, 179.49, 182.55),
    ]
}

fn dp(y: i32, m: u32, d: u32, open: f64, close: f64) -> DailyUnderlyingPrice {
    DailyUnderlyingPrice {
        date: NaiveDate::from_ymd_opt(y, m, d).unwrap(),
        open,
        close,
    }
}
