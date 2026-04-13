# CODEBASE.md -- OPRA Statistical Profiler

Deep technical reference for the `opra-statistical-profiler` Rust crate. Covers architecture, every module, all formulas, configuration, and design decisions.

---

## 1. Overview

High-performance statistical profiler for OPRA options microstructure analysis. Processes raw OPRA CMBP-1 `.dbn.zst` files in a single pass through composable analysis trackers, producing JSON statistical profiles.

| Property | Value |
|----------|-------|
| Language | Rust (edition 2021, MSRV 1.82) |
| Trackers | 8 |
| Tests | 79 |
| Source LOC | ~4,151 (21 files including binary) |
| Throughput | ~4.1M events/sec |
| Architecture | Single-pass composable tracker dispatch |
| Input | OPRA CMBP-1 `.dbn.zst` (consolidated BBO + trades) |
| Output | Numbered JSON files with `_provenance` metadata |

**Dependencies:**
- `hft-statistics` (git, github.com/nagarx/hft-statistics.git, branch main) -- Welford, streaming distribution, intraday curve accumulator, DST-aware time utilities
- `dbn` v0.20.0 (git, github.com/databento/dbn.git) -- Databento binary format decoder
- `ahash` 0.8 -- Fast hashing for contract maps and unique contract sets
- `serde` + `serde_json` + `toml` -- Serialization, JSON output, TOML config
- `chrono` 0.4 -- Date handling (NaiveDate for trading dates, expirations)
- `time` 0.3 -- Required by dbn for Date/Month types in symbology resolution
- `log` + `env_logger` -- Structured logging
- `criterion` 0.5 (dev) -- Benchmarking

Local development uses `.cargo/config.toml` to patch `hft-statistics` to a local path (`../hft-statistics`).

The profiler does NOT depend on `mbo-statistical-profiler` or `MBO-LOB-reconstructor`. Statistical primitives are shared via the domain-independent `hft-statistics` crate.

---

## 2. Architecture -- Full Data Flow

```
.dbn.zst (OPRA CMBP-1)
    |
    v
Cmbp1Loader (1 MB I/O buffer, BufReader)
    |
    v
DynDecoder (dbn, VersionUpgradePolicy::AsIs)
    |
    +---> Metadata (symbology mappings)
    |         |
    |         v
    |     ContractMap (parse_occ_symbol for each SymbolMapping)
    |         AHashMap<u32, ContractInfo>
    |         OCC format: ROOT__YYMMDDCSSSSSSSS
    |
    v
For each CbboMsg record:
    |
    +---> contract_map.get(instrument_id) --> ContractInfo or skip
    |
    +---> price_or_nan(i64 nanodollars)
    |         SENTINEL_I64 (i64::MAX) or <= 0 --> f64::NAN
    |         otherwise: i64 as f64 / 1e9 --> f64 USD
    |
    +---> Action: 'T' = Trade, 'A' = Quote, other = Other
    +---> Side: 'B' = Bid, 'A' = Ask, other = None
    |
    +---> DTE = contract.expiration - trading_date (calendar days)
    +---> moneyness_ratio = strike / underlying_estimate
    +---> Moneyness = MoneynessBuckets.classify(strike, underlying, contract_type)
    |         5-class: DeepItm / Itm / Atm / Otm / DeepOtm
    |
    +---> OptionsEvent enrichment (all fields populated)
    |
    +---> Dispatch to ALL enabled trackers:
              tracker.process_event(&event, regime=0)
    |
    v
End of day:
    tracker.end_of_day(day_index)
    tracker.reset_day()
    |
    v
After all files:
    tracker.finalize() --> serde_json::Value
    |
    v
write_output():
    Insert _provenance (version, symbol, dataset, config, throughput)
    Write output_dir/{NN}_{TrackerName}.json
```

**Day processing lifecycle:**

1. Parse date from filename (YYYYMMDD via `{date}` placeholder in pattern)
2. Compute `utc_offset` and `day_epoch_ns` via `hft_statistics::time::regime`
3. Look up underlying open/close from EQUS prices (or fallback)
4. Build `DayContext`, call `begin_day()` on all trackers
5. Open `.dbn.zst`, build `ContractMap` from metadata symbology
6. Set `underlying_estimate = underlying_open` (static for entire day -- see D1)
7. Iterate all `CbboMsg` records, enrich into `OptionsEvent`, dispatch
8. Call `end_of_day()`, `reset_day()` on all trackers
9. Repeat for next file

**File discovery:** `discover_files()` scans `data_dir` for files matching `filename_pattern` with `{date}` placeholder (YYYYMMDD). Filters by optional `date_start`/`date_end` (inclusive, YYYY-MM-DD format). Results sorted chronologically.

**Underlying prices:** Loaded from EQUS OHLCV `.dbn.zst` via `load_underlying_prices_from_equs()`. Decodes `OhlcvMsg` records, converts nanodollar prices to USD. Falls back to hardcoded NVDA prices for 8-day Nov 2025 window if no file configured.

---

## 3. Module Reference

### src/lib.rs (79 lines)

Crate root. Declares public modules and the `OptionsTracker` trait.

**Trait: `OptionsTracker`** (Send-bounded for potential future parallelism)

| Method | Signature | Purpose |
|--------|-----------|---------|
| `begin_day` | `(&mut self, ctx: &DayContext)` | Receive day-level context (date, UTC offset, underlying prices) |
| `process_event` | `(&mut self, event: &OptionsEvent, regime: u8)` | Process one enriched options event. `regime` is reserved (always 0). |
| `end_of_day` | `(&mut self, day_index: u32)` | Finalize day-level accumulators |
| `reset_day` | `(&mut self)` | Clear day-level state for next day |
| `finalize` | `(&self) -> serde_json::Value` | Produce final JSON report |
| `name` | `(&self) -> &str` | Human-readable identifier |

---

### src/config.rs (148 lines)

TOML-driven profiler configuration. All fields have defaults except `input.data_dir` and `input.filename_pattern`.

**Structs:**

| Struct | Fields | Purpose |
|--------|--------|---------|
| `ProfilerConfig` | `input`, `trackers`, `output`, `buckets`, `reservoir_capacity` | Top-level config |
| `InputConfig` | `data_dir`, `filename_pattern`, `symbol`, `underlying_prices_file`, `risk_free_rate`, `date_start`, `date_end` | Data source |
| `TrackerConfig` | 8 booleans (one per tracker) | Which trackers to enable |
| `OutputConfig` | `output_dir`, `write_summaries` | Output paths |
| `BucketConfig` | `atm_range_pct`, `deep_range_pct` | Moneyness classification thresholds |

**Defaults:**

| Field | Default | Type |
|-------|---------|------|
| `symbol` | `"NVDA"` | String |
| `risk_free_rate` | `0.05` (5%) | f64 |
| `reservoir_capacity` | `10_000` | usize |
| `output_dir` | `"output_opra"` | PathBuf |
| `write_summaries` | `true` | bool |
| `atm_range_pct` | `0.02` (+/- 2%) | f64 |
| `deep_range_pct` | `0.10` (10%) | f64 |
| All tracker enables | `true` | bool |

**Method:** `ProfilerConfig::from_file(path) -> Result<Self>` -- reads TOML file, deserializes via serde.

---

### src/contract.rs (257 lines, 12 tests)

OCC option symbol parser and contract metadata management.

**OCC Symbol Format:**
```
NVDA  251114C00185000
^^^^^^ ^^^^^^ ^^^^^^^^
root   YYMMDD strike*1000
(1-6   expiry C/P
chars, padded)
```

- Root: 1-6 characters, left-padded with spaces to 6 chars in raw symbol
- Expiry: YYMMDD (year = 2000 + YY)
- Type: `C` = Call, `P` = Put
- Strike: 8 digits, price * 1000 (e.g., `00185000` = $185.000)

**Types:**

| Type | Description |
|------|-------------|
| `ContractType` | Enum: `Call`, `Put`. Derives Hash, Serialize, Deserialize. |
| `ContractInfo` | `instrument_id: u32`, `underlying: String`, `expiration: NaiveDate`, `contract_type: ContractType`, `strike: f64` (USD), `occ_symbol: String` |
| `ContractMap` | Wrapper around `AHashMap<u32, ContractInfo>` |

**Functions:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `parse_occ_symbol` | `(occ_symbol: &str, instrument_id: u32) -> Option<ContractInfo>` | Parse OCC symbol. Returns `None` for malformed inputs. |
| `ContractInfo::dte` | `(&self, trading_date: NaiveDate) -> i64` | Calendar days to expiration |
| `ContractInfo::is_zero_dte` | `(&self, trading_date: NaiveDate) -> bool` | `expiration == trading_date` |
| `ContractMap::from_dbn_metadata` | `(metadata: &Metadata, date: time::Date) -> Self` | Build map from dbn point-in-time symbology. Uses `metadata.symbol_map_for_date()`. |
| `ContractMap::get` | `(&self, instrument_id: u32) -> Option<&ContractInfo>` | O(1) lookup |
| `ContractMap::len` | `(&self) -> usize` | Total number of contracts |
| `ContractMap::is_empty` | `(&self) -> bool` | Whether the map is empty |
| `ContractMap::iter` | `(&self) -> impl Iterator<Item = (&u32, &ContractInfo)>` | Iterate all contracts |
| `ContractMap::count_by_type` | `(&self) -> (usize, usize)` | Returns (calls, puts) |

**parse_occ_symbol algorithm:**
1. Trim trailing whitespace
2. Reject if length < 15
3. Extract root (chars 0..6, trimmed), reject if empty
4. Parse date (chars 6..12): `2000 + YY`, `MM`, `DD` -> `NaiveDate`
5. Parse call/put byte at position 12: `b'C'` or `b'P'`
6. Parse strike string (chars 13..): must be >= 8 digits, divide by 1000.0
7. Return `ContractInfo` or `None`

---

### src/event.rs (141 lines)

Enriched options event type and day-level context.

**DayContext:**

| Field | Type | Description |
|-------|------|-------------|
| `trading_date` | `NaiveDate` | Current trading day |
| `utc_offset` | `i32` | UTC offset in hours (-5 EST, -4 EDT) |
| `day_epoch_ns` | `i64` | Midnight UTC in nanoseconds (reserved -- see D8) |
| `underlying_open` | `f64` | Underlying stock opening price (USD) |
| `underlying_close` | `f64` | Underlying stock closing price (USD) |

**Action enum:** `Quote` (BBO change, `'A'`), `Trade` (execution, `'T'`), `Other` (modify/cancel/clear)

**Side enum:** `Bid` (`'B'`), `Ask` (`'A'`), `None`

**OptionsEvent<'a>:**

| Field | Type | Description |
|-------|------|-------------|
| `ts_event` | `i64` | UTC nanoseconds since epoch |
| `instrument_id` | `u32` | dbn instrument_id |
| `contract` | `&'a ContractInfo` | Parsed contract metadata (borrowed from ContractMap) |
| `action` | `Action` | Quote or Trade |
| `side` | `Side` | Aggressor side |
| `trade_price` | `f64` | USD. `NAN` for quote-only events. |
| `trade_size` | `u32` | Contracts. 0 for quote-only events. |
| `bid_px` / `ask_px` | `f64` | Best bid/ask in USD. `NAN` if sentinel. |
| `bid_sz` / `ask_sz` | `u32` | Best bid/ask size in contracts |
| `dte` | `i64` | Days to expiration |
| `moneyness` | `Moneyness` | 5-class classification |
| `moneyness_ratio` | `f64` | strike / underlying_price |
| `underlying_price` | `f64` | Current underlying estimate (USD) |

**Helper methods:**

| Method | Formula / Logic | Notes |
|--------|----------------|-------|
| `option_mid()` | `(bid + ask) / 2` | Returns NAN if either side NAN/infinite or `ask <= bid` (strict inequality -- see D6) |
| `spread()` | `ask - bid` | Returns NAN if either side NAN/infinite or `ask < bid` (allows `ask == bid` -- see D6) |
| `spread_pct()` | `spread / mid * 100` | Returns NAN if mid <= 0 or non-finite |
| `has_valid_bbo()` | `bid.is_finite() && ask.is_finite() && bid > 0 && ask > bid` | Strict validity gate |
| `is_zero_dte()` | `dte == 0` | |
| `is_trade()` | `action == Trade` | |
| `is_call()` | `contract.contract_type == Call` | |
| `is_atm()` | `moneyness == Atm` | |

---

### src/loader.rs (87 lines)

Streaming loader for OPRA CMBP-1 `.dbn.zst` files.

**Constants:**

| Constant | Value | Rationale |
|----------|-------|-----------|
| `IO_BUFFER_SIZE` | `1_048_576` (1 MB) | Optimal throughput on modern SSDs. Matches MBO profiler pattern. |

**Types:**

| Type | Description |
|------|-------------|
| `Cmbp1Loader` | Holds `PathBuf`, validates file existence on construction |
| `Cmbp1RecordIterator<'a>` | Wraps `DynDecoder<BufReader<File>>` with record counter |

**Cmbp1Loader::open() -> (Metadata, Cmbp1RecordIterator)**

Opens the `.dbn.zst` file with 1 MB buffered reader, creates a `DynDecoder` with `VersionUpgradePolicy::AsIs`, clones metadata for symbology, returns metadata + iterator.

**Iterator behavior:** Calls `decoder.decode_record::<CbboMsg>()`. On decode error, logs a warning and continues to next record (tolerant of rare corrupt records). Counts all successfully yielded records via `.count()`.

---

### src/profiler.rs (371 lines)

Single-pass profiling engine. Orchestrates file discovery, day processing, event enrichment, and tracker dispatch.

**Constants:**

| Constant | Value | Purpose |
|----------|-------|---------|
| `SENTINEL_I64` | `i64::MAX` | dbn sentinel for undefined prices |

**Types:**

| Type | Fields | Description |
|------|--------|-------------|
| `ProfileResult` | `n_days: u32`, `total_events: u64`, `elapsed_secs: f64`, `reports: Vec<(String, Value)>` | Profiling output |
| `DailyUnderlyingPrice` | `date: NaiveDate`, `open: f64`, `close: f64` | Per-day underlying OHLCV |

**Functions:**

| Function | Description |
|----------|-------------|
| `run(config, trackers, underlying_prices) -> Result<ProfileResult>` | Main orchestration. Discovers files, iterates days, enriches events, dispatches to trackers. |
| `price_or_nan(price_i64: i64) -> f64` | Converts i64 nanodollar price to f64 USD. Returns `NAN` for `SENTINEL_I64` or `<= 0`. Formula: `price_i64 as f64 / 1e9`. Inlined. |
| `write_output(config, result) -> Result<()>` | Creates output directory, writes `{NN}_{TrackerName}.json` with `_provenance` object injected. |
| `discover_files(config) -> Result<Vec<(String, PathBuf)>>` | Scans `data_dir` for files matching pattern with `{date}` placeholder. Extracts YYYYMMDD, filters by date range, sorts chronologically. |
| `load_underlying_prices_from_equs(path) -> Result<Vec<DailyUnderlyingPrice>>` | Decodes EQUS OHLCV `.dbn.zst` (small, ~8 KB). Converts `OhlcvMsg` timestamps to dates via epoch-day arithmetic (`ts / 1e9 / 86400 + 719163` for CE day conversion). Converts nanodollar open/close to USD. |

**Event enrichment (inside run()):**

```rust
let bid_px = price_or_nan(record.levels[0].bid_px);
let ask_px = price_or_nan(record.levels[0].ask_px);
let trade_price = price_or_nan(record.price);
let action = match record.action as u8 { b'T' => Trade, b'A' => Quote, _ => Other };
let side = match record.side as u8 { b'B' => Bid, b'A' => Ask, _ => None };
let dte = contract.dte(trading_date);
let moneyness_ratio = contract.strike / underlying_estimate;
let moneyness = moneyness_buckets.classify(...).unwrap_or(Otm);
```

Key detail: `underlying_estimate = underlying_open` for the entire day (see D1).

**Provenance object (injected into every JSON output):**

```json
{
  "profiler_version": "0.1.0",
  "symbol": "NVDA",
  "dataset": "OPRA.PILLAR",
  "schema": "cmbp-1",
  "n_days": 8,
  "total_events": 32000000,
  "runtime_secs": 7.8,
  "throughput_events_per_sec": 4100000,
  "config": { "...full config..." }
}
```

---

### src/report_utils.rs (103 lines, 4 tests)

Shared constants and utilities for DTE bucketing, moneyness indexing, and intraday curve finalization.

**Constants:**

| Constant | Value |
|----------|-------|
| `DTE_LABELS` | `["0dte", "1dte", "2_7dte", "other"]` |
| `MONEYNESS_LABELS` | `["deep_itm", "itm", "atm", "otm", "deep_otm"]` |

**Functions:**

| Function | Mapping | Description |
|----------|---------|-------------|
| `dte_bucket_index(dte: i64) -> usize` | 0->0, 1->1, 2..=7->2, _->3 | Maps DTE to 4 buckets. Negative DTE maps to bucket 3 ("other"). |
| `moneyness_index(m: Moneyness) -> usize` | DeepItm->0, Itm->1, Atm->2, Otm->3, DeepOtm->4 | Maps 5-class moneyness to array index |
| `finalize_curve(acc: &IntradayCurveAccumulator, value_key: &str) -> Vec<Value>` | Filters to bins with count > 0 | Converts IntradayCurveAccumulator to JSON array with `minutes_since_open`, `value_key` (mean), `std`, `count` |

---

### src/test_helpers.rs (102 lines, test-only)

Factory functions for constructing synthetic `OptionsEvent` instances in tests. Only compiled under `#[cfg(test)]`.

**Constants:**

| Constant | Value |
|----------|-------|
| `NS_PER_HOUR` | `3_600_000_000_000` |
| `NS_PER_MINUTE` | `60_000_000_000` |
| `RTH_OPEN_UTC_NS` | `14 * NS_PER_HOUR + 30 * NS_PER_MINUTE` (14:30 UTC = 09:30 ET in EST) |

**Functions:**

| Function | Returns | Key Details |
|----------|---------|-------------|
| `make_day_context()` | `DayContext` | Date: 2025-11-14, utc_offset: -5, underlying: 190.0/190.0 |
| `make_contract_call(strike)` | `ContractInfo` | NVDA call, expiration 2025-11-14, instrument_id=1 |
| `make_contract_put(strike)` | `ContractInfo` | NVDA put, expiration 2025-11-14, instrument_id=2 |
| `make_quote_event(contract, bid, ask, dte, moneyness)` | `OptionsEvent` | Action::Quote, Side::None, ts=RTH_OPEN+5min, underlying=190.0 |
| `make_trade_event(contract, price, size, bid, ask, dte, moneyness)` | `OptionsEvent` | Action::Trade, Side::Ask, ts=RTH_OPEN+10min, underlying=190.0 |

---

### src/bin/profile_opra.rs (128 lines)

CLI binary entry point.

**Usage:** `profile_opra --config <path>`

**Flow:**
1. Parse `--config` argument
2. Load `ProfilerConfig` from TOML file
3. Load underlying prices from EQUS file or fallback (8-day NVDA Nov 2025 hardcoded)
4. Instantiate enabled trackers based on `config.trackers.*` booleans
5. Call `profiler::run()` then `profiler::write_output()`
6. Log final summary with day count, event count, throughput

**Tracker instantiation order:** QualityTracker, SpreadTracker, ZeroDteTracker, PremiumDecayTracker, VolumeTracker, GreeksTracker, PutCallRatioTracker, OptionsEffectiveSpreadTracker.

**Fallback prices:** 8 NVDA trading days (2025-11-13 to 2025-11-24), open/close from EQUS OHLCV. Used when no `underlying_prices_file` is configured.

---

## 4. Options Math

### BSM (src/options_math/bsm.rs, 440 lines, 19 tests)

Black-Scholes-Merton European option pricing, implied volatility solver, and Greek computation.

**Constants:**

| Constant | Value | Purpose |
|----------|-------|---------|
| `MIN_T` | `1e-6` | Minimum time-to-expiry (~1 second). Avoids division by zero. |
| `MAX_IV_ITER` | `100` | Maximum Newton-Raphson iterations |
| `IV_TOL` | `1e-8` | IV convergence tolerance |
| `MAX_IV` | `10.0` | Maximum IV cap (1000% annualized) |
| `MIN_IV` | `1e-6` | Minimum IV floor |

**Core Formulas (Black & Scholes 1973, Merton 1973):**

```
d1 = [ln(S/K) + (r + sigma^2/2) * T] / (sigma * sqrt(T))
d2 = d1 - sigma * sqrt(T)

Call:  C = S * N(d1) - K * e^(-rT) * N(d2)
Put:   P = K * e^(-rT) * N(-d2) - S * N(-d1)
```

**Greeks:**

| Greek | Formula | Notes |
|-------|---------|-------|
| Delta (call) | `N(d1)` | |
| Delta (put) | `N(d1) - 1` | |
| Gamma | `N'(d1) / (S * sigma * sqrt(T))` | Same for calls and puts |
| Theta (call) | `[-S * N'(d1) * sigma / (2*sqrt(T)) - r * K * e^(-rT) * N(d2)] / 365` | Per calendar day |
| Theta (put) | `[-S * N'(d1) * sigma / (2*sqrt(T)) + r * K * e^(-rT) * N(-d2)] / 365` | Per calendar day |
| Vega | `S * sqrt(T) * N'(d1)` | Same for calls and puts |

**Edge case handling:** When `t < MIN_T` or `sigma < MIN_IV` or `s <= 0` or `k <= 0`, pricing functions return intrinsic value (`max(S-K, 0)` for calls, `max(K-S, 0)` for puts), Greeks return 0 (or 1/-1 for deep ITM/OTM delta).

**norm_cdf (Standard Normal CDF):**

Hart (1968) rational approximation via Abramowitz & Stegun 7.1.26.
- Accuracy: |error| < 7.5e-8
- Implementation: `N(x) = 0.5 * erfc(-x / sqrt(2))`
- `erfc_approx(x)` for x >= 0: Horner-form polynomial `t * (a1 + t*(a2 + t*(a3 + t*(a4 + t*a5)))) * exp(-x^2)` where `t = 1/(1 + 0.3275911*x)`
- Coefficients: `a1=0.254829592, a2=-0.284496736, a3=1.421413741, a4=-1.453152027, a5=1.061405429`
- For x < 0: `erfc(-x) = 2 - erfc(x)`

**norm_pdf:** `N'(x) = exp(-x^2/2) / sqrt(2*pi)`

**Implied Volatility:**

Newton-Raphson with Brenner-Subrahmanyam (1988) initial guess:

```
sigma_0 = sqrt(2 * pi / T) * C / S
```

Clamped to `[0.10, MAX_IV]` initially. Each iteration: `sigma -= (price(sigma) - market_price) / vega(sigma)`, clamped to `[MIN_IV, MAX_IV]`. Returns `None` if:
- Market price is non-finite, zero, or negative
- `s <= 0` or `k <= 0` or `t < MIN_T`
- Market price below intrinsic value minus 0.001 (arbitrage violation)
- Vega drops below `1e-12` (flat region)
- 100 iterations without convergence to `IV_TOL`

**Time conversion:**

| Function | Formula | Notes |
|----------|---------|-------|
| `minutes_to_years(minutes_remaining)` | `max(minutes / (252 * 390), MIN_T)` | 252 trading days/year, 390 min/trading day. Clamped to MIN_T. |

---

### Moneyness (src/options_math/moneyness.rs, 220 lines, 11 tests)

5-class moneyness classification based on strike-to-underlying ratio.

**Enum: `Moneyness`** -- `DeepItm`, `Itm`, `Atm`, `Otm`, `DeepOtm`

Implements `Display` (lowercase with underscores: `"deep_itm"`, `"atm"`, etc.), `Hash`, `Serialize`, `Deserialize`.

**Struct: `MoneynessBuckets`**

| Field | Default | Description |
|-------|---------|-------------|
| `atm_range` | `0.02` | Half-width of ATM band (+/- 2%) |
| `deep_range` | `0.10` | Boundary for deep ITM/OTM (10% from 1.0) |

**Classification algorithm (`classify()`):**

```
ratio = strike / underlying_price

For calls:
  DeepItm:  ratio < 1.0 - deep_range   (e.g., < 0.90)
  Itm:      ratio < 1.0 - atm_range    (e.g., < 0.98)
  Atm:      ratio in [0.98, 1.02]
  Otm:      ratio > 1.0 + atm_range    (e.g., > 1.02)
  DeepOtm:  ratio > 1.0 + deep_range   (e.g., > 1.10)

For puts: ITM/OTM directions are reversed.
  DeepItm:  ratio > 1.0 + deep_range
  Itm:      ratio > 1.0 + atm_range
  Otm:      ratio < 1.0 - atm_range
  DeepOtm:  ratio < 1.0 - deep_range
```

Returns `None` if `underlying_price <= 0`, non-finite, or strike non-finite.

Priority: Deep checks evaluate first. If both deep ITM and deep OTM are false, then ITM/OTM checks run. If neither, the result is ATM.

**`MoneynessBuckets::ratio(strike, underlying_price) -> f64`:** Returns `strike / underlying_price` or NAN if underlying invalid.

---

## 5. All 8 Trackers

All trackers implement `OptionsTracker`. Each tracker is independently enable/disable via TOML config. All use streaming accumulators with bounded memory.

---

### 5.1 QualityTracker (src/trackers/quality.rs, 229 lines, 4 tests)

**Purpose:** Data quality and event count statistics. Entry-level validation of the OPRA data feed.

**Key metrics:**
- Total events, quote/trade breakdown, trade percentage
- Quote-to-trade ratio: `quote_events / trade_events`
- Sentinel event count and percentage (quotes with invalid BBO)
- Unique contracts per day and total
- Events/trades/contracts per day (mean, std, min, max)
- DTE distribution (4 buckets: 0DTE, 1DTE, 2-7DTE, other)

**hft-statistics types:** `WelfordAccumulator` (3 instances: events_per_day, trades_per_day, contracts_per_day)

**Additional dependencies:** `ahash::AHashSet<u32>` for unique contract tracking (per-day and total)

**Per-day state (reset via `reset_day()`):**
- `day_events`, `day_trades`: u64 counters
- `unique_contracts_day`: AHashSet, cleared each day

**Sentinel detection:** An event is "sentinel" if it is NOT a trade AND does NOT have a valid BBO. This catches quote updates where bid/ask are undefined (sentinel i64::MAX).

**Output JSON keys:** `tracker`, `n_days`, `total_events`, `quote_events`, `trade_events`, `trade_pct`, `quote_to_trade_ratio`, `sentinel_events`, `sentinel_pct`, `unique_contracts_total`, `events_per_day` (mean/std/min/max), `trades_per_day` (mean/std), `contracts_per_day` (mean/std), `dte_distribution` (dte_0/dte_1/dte_2_7/dte_other).

---

### 5.2 SpreadTracker (src/trackers/spread.rs, 224 lines, 4 tests)

**Purpose:** BBO spread distribution by DTE, moneyness, and time-of-day. Focused on 0DTE ATM spreads.

**Key metrics:**
- All-contract spread (USD and %) with full distribution (reservoir sampling)
- 0DTE ATM spread (USD and %) with full distribution
- Per-moneyness spread (5 buckets, Welford mean/std)
- Per-DTE spread (4 buckets, Welford mean/std)
- 0DTE ATM intraday spread curve (390-bin, per-minute)
- 0DTE ATM intraday mid-price curve (390-bin, per-minute)

**Filters:** Only processes events with `has_valid_bbo()`. Spread must be finite.

**hft-statistics types:**
- `StreamingDistribution` (4 instances: all_spread_usd, all_spread_pct, dte0_atm_spread_usd, dte0_atm_spread_pct)
- `WelfordAccumulator` (9 instances: 5 per-moneyness + 4 per-DTE)
- `IntradayCurveAccumulator` (2 instances: dte0_atm intraday spread and mid)

**Constructor:** `SpreadTracker::new(reservoir_capacity: usize)`

**Output JSON keys:** `tracker`, `n_days`, `all_spread_usd`, `all_spread_pct`, `dte0_atm_spread_usd`, `dte0_atm_spread_pct`, `spread_by_moneyness` (deep_itm/itm/atm/otm/deep_otm, each with mean/std/count), `spread_by_dte` (0dte/1dte/2_7dte/other, each with mean/std/count), `dte0_atm_intraday_spread_curve`, `dte0_atm_intraday_mid_curve`.

---

### 5.3 PremiumDecayTracker (src/trackers/premium_decay.rs, 216 lines, 3 tests)

**Purpose:** Theta decay curve for 0DTE ATM options. Tracks how premium decays throughout the trading day. Key deliverable: actual theta decay vs BSM theoretical, providing empirical data for the 0DTE strategy's time decay budget.

**Key metrics:**
- Daily call/put decay percentage: `(first_mid - last_mid) / first_mid * 100`
- Intraday call/put premium curves (390-bin, per-minute)
- Premium-to-spread ratio curves (`mid / spread`): how many spreads of "edge" exist
- Number of 0DTE days observed

**Filters:** Only processes events that are 0DTE, ATM, and have valid BBO with positive mid.

**Day-level state (reset via `reset_day()`):**
- `day_first_call_mid`, `day_last_call_mid`: Option<f64>
- `day_first_put_mid`, `day_last_put_mid`: Option<f64>

**hft-statistics types:**
- `IntradayCurveAccumulator` (4 instances: actual call/put curves, premium-to-spread call/put)
- `WelfordAccumulator` (2 instances: daily_call_decay_pct, daily_put_decay_pct)

**Stored fields:** `_risk_free_rate: f64` -- stored for future BSM theoretical theta comparison (see D5).

**Constructor:** `PremiumDecayTracker::new(risk_free_rate: f64)`

**Output JSON keys:** `tracker`, `n_days`, `n_0dte_days`, `daily_call_decay_pct` (mean/std/count), `daily_put_decay_pct` (mean/std/count), `intraday_call_premium_curve`, `intraday_put_premium_curve`, `intraday_call_premium_to_spread`, `intraday_put_premium_to_spread`.

---

### 5.4 VolumeTracker (src/trackers/volume.rs, 251 lines, 4 tests)

**Purpose:** Trade volume distribution by DTE, moneyness, and time-of-day.

**Key metrics:**
- Total trades and volume
- Call/put volume split and put-call ratio (volume-based): `put_volume / call_volume`
- 0DTE volume share percentage: `dte_volume[0] / total_volume * 100`
- Trade size distribution (full reservoir, separate for call/put)
- Per-DTE volume and trade counts (4 buckets)
- Per-moneyness volume (5 buckets)
- Daily volume and trade counts (mean, std)
- Intraday all-contract and 0DTE volume curves (390-bin)

**Filters:** Only processes trade events (`is_trade()`) with `trade_size > 0`.

**hft-statistics types:**
- `StreamingDistribution` (3 instances: trade_size_dist, call_trade_size, put_trade_size)
- `WelfordAccumulator` (2 instances: daily_volume, daily_trades)
- `IntradayCurveAccumulator` (2 instances: intraday_all_volume, intraday_0dte_volume)

**Constructor:** `VolumeTracker::new(reservoir_capacity: usize)`

**Output JSON keys:** `tracker`, `n_days`, `total_trades`, `total_volume`, `call_volume`, `put_volume`, `put_call_ratio_volume`, `dte0_volume_share_pct`, `trade_size_distribution`, `call_trade_size`, `put_trade_size`, `volume_by_dte`, `volume_by_moneyness`, `daily_volume` (mean/std), `daily_trades` (mean/std), `intraday_all_volume_curve`, `intraday_0dte_volume_curve`.

---

### 5.5 GreeksTracker (src/trackers/greeks.rs, 271 lines, 4 tests)

**Purpose:** Implied volatility and Greek computation for ATM options. Computes IV from BSM for ATM contracts by DTE bucket. Tracks delta, gamma, vega distributions for 0DTE ATM.

**Key metrics:**
- 0DTE ATM call/put IV distributions (reservoir)
- IV by DTE bucket (4 buckets, Welford)
- Intraday 0DTE ATM IV curve (390-bin)
- 0DTE ATM delta (absolute value), gamma, vega distributions (reservoir)
- IV computation success rate: `iv_computed / (iv_computed + iv_failed)`
- ATM quote count and sample interval

**Filters:** Only processes ATM events with valid BBO and positive mid. Non-ATM events are entirely skipped.

**IV sampling gate:** IV computation is expensive. Only computed every Nth qualifying ATM quote (`atm_quote_count % iv_sample_interval != 0` -> skip). Default interval: 100.

**Important:** `atm_quote_count` is NOT reset across days -- it is a continuous counter (see D4).

**Time-to-expiry calculation:**
- 0DTE: Estimate minutes remaining from timestamp. Close at 16:00 ET. `remaining_ns = (close_utc_ns - event_tod_ns).max(0)`. Convert to minutes, then `bsm::minutes_to_years()` (252-day year, 390 min/day).
- Non-0DTE: `dte as f64 / 365.0` (calendar days, see D2). Clamped to min 1e-6.

**IV filter:** Only accepts IV where `sigma.is_finite() && sigma > 0.0 && sigma < 5.0` (see D3). Outside this range counts as `iv_failed`.

**Greek computation:** Delta uses absolute value (`d.abs()`). Gamma and vega are stored as-is. All require finite values to be accepted into distributions.

**hft-statistics types:**
- `StreamingDistribution` (5 instances: dte0_atm_call_iv, dte0_atm_put_iv, delta, gamma, vega)
- `WelfordAccumulator` (4 instances: per-DTE-bucket IV)
- `IntradayCurveAccumulator` (1 instance: dte0_atm_iv_curve)

**Constructors:**
- `GreeksTracker::new(risk_free_rate, reservoir_capacity)` -- default iv_sample_interval=100
- `GreeksTracker::with_sample_interval(risk_free_rate, reservoir_capacity, iv_sample_interval)` -- explicit interval (clamped to min 1)

**Output JSON keys:** `tracker`, `n_days`, `atm_quote_count`, `iv_sample_interval`, `iv_computed`, `iv_failed`, `iv_success_rate`, `dte0_atm_call_iv`, `dte0_atm_put_iv`, `dte0_atm_delta`, `dte0_atm_gamma`, `dte0_atm_vega`, `iv_by_dte` (0dte/1dte/2_7dte/other, each with mean/std/count), `intraday_dte0_atm_iv_curve`.

---

### 5.6 ZeroDteTracker (src/trackers/zero_dte.rs, 263 lines, 4 tests)

**Purpose:** 0DTE ATM-focused analysis for strategy validation. The most critical tracker for the 0DTE options strategy.

**Key metrics:**
- Total 0DTE ATM events and trades
- 0DTE day count
- ATM call/put spread distributions (reservoir)
- ATM call/put premium distributions (reservoir)
- ATM trade size distribution (reservoir)
- ATM bid-ask size imbalance: `(bid_sz - ask_sz) / (bid_sz + ask_sz)`
- Trades per 0DTE day, volume per 0DTE day (Welford)
- 6 intraday curves (390-bin each): call/put spread, call/put premium, trade volume, trade count

**Filters:** Only processes events that are both 0DTE AND ATM.

**Day-level state (reset via `reset_day()`):**
- `day_trade_count`, `day_trade_volume`, `day_0dte_atm_events`: u64 counters

**hft-statistics types:**
- `StreamingDistribution` (6 instances: call/put spread, call/put premium, trade size, bid-ask imbalance)
- `WelfordAccumulator` (2 instances: trades_per_day, volume_per_day)
- `IntradayCurveAccumulator` (6 instances: call/put spread curves, call/put mid curves, trade volume, trade count)

**Constructor:** `ZeroDteTracker::new(reservoir_capacity: usize)`

**Output JSON keys:** `tracker`, `n_days`, `n_0dte_days`, `total_0dte_atm_events`, `total_0dte_atm_trades`, `atm_call_spread`, `atm_put_spread`, `atm_call_premium`, `atm_put_premium`, `atm_trade_size`, `atm_bid_ask_imbalance`, `trades_per_0dte_day` (mean/std/count), `volume_per_0dte_day` (mean/std), `intraday_atm_call_spread`, `intraday_atm_put_spread`, `intraday_atm_call_premium`, `intraday_atm_put_premium`, `intraday_atm_trade_volume`, `intraday_atm_trade_count`.

---

### 5.7 PutCallRatioTracker (src/trackers/put_call.rs, 225 lines, 4 tests)

**Purpose:** Put-call volume and trade ratios, both overall and 0DTE-specific. Computed per-day then aggregated with Welford.

**Key metrics:**
- Daily put-call ratio by volume: `put_vol / call_vol` (only computed when call_vol > 0)
- Daily put-call ratio by trade count: `put_trades / call_trades`
- Daily 0DTE-specific put-call ratio: `0dte_put_vol / 0dte_call_vol`
- Intraday call/put volume curves (all and 0DTE, 390-bin each)

**Filters:** Only processes trade events with `trade_size > 0`.

**Day-level state (reset via `reset_day()`):**
- `day_call_vol`, `day_put_vol`, `day_call_trades`, `day_put_trades`: u64
- `day_0dte_call_vol`, `day_0dte_put_vol`: u64

**hft-statistics types:**
- `WelfordAccumulator` (3 instances: pcr_volume_daily, pcr_trades_daily, pcr_0dte_daily)
- `IntradayCurveAccumulator` (4 instances: intraday call/put volume, 0DTE call/put volume)

**Constructor:** `PutCallRatioTracker::new()` (also implements `Default`)

**Output JSON keys:** `tracker`, `n_days`, `pcr_volume_daily` (mean/std/count), `pcr_trades_daily` (mean/std), `pcr_0dte_daily` (mean/std/count), `intraday_call_volume`, `intraday_put_volume`, `intraday_0dte_call_volume`, `intraday_0dte_put_volume`.

---

### 5.8 OptionsEffectiveSpreadTracker (src/trackers/effective_spread.rs, 368 lines, 6 tests)

**Purpose:** Realized execution cost analysis. Compares quoted spread (BBO) to effective spread (actual execution cost). Critical for limit vs market order decisions.

**Key formulas:**

| Formula | Citation | Description |
|---------|----------|-------------|
| `effective_spread = 2 * abs(trade_price - mid)` | Lee & Ready (1991) | Realized execution cost |
| `quoted_spread = ask - bid` | -- | BBO-implied cost |
| `effective_vs_quoted_ratio = effective / quoted` | -- | Price improvement metric |
| `trade_through_rate = trades_outside_bbo / total_trades` | -- | Fraction of trades beyond BBO |
| `price_improvement_rate = trades_inside_bbo / total_trades` | -- | Fraction of trades inside BBO |

**Trade location classification:**
- **Inside BBO:** `bid < trade_price < ask`
- **At BBO:** `abs(trade_price - bid) < 1e-9` or `abs(trade_price - ask) < 1e-9`
- **Outside BBO:** everything else

**Size buckets:**

| Index | Label | Size Range |
|-------|-------|------------|
| 0 | `"1"` | 1 |
| 1 | `"2-5"` | 2-5 |
| 2 | `"6-10"` | 6-10 |
| 3 | `"11-50"` | 11-50 |
| 4 | `"51-100"` | 51-100 |
| 5 | `"100+"` | > 100 |

Constants: `SIZE_BUCKET_BOUNDS = [1, 5, 10, 50, 100]`, `N_SIZE_BUCKETS = 6`, `N_DTE_BUCKETS = 4`, `N_MONEYNESS_BUCKETS = 5`.

**Bucketing dimensions:** DTE (4) x Moneyness (5) for both effective and quoted spreads. Size (6) for effective spread only.

**Filters:** Only processes trade events with `trade_size > 0`, valid BBO, finite positive trade price, and finite positive mid.

**Trade location tracked separately for:** all trades, and 0DTE ATM trades.

**hft-statistics types:**
- `WelfordAccumulator` (46 instances: 4x5 effective spread + 4x5 quoted spread + 6 size-bucketed effective spread)
- `IntradayCurveAccumulator` (2 instances: intraday effective/quoted spread for 0DTE ATM)

**Constructor:** `OptionsEffectiveSpreadTracker::new()` (also implements `Default`)

**Output JSON keys:** `tracker`, `n_days`, `total_trades`, `trade_location` (all + 0dte_atm, each with inside_bbo/at_bbo/outside_bbo/trade_through_rate/price_improvement_rate), `by_dte_moneyness` (nested DTE -> moneyness -> effective_spread/quoted_spread/ratio), `by_size_bucket` (array), `intraday_effective_spread_0dte_atm`, `intraday_quoted_spread_0dte_atm`.

---

## 6. hft-statistics API Surface

The profiler imports these types from `hft-statistics`:

### WelfordAccumulator

Online mean/variance/std computation (Welford 1962). O(1) memory, numerically stable.

- `WelfordAccumulator::new() -> Self`
- `.update(value: f64)` -- add one observation
- `.mean() -> f64`, `.std() -> f64`, `.count() -> u64`
- `.min() -> f64`, `.max() -> f64`

Used for: per-day statistics (events, trades, contracts, volume, decay percentages, PCR), per-bucket spread/IV aggregation, per-size effective spread.

Total instances across all trackers: ~68.

### StreamingDistribution

Reservoir sampling with configurable capacity. Provides quantiles, mean, std, count.

- `StreamingDistribution::new(capacity: usize) -> Self`
- `.add(value: f64)` -- add one observation (reservoir replacement)
- `.summary() -> serde_json::Value` -- JSON with mean, std, count, quantiles

Used for: full distribution of spreads, premiums, IVs, trade sizes, imbalances.

Total instances across all trackers: ~18.

### IntradayCurveAccumulator

390-bin RTH (09:30-16:00 ET) minute-level aggregation. One Welford accumulator per minute bin.

- `IntradayCurveAccumulator::new_rth_1min() -> Self`
- `.add(ts_event: i64, value: f64, utc_offset: i32)` -- routes to correct minute bin using UTC offset
- `.finalize() -> Vec<BinResult>` -- all 390 bins with `minutes_since_open`, `mean`, `std`, `count`

Used for: intraday curves (spread, mid, premium, premium-to-spread, volume, trade count, IV, effective spread).

Total instances across all trackers: ~21.

### Time Utilities

- `utc_offset_for_date(year: i32, month: u32, day: u32) -> i32` -- DST-aware UTC offset for US Eastern. Returns -5 (EST) or -4 (EDT). Uses exact DST rules (2nd Sunday March, 1st Sunday November).
- `day_epoch_ns(year: i32, month: u32, day: u32, utc_offset: i32) -> i64` -- Midnight UTC in nanoseconds for the given trading day.

Used in `profiler::run()` to compute `DayContext.utc_offset` and `DayContext.day_epoch_ns`.

---

## 7. Configuration Schema

Full TOML reference. Example config at `configs/nvda_opra_8day.toml`.

```toml
[input]
# Directory containing OPRA .dbn.zst files.
# REQUIRED - no default.
data_dir = "../data/OPRA/NVDA/cmbp1_2025-11-13_to_2025-11-25"

# Filename pattern with {date} placeholder (YYYYMMDD).
# REQUIRED - no default.
filename_pattern = "opra-pillar-{date}.cmbp-1.dbn.zst"

# Underlying symbol for logging and provenance.
# Default: "NVDA"
symbol = "NVDA"

# Path to EQUS OHLCV .dbn.zst file for underlying open/close prices.
# Optional. Falls back to hardcoded NVDA 8-day prices if absent.
underlying_prices_file = "../data/EQUS/nvda_ohlcv.dbn.zst"

# Annualized risk-free rate for BSM calculations.
# Default: 0.05 (5%)
risk_free_rate = 0.05

# Optional date range filter (inclusive, YYYY-MM-DD format).
# Both optional. If omitted, all discovered files are processed.
date_start = "2025-11-13"
date_end = "2025-11-24"

[trackers]
# Each tracker can be independently enabled/disabled.
# All default to true.
quality = true
spread = true
premium_decay = true
volume = true
greeks = true
zero_dte = true
put_call = true
effective_spread = true

[output]
# Output directory for JSON files.
# Default: "output_opra"
output_dir = "output_opra_nvda"

# Whether to write summary files.
# Default: true
write_summaries = true

[buckets]
# ATM range as fraction of underlying price.
# Default: 0.02 (+/- 2%). ATM band = [S * 0.98, S * 1.02].
atm_range_pct = 0.02

# Deep ITM/OTM boundary.
# Default: 0.10 (10% from 1.0). Deep = ratio outside [0.90, 1.10].
deep_range_pct = 0.10

# Reservoir capacity for StreamingDistribution instances.
# Controls memory usage for quantile estimation.
# Default: 10000
reservoir_capacity = 10000
```

---

## 8. Design Decisions and Documented Limitations

### D1: Static underlying price

`underlying_estimate = underlying_open` for the entire trading day. OPRA data contains only option quotes and trades -- there are no equity quotes to track intraday underlying price changes. The open price from EQUS is the best available estimate. This affects moneyness classification (a borderline ATM/OTM option may be misclassified if the underlying moves significantly intraday) and IV computation (BSM S parameter is stale).

### D2: Time-to-expiry convention (0DTE vs non-0DTE)

For 0DTE contracts, time-to-expiry is computed using 252 trading days per year and 390 minutes per trading day (via `bsm::minutes_to_years()`). For non-0DTE contracts, time-to-expiry uses 365 calendar days (`dte as f64 / 365.0`). This split follows industry convention: 0DTE requires trading-day granularity because the remaining lifetime is measured in hours, while longer-dated options use calendar days. The 252/365 discontinuity at the 0DTE/1DTE boundary is accepted.

### D3: IV cap mismatch between tracker and solver

The GreeksTracker filters computed IV at `sigma < 5.0` (500% annualized), rejecting anything above as unreasonable for statistical profiling. The BSM solver itself allows up to `MAX_IV = 10.0` (1000%) during Newton-Raphson iteration. This two-tier approach means the solver can converge on extreme values that the tracker then discards, which is intentional: the solver should not artificially constrain convergence, but the profiler should not pollute aggregate statistics with extreme outliers.

### D4: atm_quote_count not reset across days

The `GreeksTracker.atm_quote_count` counter (used for IV sampling: compute every Nth ATM quote) is intentionally NOT reset in `reset_day()`. This provides continuous uniform sampling across the full dataset rather than per-day sampling with potential phase alignment artifacts. The IV sample interval can be configured via `with_sample_interval()` but is not exposed in the TOML config (see D7).

### D5: _risk_free_rate stored for future use

`PremiumDecayTracker._risk_free_rate` is stored but not currently used. It is reserved for a future feature comparing actual observed theta decay against BSM-theoretical theta decay using the risk-free rate. The leading underscore signals intentional unused storage.

### D6: option_mid() vs spread() strictness

`option_mid()` requires strict `ask > bid` (returns NAN for locked markets where ask == bid). `spread()` allows `ask >= bid` (returns 0.0 for locked markets). This asymmetry is intentional: a locked market has zero spread (meaningful) but no defined midpoint (the BBO is a single price, not a range to bisect).

### D7: iv_sample_interval not in TOML

The IV computation sampling interval (every Nth ATM quote) is configurable via the `GreeksTracker::with_sample_interval()` API but is not exposed as a TOML config field. The CLI binary hardcodes the default (100) via `GreeksTracker::new()`. This is a known limitation -- adding it to `ProfilerConfig` requires extending `TrackerConfig` or adding a `[greeks]` section.

### D8: day_epoch_ns unused by trackers

`DayContext.day_epoch_ns` (midnight UTC in nanoseconds) is computed by the profiler using `hft_statistics::time::regime::day_epoch_ns()` and passed to all trackers via `begin_day()`, but no tracker currently reads it. It is reserved for future extensions that may need absolute day-boundary timestamps (e.g., pre-market/post-market analysis, cross-day sequence alignment).

---

## 9. Test Coverage

All tests are inline (`#[cfg(test)]` modules within each source file). No integration tests.

| File | Tests | Description |
|------|-------|-------------|
| `contract.rs` | 12 | OCC parsing: standard call/put, fractional/small/large strikes, LEAPS expiry, DTE, 0DTE, malformed (too short, bad type, bad date, empty root) |
| `bsm.rs` | 19 | norm_cdf known values, BSM call/put prices, put-call parity (5 strikes), delta (ATM/deep ITM/deep OTM), gamma (positive, peak at ATM), theta (negative for long), vega (positive), IV recovery (call, put), IV edge cases (zero price, below intrinsic, 0DTE ATM), minutes_to_years, near-expiry no-panic |
| `moneyness.rs` | 11 | Call ATM/ATM-edge/ITM/deep-ITM/OTM/deep-OTM, put ATM/ITM/OTM, invalid underlying (0, negative, NaN), ratio computation |
| `report_utils.rs` | 4 | dte_bucket_index (including negative DTE), moneyness_index, DTE labels match indices, moneyness labels match indices |
| `quality.rs` | 4 | Quote/trade counting, DTE bucketing, day rollover, finalize structure |
| `spread.rs` | 4 | Spread computation (0.05 USD), sentinel filtering, 0DTE ATM tracking, finalize structure |
| `premium_decay.rs` | 3 | First/last premium tracking with decay calculation (~73%), empty day handling, finalize structure |
| `volume.rs` | 4 | Trade-only counting, PCR with zero calls, DTE bucketing, finalize structure |
| `greeks.rs` | 4 | IV computation fires for ATM, IV sampling with interval (500/10=50), non-ATM ignored, finalize structure |
| `zero_dte.rs` | 4 | Non-0DTE filtered, non-ATM filtered, 0DTE ATM counting, call/put separation |
| `put_call.rs` | 4 | PCR computation (50/100=0.5), no-calls-no-PCR, day rollover, finalize structure |
| `effective_spread.rs` | 6 | size_bucket_index, effective spread formula (2*|1.08-1.05|=0.06), trade-through classification (inside/at/outside BBO), quotes ignored, zero-size ignored, finalize structure |
| **Total** | **79** | |

---

## 10. Build and Run

**Build:**
```bash
cargo build --release
```

**Run:**
```bash
cargo run --release --bin profile_opra -- --config configs/nvda_opra_8day.toml
```

**Test:**
```bash
cargo test
```

**Release profile:** `opt-level=3`, `lto=fat`, `codegen-units=1`, `strip=true`.

**Output:** JSON files written to `output_dir` (default `output_opra/`), numbered by tracker instantiation order:
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

Each JSON file contains the tracker's report object plus an injected `_provenance` object with full config, runtime stats, and version.
