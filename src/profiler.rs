//! Single-pass profiling engine for OPRA options data.
//!
//! Coordinates CMBP-1 decoding, contract resolution, event enrichment,
//! and tracker dispatch across all trading days.

use std::path::PathBuf;
use std::time::Instant;

use chrono::NaiveDate;

use crate::config::ProfilerConfig;
use crate::contract::ContractMap;
use crate::diagnostics::ProfileDiagnostics;
use crate::event::{Action, DayContext, OptionsEvent, Side};
use crate::loader::Cmbp1Loader;
use crate::options_math::moneyness::{Moneyness, MoneynessBuckets};
use crate::OptionsTracker;

use hft_statistics::time::regime::{midnight_utc_ns, time_regime, utc_offset_for_date};

const SENTINEL_I64: i64 = i64::MAX;

/// SemVer for the per-tracker JSON output schema.
///
/// Policy: MAJOR for breaking changes (field renames/removals), MINOR for
/// additive changes (new fields), PATCH for bugfix-only (no schema change).
/// Emitted in every `_provenance.output_schema_version` block.
pub const OUTPUT_SCHEMA_VERSION: &str = "2.1.0"; // 2.1.0: additive — NN_OsRatio.json (Cycle 27)

/// Profiling result.
pub struct ProfileResult {
    pub n_days: u32,
    pub total_events: u64,
    /// Cumulative count of CMBP-1 records whose decoding failed and were skipped
    /// across all days. Surfaced via `_provenance.diagnostics.decode_errors` in the
    /// per-tracker JSON output. Closes F-003 per hft-rules §8 (record diagnostics,
    /// never silently drop).
    pub total_decode_errors: u64,
    pub elapsed_secs: f64,
    pub reports: Vec<(String, serde_json::Value)>,
    /// Dispatch-loop diagnostic counters with conservation law.
    pub diagnostics: ProfileDiagnostics,
}

/// Underlying market data for a trading day (from EQUS or the built-in fallback).
pub struct DailyUnderlyingPrice {
    pub date: NaiveDate,
    pub open: f64,
    pub close: f64,
    /// Total consolidated equity SHARE volume for the day (EQUS `OhlcvMsg.volume`).
    /// The O/S-ratio denominator (Johnson-So 2012). `0` in built-in fallback mode
    /// (open/close-only), which yields a null O/S — see `os_ratio` + the run()
    /// fallback-mode warning.
    pub volume: u64,
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
    let mut total_decode_errors: u64 = 0;
    let mut diag = ProfileDiagnostics::default();
    let mut day_index: u32 = 0;

    // O/S-ratio (Johnson-So 2012) per-day numerator accumulation. The 5-35-DTE
    // option-contract volume is a DIFFERENT quantity from VolumeTracker's all-DTE
    // total, so it is accumulated fresh here (the one site with access to both the
    // option events and the equity-volume denominator). See os_ratio module.
    let os_enabled = config.os_ratio.enabled;
    let os_dte_min = config.os_ratio.dte_min;
    let os_dte_max = config.os_ratio.dte_max;
    let mut os_daily_opvol: Vec<(NaiveDate, u64)> = Vec::new();

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
        let (metadata, mut records) = loader.open()?;

        let dbn_date = time::Date::from_calendar_date(
            year,
            time::Month::try_from(month as u8).map_err(|e| format!("Invalid month: {}", e))?,
            day as u8,
        )
        .map_err(|e| format!("Invalid date for symbology: {}", e))?;

        let contract_map = ContractMap::from_dbn_metadata(&metadata, dbn_date);

        let mut day_events: u64 = 0;
        let mut day_opvol_os: u64 = 0;

        // FIND-UFO partial fix: interpolate underlying price between EQUS daily
        // open and close using session progress. This captures directional drift
        // (e.g., $194→$183 on earnings day) instead of freezing at open.
        // Full fix requires intraday EQUS feed at 1s/1m resolution (Axis L).
        // Both bounds are ET wall-clock hours converted ONCE: UTC = local - offset,
        // and `utc_offset_for_date` returns -4 (EDT) / -5 (EST).
        //
        // DEFECT FIXED 2026-08-14: the open bound was written `14 - utc_offset`.
        // 14:30 is 09:30 ET ALREADY EXPRESSED IN UTC under EST, so subtracting the
        // offset again applied the conversion twice: (14 - (-4)) = 18:30 UTC = 14:30 ET
        // (EDT), (14 - (-5)) = 19:30 UTC = 14:30 ET (EST). The close bound was always
        // correct — it converts an ET hour once — so the defect is ASYMMETRIC and the
        // window was 14:30-16:00 ET, 1.5h of the 6.5h session. Consequence: every event
        // before 14:30 ET took the `event_tod_ns <= rth_open_utc_ns` branch and froze
        // `underlying_estimate` at `underlying_open`, and the surviving 1.5h ran the
        // open->close drift 4.33x too fast because `rth_duration_ns` shrank with it.
        let rth_open_utc_ns: i64 =
            (9 - utc_offset as i64) * 3600 * 1_000_000_000 + 30 * 60 * 1_000_000_000; // 09:30 ET in UTC ns-since-midnight
        let rth_close_utc_ns: i64 = (16 - utc_offset as i64) * 3600 * 1_000_000_000; // 16:00 ET
        let rth_duration_ns: f64 = (rth_close_utc_ns - rth_open_utc_ns) as f64;

        // `records.by_ref()` keeps the iterator alive after the loop so we can read
        // the cumulative `decode_errors` counter below (F-003 fix; see loader.rs).
        for record in records.by_ref() {
            diag.total_decoded += 1;

            let contract = match contract_map.get(record.instrument_id) {
                Some(c) => c,
                None => {
                    diag.unknown_instrument += 1;
                    continue;
                }
            };

            // Flags-based observability (NOT filters — events still dispatched)
            let flags_raw = record.flags;
            if flags_raw & 0x04 != 0 {
                diag.observability.bad_book_flagged += 1;
            }
            if flags_raw & 0x08 != 0 {
                diag.observability.bad_ts_flagged += 1;
            }
            if flags_raw & 0x20 != 0 {
                diag.observability.snapshot_flagged += 1;
            }

            let bid_px = price_or_nan(record.bid_px);
            let ask_px = price_or_nan(record.ask_px);
            let trade_price = price_or_nan(record.price);

            // `record.action` is `loader::ACTION_UNAVAILABLE` (0) for CBBO-1s/1m
            // input, where the schema has no `action` field at all — those land
            // in `Action::Other` and are additionally counted by the loader's
            // `cbbo_action_unavailable`. See loader.rs module docs.
            let action = match record.action {
                b'T' => {
                    diag.observability.action_trade += 1;
                    Action::Trade
                }
                b'A' => {
                    diag.observability.action_quote += 1;
                    Action::Quote
                }
                _ => {
                    diag.observability.action_other += 1;
                    Action::Other
                }
            };

            let side = match record.side {
                b'B' => Side::Bid,
                b'A' => Side::Ask,
                _ => Side::None,
            };

            let dte = contract.dte(trading_date);

            // Per-event underlying estimate via time-weighted open→close interpolation.
            // Pre-market events use open; post-market use close; RTH linearly interpolates.
            let event_tod_ns = record.ts_event as i64 % (24 * 3600 * 1_000_000_000_i64);
            let underlying_estimate = if event_tod_ns <= rth_open_utc_ns {
                underlying_open
            } else if event_tod_ns >= rth_close_utc_ns {
                underlying_close
            } else {
                let progress = (event_tod_ns - rth_open_utc_ns) as f64 / rth_duration_ns;
                underlying_open + progress * (underlying_close - underlying_open)
            };

            let moneyness_ratio = if underlying_estimate > 0.0 {
                contract.strike / underlying_estimate
            } else {
                f64::NAN
            };
            let moneyness = match moneyness_buckets.classify(
                contract.strike,
                underlying_estimate,
                contract.contract_type,
            ) {
                Some(m) => m,
                None => {
                    diag.observability.moneyness_fallback += 1;
                    Moneyness::Otm
                }
            };

            let event = OptionsEvent {
                ts_event: record.ts_event as i64,
                instrument_id: record.instrument_id,
                contract,
                action,
                side,
                trade_price,
                trade_size: if action == Action::Trade {
                    record.size
                } else {
                    0
                },
                bid_px,
                ask_px,
                bid_sz: record.bid_sz,
                ask_sz: record.ask_sz,
                dte,
                moneyness,
                moneyness_ratio,
                underlying_price: underlying_estimate,
                publisher_id: record.publisher_id,
                bid_pb: record.bid_pb,
                ask_pb: record.ask_pb,
                ts_in_delta: record.ts_in_delta,
                flags: record.flags,
                ts_recv: record.ts_recv as i64,
            };

            // Reserved for future regime-aware trackers. Currently unused by all trackers.
            let regime = time_regime(record.ts_event as i64, utc_offset);

            diag.dispatched += 1;

            for tracker in trackers.iter_mut() {
                tracker.process_event(&event, regime);
            }

            // O/S numerator: 5-35-DTE option-contract trade volume (Johnson-So
            // footnote 9). `trade_size` is 0 for non-trades, so a zero-size trade
            // print contributes 0 — no explicit `trade_size != 0` guard is needed
            // here (VolumeTracker adds one only to skip its distribution updates).
            if os_enabled
                && event.is_trade()
                && event.dte >= os_dte_min
                && event.dte <= os_dte_max
            {
                day_opvol_os += event.trade_size as u64;
            }

            day_events += 1;
        }

        // Harvest day-level loader counters BEFORE `records` goes out of scope at
        // the end of this loop iteration (F-003 — closes silent decode-error
        // swallow; `unsupported_rtype` + `cbbo_action_unavailable` added 2026-08-01
        // with the dbn v0.64.0 rtype dispatch, same never-drop-silently rule).
        let day_decode_errors = records.decode_errors();
        total_decode_errors += day_decode_errors;
        diag.decode_errors += day_decode_errors;
        diag.unsupported_rtype += records.unsupported_rtype();
        diag.cbbo_action_unavailable += records.cbbo_action_unavailable();

        for tracker in trackers.iter_mut() {
            tracker.end_of_day(day_index);
        }

        total_events += day_events;
        if os_enabled {
            os_daily_opvol.push((trading_date, day_opvol_os));
        }
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

    let mut reports: Vec<(String, serde_json::Value)> = trackers
        .iter()
        .map(|t| (t.name().to_string(), t.finalize()))
        .collect();

    // O/S ratio (Johnson-So 2012) — appended after the trackers so it lands as the
    // last numbered output (e.g. 09_OsRatio.json with the default 8-tracker set; the
    // numeric prefix is positional). Computed post-loop because O/S joins the per-day
    // option volume with the per-day equity-volume denominator.
    if os_enabled {
        let eqvol_per_day: Vec<(NaiveDate, u64)> =
            underlying_prices.iter().map(|p| (p.date, p.volume)).collect();
        if eqvol_per_day.iter().all(|(_, v)| *v == 0) {
            log::warn!(
                "O/S enabled but all underlying volumes are 0 (fallback-price mode?); O/S will be null. Provide underlying_prices_file (EQUS OHLCV) for a runnable O/S."
            );
        }
        let os_report = crate::os_ratio::compute_os_series(
            &os_daily_opvol,
            &eqvol_per_day,
            config.os_ratio.rolling_window_days,
            os_dte_min,
            os_dte_max,
        );
        reports.push(("OsRatio".to_string(), os_report));
    }

    let elapsed = start.elapsed().as_secs_f64();
    log::info!(
        "OPRA profiling complete: {} days, {} events, {:.1}s ({:.0} evt/s)",
        day_index,
        total_events,
        elapsed,
        total_events as f64 / elapsed.max(0.001),
    );

    debug_assert!(
        diag.conservation_check(),
        "BUG: conservation law violated: total_decoded({}) != unknown_instrument({}) + dispatched({})",
        diag.total_decoded,
        diag.unknown_instrument,
        diag.dispatched
    );
    debug_assert_eq!(
        total_events, diag.dispatched,
        "BUG: total_events({}) diverged from diagnostics.dispatched({})",
        total_events, diag.dispatched
    );

    Ok(ProfileResult {
        n_days: day_index,
        total_events,
        total_decode_errors,
        elapsed_secs: elapsed,
        reports,
        diagnostics: diag,
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
        "output_schema_version": OUTPUT_SCHEMA_VERSION,
        "symbol": config.input.symbol,
        "dataset": "OPRA.PILLAR",
        "schema": "cmbp-1",
        "n_days": result.n_days,
        "total_events": result.total_events,
        "runtime_secs": result.elapsed_secs,
        "throughput_events_per_sec": result.total_events as f64 / result.elapsed_secs.max(0.001),
        "diagnostics_schema_version": crate::diagnostics::PROFILER_DIAGNOSTICS_SCHEMA_VERSION,
        "diagnostics": &result.diagnostics,
        "conservation_check": result.diagnostics.conservation_check(),
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
        let date =
            chrono::NaiveDate::from_num_days_from_ce_opt((days_since_epoch + 719_163) as i32);

        if let Some(d) = date {
            let open = record.open as f64 / 1e9;
            let close = record.close as f64 / 1e9;
            if open > 0.0 && close > 0.0 {
                prices.push(DailyUnderlyingPrice {
                    date: d,
                    open,
                    close,
                    volume: record.volume,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Integration: load the REAL EQUS NVDA OHLCV file and confirm the O/S-ratio
    /// denominator off-ramp (Cycle 27) is wired — `DailyUnderlyingPrice.volume` is
    /// populated with nonzero equity SHARE volume from `OhlcvMsg.volume`. `#[ignore]`
    /// because it reads a real (large, gitignored) data file; run on demand with
    /// `cargo test -- --ignored equs_volume_offramp`.
    #[test]
    #[ignore = "reads real EQUS data; run with --ignored to confirm the O/S denominator off-ramp"]
    fn test_equs_volume_offramp_wired_on_real_data() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../data/EQUS_SUMMARY/NVDA/ohlcv1d_2025-02-03_to_2026-03-05/equs-summary-20250203-20260305.ohlcv-1d.dbn.zst",
        );
        let prices = load_underlying_prices_from_equs(&path).expect("EQUS file must load");
        assert!(!prices.is_empty(), "EQUS load returned no days");
        let with_volume = prices.iter().filter(|p| p.volume > 0).count();
        assert!(
            with_volume > 0,
            "O/S denominator off-ramp FAILED: no day has nonzero equity volume (OhlcvMsg.volume not wired into DailyUnderlyingPrice)"
        );
        // Spot-check the Nov-14 NVDA day is present with a plausible share volume.
        if let Some(p) = prices
            .iter()
            .find(|p| p.date == NaiveDate::from_ymd_opt(2025, 11, 14).unwrap())
        {
            assert!(
                p.volume > 1_000_000,
                "NVDA daily volume should be > 1M shares, got {}",
                p.volume
            );
        }
    }
}
