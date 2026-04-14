//! Single-pass profiling engine for OPRA options data.
//!
//! Coordinates CMBP-1 decoding, contract resolution, event enrichment,
//! and tracker dispatch across all trading days.

use std::path::PathBuf;
use std::time::Instant;

use chrono::NaiveDate;

use crate::config::ProfilerConfig;
use crate::contract::ContractMap;
use crate::event::{Action, DayContext, OptionsEvent, Side};
use crate::loader::Cmbp1Loader;
use crate::options_math::moneyness::{Moneyness, MoneynessBuckets};
use crate::OptionsTracker;

use hft_statistics::time::regime::{utc_offset_for_date, midnight_utc_ns};

const SENTINEL_I64: i64 = i64::MAX;


/// Profiling result.
pub struct ProfileResult {
    pub n_days: u32,
    pub total_events: u64,
    pub elapsed_secs: f64,
    pub reports: Vec<(String, serde_json::Value)>,
}

/// Underlying price for a trading day (from EQUS or config).
pub struct DailyUnderlyingPrice {
    pub date: NaiveDate,
    pub open: f64,
    pub close: f64,
}

/// Run the OPRA profiler.
pub fn run(
    config: &ProfilerConfig,
    trackers: &mut Vec<Box<dyn OptionsTracker>>,
    underlying_prices: &[DailyUnderlyingPrice],
) -> Result<ProfileResult, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let files = discover_files(config)?;

    if files.is_empty() {
        return Err("No OPRA .dbn.zst files found matching the configuration".into());
    }
    log::info!("Discovered {} day files to process", files.len());

    let moneyness_buckets = MoneynessBuckets {
        atm_range: config.buckets.atm_range_pct,
        deep_range: config.buckets.deep_range_pct,
    };

    let mut total_events: u64 = 0;
    let mut day_index: u32 = 0;

    for (i, (date_str, file_path)) in files.iter().enumerate() {
        let day_start = Instant::now();

        let year: i32 = date_str[0..4].parse()?;
        let month: u32 = date_str[5..7].parse()?;
        let day: u32 = date_str[8..10].parse()?;
        let trading_date = NaiveDate::from_ymd_opt(year, month, day)
            .ok_or_else(|| format!("Invalid date: {}", date_str))?;

        let utc_offset = utc_offset_for_date(year, month, day);
        let day_epoch = midnight_utc_ns(year, month, day);

        // Underlying price is REQUIRED for moneyness classification and BSM Greeks.
        // Falling back silently to 0.0 would NaN-poison moneyness for the entire day
        // and silently misclassify every contract as OTM. Hard error instead.
        let underlying_px = underlying_prices
            .iter()
            .find(|p| p.date == trading_date)
            .ok_or_else(|| {
                format!(
                    "No underlying price for trading date {}. Configure underlying_prices_file to cover this date, or extend the built-in fallback prices.",
                    date_str
                )
            })?;
        let underlying_open = underlying_px.open;
        let underlying_close = underlying_px.close;

        let day_ctx = DayContext {
            trading_date,
            utc_offset,
            day_epoch_ns: day_epoch,
            underlying_open,
            underlying_close,
        };

        for tracker in trackers.iter_mut() {
            tracker.begin_day(&day_ctx);
        }

        let loader = Cmbp1Loader::new(file_path)?;
        let (metadata, records) = loader.open()?;

        let dbn_date = time::Date::from_calendar_date(
            year,
            time::Month::try_from(month as u8).map_err(|e| format!("Invalid month: {}", e))?,
            day as u8,
        ).map_err(|e| format!("Invalid date for symbology: {}", e))?;

        let contract_map = ContractMap::from_dbn_metadata(&metadata, dbn_date);

        let mut day_events: u64 = 0;
        let underlying_estimate = underlying_open;

        for record in records {
            let contract = match contract_map.get(record.hd.instrument_id) {
                Some(c) => c,
                None => continue,
            };

            let bid_px = price_or_nan(record.levels[0].bid_px);
            let ask_px = price_or_nan(record.levels[0].ask_px);
            let trade_price = price_or_nan(record.price);

            let action = match record.action as u8 {
                b'T' => Action::Trade,
                b'A' => Action::Quote,
                _ => Action::Other,
            };

            let side = match record.side as u8 {
                b'B' => Side::Bid,
                b'A' => Side::Ask,
                _ => Side::None,
            };

            let dte = contract.dte(trading_date);
            let moneyness_ratio = if underlying_estimate > 0.0 {
                contract.strike / underlying_estimate
            } else {
                f64::NAN
            };
            let moneyness = moneyness_buckets
                .classify(contract.strike, underlying_estimate, contract.contract_type)
                .unwrap_or(Moneyness::Otm);

            let event = OptionsEvent {
                ts_event: record.hd.ts_event as i64,
                instrument_id: record.hd.instrument_id,
                contract,
                action,
                side,
                trade_price,
                trade_size: if action == Action::Trade { record.size } else { 0 },
                bid_px,
                ask_px,
                bid_sz: record.levels[0].bid_sz,
                ask_sz: record.levels[0].ask_sz,
                dte,
                moneyness,
                moneyness_ratio,
                underlying_price: underlying_estimate,
            };

            // Reserved for future regime-aware trackers. Currently unused by all trackers.
            let regime: u8 = 0;

            for tracker in trackers.iter_mut() {
                tracker.process_event(&event, regime);
            }

            day_events += 1;
        }

        for tracker in trackers.iter_mut() {
            tracker.end_of_day(day_index);
        }

        total_events += day_events;
        day_index += 1;

        let day_elapsed = day_start.elapsed().as_secs_f64();
        let throughput = day_events as f64 / day_elapsed.max(0.001);
        let eta_secs = if i > 0 {
            let avg = start.elapsed().as_secs_f64() / (i + 1) as f64;
            avg * (files.len() - i - 1) as f64
        } else {
            0.0
        };

        log::info!(
            "[{}/{}] {} — {:.1}s, {} events, {:.0} evt/s, {} contracts, ETA {:.0}s",
            i + 1,
            files.len(),
            date_str,
            day_elapsed,
            day_events,
            throughput,
            contract_map.len(),
            eta_secs,
        );

        for tracker in trackers.iter_mut() {
            tracker.reset_day();
        }
    }

    let reports: Vec<(String, serde_json::Value)> = trackers
        .iter()
        .map(|t| (t.name().to_string(), t.finalize()))
        .collect();

    let elapsed = start.elapsed().as_secs_f64();
    log::info!(
        "OPRA profiling complete: {} days, {} events, {:.1}s ({:.0} evt/s)",
        day_index,
        total_events,
        elapsed,
        total_events as f64 / elapsed.max(0.001),
    );

    Ok(ProfileResult {
        n_days: day_index,
        total_events,
        elapsed_secs: elapsed,
        reports,
    })
}

/// Convert fixed-point i64 price to f64 dollars. Returns NAN for sentinel values.
#[inline]
fn price_or_nan(price_i64: i64) -> f64 {
    if price_i64 == SENTINEL_I64 || price_i64 <= 0 {
        f64::NAN
    } else {
        price_i64 as f64 / 1e9
    }
}

/// Write profiler output to disk.
pub fn write_output(
    config: &ProfilerConfig,
    result: &ProfileResult,
) -> Result<(), Box<dyn std::error::Error>> {
    let output_dir = &config.output.output_dir;
    std::fs::create_dir_all(output_dir)?;

    let provenance = serde_json::json!({
        "profiler_version": env!("CARGO_PKG_VERSION"),
        "symbol": config.input.symbol,
        "dataset": "OPRA.PILLAR",
        "schema": "cmbp-1",
        "n_days": result.n_days,
        "total_events": result.total_events,
        "runtime_secs": result.elapsed_secs,
        "throughput_events_per_sec": result.total_events as f64 / result.elapsed_secs.max(0.001),
        "config": serde_json::to_value(config).unwrap_or_default(),
    });

    for (i, (name, report)) in result.reports.iter().enumerate() {
        let mut full_report = report.clone();
        if let Some(obj) = full_report.as_object_mut() {
            obj.insert("_provenance".to_string(), provenance.clone());
        }

        let json_path = output_dir.join(format!("{:02}_{}.json", i + 1, name));
        let json_str = serde_json::to_string_pretty(&full_report)?;
        std::fs::write(&json_path, &json_str)?;
        log::info!("Wrote {}", json_path.display());
    }

    Ok(())
}

/// Discover OPRA .dbn.zst files sorted by date.
fn discover_files(
    config: &ProfilerConfig,
) -> Result<Vec<(String, PathBuf)>, Box<dyn std::error::Error>> {
    let pattern = &config.input.filename_pattern;
    let search_dir = &config.input.data_dir;

    if !search_dir.exists() {
        return Err(format!("Data directory does not exist: {}", search_dir.display()).into());
    }

    let date_placeholder = "{date}";
    let (prefix, suffix) = if let Some(pos) = pattern.find(date_placeholder) {
        (&pattern[..pos], &pattern[pos + date_placeholder.len()..])
    } else {
        return Err("filename_pattern must contain {date} placeholder".into());
    };

    let mut files: Vec<(String, PathBuf)> = Vec::new();

    for entry in std::fs::read_dir(search_dir)? {
        let entry = entry?;
        let filename = entry.file_name();
        let name = filename.to_string_lossy();

        if !name.starts_with(prefix) || !name.ends_with(suffix) {
            continue;
        }

        let date_part = &name[prefix.len()..name.len() - suffix.len()];
        if date_part.len() != 8 || date_part.chars().any(|c| !c.is_ascii_digit()) {
            continue;
        }

        let date_str = format!(
            "{}-{}-{}",
            &date_part[0..4],
            &date_part[4..6],
            &date_part[6..8]
        );

        if let Some(ref start) = config.input.date_start {
            if date_str < *start {
                continue;
            }
        }
        if let Some(ref end) = config.input.date_end {
            if date_str > *end {
                continue;
            }
        }

        files.push((date_str, entry.path()));
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Load underlying prices from an EQUS OHLCV `.dbn.zst` file.
///
/// Uses the dbn crate to decode `OhlcvMsg` records and extract daily
/// open/close prices. The EQUS file is typically small (~8 KB).
pub fn load_underlying_prices_from_equs(
    path: &std::path::Path,
) -> Result<Vec<DailyUnderlyingPrice>, Box<dyn std::error::Error>> {
    use dbn::decode::{DecodeRecord, DynDecoder};
    use dbn::enums::VersionUpgradePolicy;
    use dbn::OhlcvMsg;
    use std::io::BufReader;

    let file = std::fs::File::open(path)?;
    let reader = BufReader::with_capacity(64 * 1024, file);
    let mut decoder = DynDecoder::inferred_with_buffer(reader, VersionUpgradePolicy::AsIs)?;

    let mut prices = Vec::new();

    while let Some(record) = decoder.decode_record::<OhlcvMsg>()? {
        let ts_secs = record.hd.ts_event / 1_000_000_000;
        let days_since_epoch = ts_secs / 86400;
        let date = chrono::NaiveDate::from_num_days_from_ce_opt(
            (days_since_epoch + 719_163) as i32,
        );

        if let Some(d) = date {
            let open = record.open as f64 / 1e9;
            let close = record.close as f64 / 1e9;
            if open > 0.0 && close > 0.0 {
                prices.push(DailyUnderlyingPrice {
                    date: d,
                    open,
                    close,
                });
            }
        }
    }

    prices.sort_by_key(|p| p.date);
    log::info!(
        "Loaded {} daily underlying prices from {}",
        prices.len(),
        path.display()
    );
    Ok(prices)
}
