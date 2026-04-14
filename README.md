# OPRA Statistical Profiler

High-performance Rust profiler for OPRA options microstructure analysis. Processes raw OPRA CMBP-1 `.dbn.zst` files in a single pass through composable analysis trackers, producing JSON statistical profiles with full provenance.

## Performance

| Metric | Value |
|--------|-------|
| Throughput | 4.1 million events/sec (release mode, single-threaded) |
| Dataset | 10.28 billion events across 8 trading days |
| Runtime | 42 minutes (Apple Silicon, NVMe SSD) |
| Bottleneck | zstd decompression (single-threaded per file stream) |
| I/O buffer | 1 MB per file (optimized for modern SSDs) |

## Architecture

```
.dbn.zst (CMBP-1) --> Cmbp1Loader --> SymbologyParser --> ContractRouter
                                                              |
                                                 +------------+------------+
                                                 |            |            |
                                                 v            v            v
                                          QualityTracker  SpreadTracker  ...
                                                 |            |            |
                                                 +------------+------------+
                                                              |
                                                              v
                                                        JSON profiles
```

### Data Flow

1. **Cmbp1Loader** reads `.dbn.zst` files with a 1 MB I/O buffer, streaming `CbboMsg` records.
2. **SymbologyParser** resolves instrument IDs to OCC option symbols using DBN metadata.
3. **ContractRouter** parses OCC symbols into `ContractInfo` (strike, expiration, call/put), classifies moneyness, computes DTE, and enriches each record into an `OptionsEvent`.
4. All enabled **trackers** receive every enriched event in a single pass -- no re-reads.
5. After all files are processed, each tracker produces a **JSON report** with a `_provenance` block.

### Single-Pass Composable Tracker Pattern

Every tracker implements the `OptionsTracker` trait:

```rust
pub trait OptionsTracker: Send {
    fn begin_day(&mut self, ctx: &DayContext);
    fn process_event(&mut self, event: &OptionsEvent, regime: u8);
    fn end_of_day(&mut self, day_index: u32);
    fn reset_day(&mut self);
    fn finalize(&self) -> serde_json::Value;
    fn name(&self) -> &str;
}
```

**Lifecycle per day**: `begin_day` (receives trading date, DST-aware UTC offset, underlying prices) -> `process_event` (called for every event) -> `end_of_day` (day-level aggregation) -> `reset_day` (prepare for next day). After all days: `finalize` produces the JSON report.

Trackers are independent and composable -- enable or disable any subset via config. No tracker depends on another tracker's output.

### hft-statistics Dependency

Statistical primitives are shared via the [`hft-statistics`](https://github.com/nagarx/hft-statistics.git) leaf crate:

- **WelfordAccumulator** -- online mean/variance (Welford 1962)
- **StreamingDistribution** -- quantile estimation via reservoir sampling
- **IntradayCurveAccumulator** -- 390-bin per-minute intraday aggregation (RTH 09:30-16:00 ET)
- **utc_offset_for_date / day_epoch_ns** -- DST-aware time utilities

The profiler has no dependency on `mbo-lob-reconstructor` or `mbo-statistical-profiler`.

## Trackers

| # | Tracker | Description |
|---|---------|-------------|
| 1 | **QualityTracker** | Event counts (total, quote, trade), quote-to-trade ratio, unique contracts per day, events/trades/contracts per day (Welford mean/std), sentinel ratio, per-DTE bucket event distribution (0DTE, 1DTE, 2-7DTE, other) |
| 2 | **SpreadTracker** | BBO bid-ask spread in USD and as percentage of mid-price. Breakdown by DTE bucket (4) and moneyness (5). 390-bin intraday spread and mid-price curves for 0DTE ATM contracts. Reservoir-sampled quantiles for all distributions |
| 3 | **ZeroDteTracker** | ATM 0DTE focused analysis: per-minute intraday curves for spread, premium (option mid-price), trade volume, and trade count. Aggregate distributions for call/put spread, call/put premium, trade size, and bid-ask size imbalance |
| 4 | **PremiumDecayTracker** | Theta decay curves for 0DTE ATM options: actual call and put premium by minute (390 bins), premium-to-spread ratio curves, daily open-to-close decay percentage (Welford statistics across days) |
| 5 | **VolumeTracker** | Trade volume and count by DTE bucket and moneyness. Call vs put volume totals, trade size distributions (overall, call, put), intraday volume curves (all options and 0DTE-only), daily volume/trade statistics, 0DTE volume share |
| 6 | **GreeksTracker** | Implied volatility via BSM Newton-Raphson solver (Brenner-Subrahmanyam 1988 initial guess). IV by DTE bucket, 390-bin intraday IV curve for 0DTE ATM calls. Delta, gamma, vega distributions for 0DTE ATM. IV sampled every 100th ATM quote for throughput |
| 7 | **PutCallRatioTracker** | Daily put-call ratio by volume and by trade count. Separate 0DTE PCR. 390-bin intraday PCR curves (all options and 0DTE-only), computed from separate call/put volume accumulators |
| 8 | **OptionsEffectiveSpreadTracker** | Effective spread (2 x |trade_price - mid|) vs quoted spread. Trade-through rate (fraction outside BBO). Effective spread by 6 size buckets (1, 2-5, 6-10, 11-50, 51-100, 100+), by DTE bucket (4), and by moneyness (5). 390-bin intraday effective spread curve for 0DTE ATM |

## Build Prerequisites

- **Rust 1.82+** (edition 2021)
- **hft-statistics** -- fetched automatically from GitHub: `https://github.com/nagarx/hft-statistics.git` (pinned to commit `e976ff7`)
- **dbn v0.20.0** -- fetched automatically from GitHub: `https://github.com/databento/dbn.git` (tag `v0.20.0`)

No other external system dependencies required. All crate dependencies are resolved by Cargo.

## Quick Start

### Build

```bash
cargo build --release
```

This produces the `profile_opra` binary at `target/release/profile_opra`.

### Run

```bash
RUST_LOG=info ./target/release/profile_opra --config configs/nvda_opra_8day.toml
```

The `RUST_LOG` environment variable controls log verbosity (`info`, `debug`, `warn`, `error`).

### Data Requirements

The profiler reads OPRA CMBP-1 `.dbn.zst` files (Databento format). You must adjust `data_dir` in your config to point to the directory containing your `.dbn.zst` files. The filename pattern uses `{date}` as a placeholder for the 8-digit date (YYYYMMDD).

Optionally provide an EQUS OHLCV `.dbn.zst` file via `underlying_prices_file` for accurate underlying prices. If not provided, the binary falls back to built-in NVDA prices for the 8-day November 2025 window. **The fallback only applies when `symbol = "NVDA"`** — any other symbol without `underlying_prices_file` is rejected with a hard error to prevent silent wrong-symbol pricing. Trading dates with no available underlying price (whether from file or fallback) also produce a hard error.

## Configuration Reference

Configuration is TOML-driven. All sections except `[input]` have defaults and can be omitted.

**Strict parsing**: All config structs use `#[serde(deny_unknown_fields)]`. Typos or misplaced keys (e.g., putting `reservoir_capacity` under `[buckets]` instead of top-level) produce a clear parse error rather than being silently ignored.

### `[input]` -- Data Source (required)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `data_dir` | path | (required) | Directory containing OPRA `.dbn.zst` files |
| `filename_pattern` | string | (required) | Filename pattern with `{date}` placeholder, e.g. `"opra-pillar-{date}.cmbp-1.dbn.zst"` |
| `symbol` | string | `"NVDA"` | Underlying symbol for contract filtering |
| `underlying_prices_file` | path | (none) | Path to EQUS OHLCV `.dbn.zst` for underlying open/close prices |
| `risk_free_rate` | f64 | `0.05` | Annualized risk-free rate for BSM calculations |
| `date_start` | string | (none) | Optional inclusive start date filter (`YYYY-MM-DD`) |
| `date_end` | string | (none) | Optional inclusive end date filter (`YYYY-MM-DD`) |

### `[trackers]` -- Tracker Enable/Disable

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `quality` | bool | `true` | Enable QualityTracker |
| `spread` | bool | `true` | Enable SpreadTracker |
| `premium_decay` | bool | `true` | Enable PremiumDecayTracker |
| `volume` | bool | `true` | Enable VolumeTracker |
| `greeks` | bool | `true` | Enable GreeksTracker |
| `zero_dte` | bool | `true` | Enable ZeroDteTracker |
| `put_call` | bool | `true` | Enable PutCallRatioTracker |
| `effective_spread` | bool | `true` | Enable OptionsEffectiveSpreadTracker |

### `[output]` -- Output Settings

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `output_dir` | path | `"output_opra"` | Directory for JSON output files |
| `write_summaries` | bool | `true` | Write JSON reports to disk |

### `[buckets]` -- Moneyness Boundaries

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `atm_range_pct` | f64 | `0.02` | ATM range as fraction of underlying price (+/- 2%) |
| `deep_range_pct` | f64 | `0.10` | Deep ITM/OTM boundary (10% from ATM) |

### Top-Level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `reservoir_capacity` | usize | `10000` | Reservoir sampling capacity for StreamingDistribution quantile estimation |

### Example Config

```toml
# Top-level (must appear before any [section] header)
reservoir_capacity = 10000

[input]
data_dir = "/path/to/opra/data"
filename_pattern = "opra-pillar-{date}.cmbp-1.dbn.zst"
symbol = "NVDA"
risk_free_rate = 0.05

[trackers]
quality = true
spread = true
premium_decay = true
volume = true
greeks = true
zero_dte = true
put_call = true
effective_spread = true

[output]
output_dir = "output_opra_nvda"
write_summaries = true

[buckets]
atm_range_pct = 0.02
deep_range_pct = 0.10
```

## Output Format

Each enabled tracker writes a numbered JSON file to the output directory:

```
output_opra_nvda/
    01_QualityTracker.json
    02_SpreadTracker.json
    03_ZeroDteTracker.json
    04_PremiumDecayTracker.json
    05_VolumeTracker.json
    06_GreeksTracker.json
    07_PutCallRatioTracker.json
    08_OptionsEffectiveSpreadTracker.json
```

File numbering corresponds to tracker registration order. Every JSON file includes a `_provenance` block containing:

```json
{
  "_provenance": {
    "profiler_version": "0.1.0",
    "symbol": "NVDA",
    "dataset": "OPRA.PILLAR",
    "schema": "cmbp-1",
    "n_days": 8,
    "total_events": 10280000000,
    "runtime_secs": 2520.0,
    "throughput_events_per_sec": 4079365.0,
    "config": { ... }
  }
}
```

The `config` field embeds the full TOML configuration used for the run, enabling exact reproduction.

## Dependencies

| Crate | Version / Source | Purpose |
|-------|-----------------|---------|
| `hft-statistics` | git: `github.com/nagarx/hft-statistics.git` (rev `e976ff7`) | Statistical primitives: Welford, StreamingDistribution, IntradayCurveAccumulator, DST-aware time |
| `dbn` | git: `github.com/databento/dbn.git` (v0.20.0) | CMBP-1 `.dbn.zst` decoding, `CbboMsg` record type, metadata/symbology |
| `ahash` | 0.8 | High-performance hashing for contract maps and unique-contract sets |
| `serde` | 1.0 (with `derive`) | Serialization/deserialization for config and report types |
| `serde_json` | 1.0 | JSON report output |
| `toml` | 0.8 | TOML config parsing |
| `log` | 0.4 | Logging facade |
| `env_logger` | 0.10 | Environment-based log configuration |
| `chrono` | 0.4 (no default features, `std` + `serde`) | Date handling for DTE computation and trading date parsing |
| `time` | 0.3 | Calendar date construction for DBN symbology API |
| `criterion` | 0.5 (dev-only) | Benchmarking framework |

### Release Profile

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
strip = true
```

Fat LTO and single codegen unit maximize throughput for the streaming event loop. Symbols are stripped from the release binary.

## Test Coverage

79 unit tests across 12 source files.

| Module | Tests | Coverage |
|--------|-------|----------|
| OCC symbol parsing (`contract.rs`) | 12 | Standard symbols, edge cases (6-char roots, penny strikes, leap year expiries), malformed input |
| BSM pricing + IV (`options_math/bsm.rs`) | 19 | Call/put pricing, IV Newton-Raphson convergence, Greeks (delta, gamma, vega, theta, rho), edge cases (deep ITM/OTM, near-expiry, zero time) |
| Moneyness classification (`options_math/moneyness.rs`) | 11 | ATM/ITM/OTM/Deep boundaries for calls and puts, edge cases at bucket boundaries |
| Report utilities (`report_utils.rs`) | 4 | DTE bucketing, moneyness indexing, curve finalization |
| QualityTracker | 4 | Event counting, quote/trade classification, DTE distribution, multi-day aggregation |
| SpreadTracker | 4 | Spread computation, DTE/moneyness bucketing, intraday curve population, reservoir sampling |
| ZeroDteTracker | 4 | ATM 0DTE filtering, spread/premium/volume curves, bid-ask imbalance |
| PremiumDecayTracker | 3 | Decay measurement, premium-to-spread ratio, multi-day statistics |
| VolumeTracker | 4 | Volume accounting, DTE/moneyness bucketing, call/put split, daily statistics |
| GreeksTracker | 4 | IV computation, sampling gate, delta/gamma/vega extraction, DTE bucket IV |
| PutCallRatioTracker | 4 | Daily PCR, intraday PCR curves, 0DTE-specific PCR, multi-day Welford |
| OptionsEffectiveSpreadTracker | 6 | Effective spread formula, trade-through detection, size bucketing, DTE/moneyness cross, intraday curve |

Run tests:

```bash
cargo test
```

## Design Decisions and Known Limitations

### Underlying Price Frozen at Day Open

OPRA data contains only options quotes and trades -- it does not include equity market quotes. The profiler uses the underlying stock's opening price (from EQUS OHLCV or built-in fallback) as the underlying estimate for the entire trading day. This means moneyness classification and BSM Greek computation use a static underlying price that does not track intraday equity moves. This is the best estimate available given the data; a live feed would require fusing OPRA with a separate equity data source.

### Time-to-Expiry Convention

- **0DTE contracts**: Use a 252-day trading year for annualized time-to-expiry in BSM calculations. Intraday time is computed from minutes remaining in the regular trading session.
- **Non-0DTE contracts**: Use 365 calendar days. This follows industry convention where short-dated options use trading days (avoiding weekend/holiday theta gaps) while longer-dated options use calendar days.

### DTE Bucketing

DTE is bucketed into 4 fixed categories:

| DTE | Bucket | Label |
|-----|--------|-------|
| 0 | 0 | `0dte` |
| 1 | 1 | `1dte` |
| 2-7 | 2 | `2_7dte` |
| 8+ | 3 | `other` |

These buckets are hardcoded in `report_utils.rs`. The choice reflects the 0DTE-focused analysis goal: most granularity where it matters most.

### Regime Parameter Reserved but Unused

The `process_event` method on the `OptionsTracker` trait accepts a `regime: u8` parameter. This is reserved for future time-of-day regime analysis (e.g., open auction, morning, midday, close). Currently all events are passed with `regime = 0`. The parameter exists to avoid a breaking trait change when regime-aware analysis is added.

### IV Sampling Interval

The `GreeksTracker` computes implied volatility via Newton-Raphson for every 100th qualifying ATM quote (the default sample interval). At 4.1M events/sec with thousands of ATM quotes per second, computing IV for every quote would be a throughput bottleneck with negligible statistical benefit. The interval is configurable via the `GreeksTracker::with_sample_interval(rate, capacity, interval)` constructor for use cases that need higher IV resolution.

### Moneyness Classification

Moneyness boundaries are configurable via `[buckets]` in the config:

| Classification | Call Condition | Put Condition |
|----------------|---------------|---------------|
| Deep ITM | strike/underlying < 1 - deep_range | strike/underlying > 1 + deep_range |
| ITM | strike/underlying < 1 - atm_range | strike/underlying > 1 + atm_range |
| ATM | within +/- atm_range of underlying | within +/- atm_range of underlying |
| OTM | strike/underlying > 1 + atm_range | strike/underlying < 1 - atm_range |
| Deep OTM | strike/underlying > 1 + deep_range | strike/underlying < 1 - deep_range |

Defaults: `atm_range_pct = 0.02` (+/- 2%), `deep_range_pct = 0.10` (10%).

## Source Layout

```
src/
    lib.rs                  -- Crate root, OptionsTracker trait
    config.rs               -- TOML config structs and defaults
    contract.rs             -- OCC symbol parser, ContractInfo, ContractMap
    event.rs                -- OptionsEvent, DayContext, Action, Side
    loader.rs               -- Cmbp1Loader (.dbn.zst streaming reader)
    profiler.rs             -- Single-pass profiling engine, file discovery, output writer
    report_utils.rs         -- Shared DTE/moneyness bucketing, curve finalization
    test_helpers.rs         -- Shared test fixtures (cfg(test) only)
    options_math/
        mod.rs              -- Options math module root
        bsm.rs              -- BSM pricing, Newton-Raphson IV, all Greeks
        moneyness.rs        -- Moneyness classification with configurable boundaries
    trackers/
        mod.rs              -- Tracker module root, re-exports
        quality.rs          -- QualityTracker
        spread.rs           -- SpreadTracker
        zero_dte.rs         -- ZeroDteTracker
        premium_decay.rs    -- PremiumDecayTracker
        volume.rs           -- VolumeTracker
        greeks.rs           -- GreeksTracker
        put_call.rs         -- PutCallRatioTracker
        effective_spread.rs -- OptionsEffectiveSpreadTracker
    bin/
        profile_opra.rs     -- CLI entry point
configs/
    nvda_opra_8day.toml     -- 8-day NVDA OPRA profiling config
    nvda_opra_nov14.toml    -- Single-day config (Nov 14 2025)
    nvda_opra_1day_test.toml -- Single-day test config
```

## License

LicenseRef-Proprietary. See `Cargo.toml` for details.
