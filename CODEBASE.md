# CODEBASE.md -- OPRA Statistical Profiler

Deep technical reference for the `opra-statistical-profiler` Rust crate. Covers architecture, every module, all formulas, configuration, and design decisions.

> **Pipeline scope (2026-06-02).** This module is part of an **intraday trading research pipeline** — an experiment-first platform for discovering and validating *any* profitable **intraday** trading edge (no overnight positions), across approach classes (microstructure/HFT, scalping, intraday momentum, intraday statistical arbitrage, …) and instruments (equities, futures, same-day options). The pipeline *originated* as a high-frequency NVDA MBO/LOB microstructure system — that origin explains the "HFT" / "LOB" / "MBO" naming here — and that microstructure-direction program is now one (largely-closed) track among many. **Names are historical; the mission is general.** This module's role: a Rust OPRA options-data profiler — 8 trackers incl. BSM Greeks/IV, moneyness, premium decay (4.1M evt/s); the options-analytics surface (directly relevant to the same-day-options approach class). For the full mission + approach taxonomy + capability-readiness boundary, see root `CLAUDE.md` §Research Scope & Charter (+ `CROSS_ASSET_OFI_FINDINGS_AND_ISSUES_2026_06_01.md` §9).

---

## 1. Overview

High-performance statistical profiler for OPRA options microstructure analysis. Processes raw OPRA CMBP-1 `.dbn.zst` files in a single pass through composable analysis trackers, producing JSON statistical profiles.

| Property | Value |
|----------|-------|
| Language | Rust (edition 2021, MSRV 1.82) |
| Trackers | 8 |
| Tests | 91 passed + 1 ignored (FIND-UFO contract-lock, blocked on Axis L) |
| Source LOC | ~4,550 (22 files including binary + diagnostics.rs) |
| Throughput | ~4.1M events/sec |
| Architecture | Single-pass composable tracker dispatch |
| Input | OPRA CMBP-1 `.dbn.zst` (consolidated BBO + trades) + EQUS OHLCV `.dbn.zst` (underlying open/close **and** the O/S equity-volume denominator) |
| Output | Numbered JSON files with `_provenance` metadata — **two kinds**: one per-tracker profile per enabled tracker, plus a post-loop O/S-ratio aggregate report (§5.9) |

**Dependencies:**
- `hft-statistics` (git, github.com/nagarx/hft-statistics.git, pinned to rev `e976ff7`) -- Welford, streaming distribution, intraday curve accumulator, DST-aware time utilities
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
    +---> [if os_ratio.enabled] O/S numerator accrual (NOT a tracker):
              is_trade AND dte in [dte_min, dte_max]
                  --> day_opvol_os += trade_size
    |
    v
End of day:
    tracker.end_of_day(day_index)
    tracker.reset_day()
    [if os_ratio.enabled] push (trading_date, day_opvol_os) to per-day OPVOL
    |
    v
After all files:
    tracker.finalize() --> serde_json::Value        (one report per tracker)
    [if os_ratio.enabled] compute_os_series(OPVOL_per_day, EQVOL_per_day)
        --> "OsRatio" aggregate report, appended AFTER the trackers (§5.9)
    |
    v
write_output():
    Insert _provenance (version, symbol, dataset, config, throughput)
    Write output_dir/{NN}_{TrackerName}.json  (+ trailing NN_OsRatio.json)
```

**Day processing lifecycle:**

1. Parse date from filename (YYYYMMDD via `{date}` placeholder in pattern)
2. Compute `utc_offset` and `day_epoch_ns` via `hft_statistics::time::regime`
3. Look up underlying open/close from EQUS prices (or fallback)
4. Build `DayContext`, call `begin_day()` on all trackers
5. Open `.dbn.zst`, build `ContractMap` from metadata symbology
6. Set `underlying_estimate = underlying_open` (static for entire day -- see D1)
7. Iterate all `CbboMsg` records, enrich into `OptionsEvent`, dispatch (and, when `os_ratio.enabled`, accrue the DTE-banded per-day option trade volume for the O/S numerator)
8. Call `end_of_day()`, `reset_day()` on all trackers
9. Repeat for next file

After all days: when `os_ratio.enabled`, join the per-day OPVOL with the per-day EQVOL and emit the O/S-ratio aggregate report (§5.9) — a second output path that does NOT flow through the `OptionsTracker` trait.

**File discovery:** `discover_files()` scans `data_dir` for files matching `filename_pattern` with `{date}` placeholder (YYYYMMDD). Filters by optional `date_start`/`date_end` (inclusive, YYYY-MM-DD format). Results sorted chronologically.

**Underlying prices:** Loaded from EQUS OHLCV `.dbn.zst` via `load_underlying_prices_from_equs()`. Decodes `OhlcvMsg` records, converts nanodollar prices to USD. Falls back to hardcoded NVDA prices for 8-day Nov 2025 window **only when `symbol = "NVDA"`** — non-NVDA symbols without `underlying_prices_file` produce a hard error. Trading dates with no available underlying price (whether from file or fallback) also produce a hard error to prevent silent NaN-poisoning of moneyness classification.

**EQUS feed's dual role.** The same `OhlcvMsg` records supply BOTH the underlying open/close (for moneyness + BSM Greeks) AND the daily equity SHARE volume (`OhlcvMsg.volume`) — the latter is the O/S-ratio EQVOL denominator (§5.9). The built-in fallback carries open/close only (`volume = 0`), so on fallback data (or any open/close-only feed) the O/S report is still emitted but **degrades to all-null** (`os_ratio_summary.n_finite == 0`) with a loud `run()` warning; a runnable (non-null) O/S therefore requires a real EQUS `underlying_prices_file`.

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

### src/config.rs (153 lines)

TOML-driven profiler configuration. All fields have defaults except `input.data_dir` and `input.filename_pattern`.

**Structs:**

| Struct | Fields | Purpose |
|--------|--------|---------|
| `ProfilerConfig` | `input`, `trackers`, `output`, `buckets`, `os_ratio`, `reservoir_capacity` | Top-level config |
| `InputConfig` | `data_dir`, `filename_pattern`, `symbol`, `underlying_prices_file`, `risk_free_rate`, `date_start`, `date_end` | Data source |
| `TrackerConfig` | one boolean per tracker | Which trackers to enable |
| `OutputConfig` | `output_dir`, `write_summaries` | Output paths |
| `BucketConfig` | `atm_range_pct`, `deep_range_pct` | Moneyness classification thresholds |
| `OsRatioConfig` | `enabled`, `rolling_window_days`, `dte_min`, `dte_max` | O/S-ratio aggregate output (§5.9, §7) |

Every config struct (the `ProfilerConfig` field types above) uses `#[serde(deny_unknown_fields)]` — typos or misplaced keys cause a parse error with a clear "unknown field X, expected Y or Z" message. This catches the class of bug where a top-level field is accidentally placed under a section header.

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
| `os_ratio.enabled` | `true` | bool |
| `os_ratio.rolling_window_days` | `6` | usize |
| `os_ratio.dte_min` / `dte_max` | `5` / `35` | i64 |

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
| `publisher_id` | `u16` | Originating venue (`RecordHeader.publisher_id`). Phase 2A-1. |
| `bid_pb` | `u16` | Best-bid venue at consolidated NBBO. Phase 2A-1. |
| `ask_pb` | `u16` | Best-ask venue at consolidated NBBO. Phase 2A-1. |
| `ts_in_delta` | `i32` | SIP-vs-exchange latency (ns, capped 2s). Phase 2A-1. |
| `flags` | `u8` | Raw dbn FlagSet: MAYBE_BAD_BOOK=0x04, BAD_TS_RECV=0x08, SNAPSHOT=0x20. Phase 2A-1. |
| `sequence` | `u32` | Venue message sequence number. Phase 2A-1. |
| `ts_recv` | `i64` | Capture-server timestamp (UTC ns). Phase 2A-1. |

**Helper methods:**

| Method | Formula / Logic | Notes |
|--------|----------------|-------|
| `option_mid()` | `(bid + ask) / 2` | Returns NAN if either side NAN/infinite or `ask <= bid` (strict — see D6) |
| `spread()` | `ask - bid` | Returns NAN if either side NAN/infinite or `ask <= bid` (strict post-F-NEW-R5-1 alignment 2026-05-08 — see D6; was `<` pre-fix) |
| `spread_pct()` | `spread / mid * 100` | Returns NAN if mid <= 0 or non-finite (transitively NaN at locked/crossed via `option_mid()` early-return) |
| `has_valid_bbo()` | `bid.is_finite() && ask.is_finite() && bid > 0 && ask > bid` | Strict validity gate; also requires `bid > 0` (stricter than the other three) |
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

### src/profiler.rs (~460 lines)

Single-pass profiling engine. Orchestrates file discovery, day processing, event enrichment, tracker dispatch, and diagnostic accounting.

**Constants:**

| Constant | Value | Purpose |
|----------|-------|---------|
| `SENTINEL_I64` | `i64::MAX` | dbn sentinel for undefined prices |
| `OUTPUT_SCHEMA_VERSION` | `&str` (see the constant + its changelog comment in `profiler.rs`) | SemVer for the per-tracker JSON output schema; additive-bumped when the `OsRatio` aggregate report was added. Emitted in `_provenance.output_schema_version`. |

**Types:**

| Type | Fields | Description |
|------|--------|-------------|
| `ProfileResult` | `n_days: u32`, `total_events: u64`, `total_decode_errors: u64`, `elapsed_secs: f64`, `reports: Vec<(String, Value)>`, `diagnostics: ProfileDiagnostics` | Profiling output. `reports` holds one entry per tracker plus (when `os_ratio.enabled`) a trailing `("OsRatio", …)` aggregate. |
| `DailyUnderlyingPrice` | `date: NaiveDate`, `open: f64`, `close: f64`, `volume: u64` | Per-day underlying OHLCV. `volume` (EQUS `OhlcvMsg.volume`) is the O/S EQVOL denominator (§5.9); `0` in built-in-fallback mode. |

**Functions:**

| Function | Description |
|----------|-------------|
| `run(config, trackers, underlying_prices) -> Result<ProfileResult>` | Main orchestration. Discovers files, iterates days, enriches events, dispatches to trackers. When `os_ratio.enabled`, it ALSO accrues the DTE-banded per-day option trade volume and, after all files, appends the post-loop `OsRatio` aggregate report (§5.9). |
| `price_or_nan(price_i64: i64) -> f64` | Converts i64 nanodollar price to f64 USD. Returns `NAN` for `SENTINEL_I64` or `<= 0`. Formula: `price_i64 as f64 / 1e9`. Inlined. |
| `write_output(config, result) -> Result<()>` | Creates output directory, writes `{NN}_{TrackerName}.json` with `_provenance` object injected. |
| `discover_files(config) -> Result<Vec<(String, PathBuf)>>` | Scans `data_dir` for files matching pattern with `{date}` placeholder. Extracts YYYYMMDD, filters by date range, sorts chronologically. |
| `load_underlying_prices_from_equs(path) -> Result<Vec<DailyUnderlyingPrice>>` | Decodes EQUS OHLCV `.dbn.zst` (small, ~8 KB). Converts `OhlcvMsg` timestamps to dates via epoch-day arithmetic (`ts / 1e9 / 86400 + 719163` for CE day conversion). Extracts nanodollar open/close (→ USD) AND `OhlcvMsg.volume` (the O/S EQVOL denominator, §5.9). |

**Event enrichment (inside run()):**

The dispatch loop reads ALL 19 non-reserved CbboMsg fields (Phase 2A-1), increments `ProfileDiagnostics` counters at every decision point (Phase 2A-2), and computes `time_regime()` from `hft_statistics` (Phase 2A-4). Every record is accounted for via the conservation law: `total_decoded == unknown_instrument + dispatched`.

Key detail: `underlying_estimate = underlying_open` for the entire day (see D1 / FIND-UFO).

**Provenance object (injected into every JSON output):**

```json
{
  "profiler_version": "0.1.0",
  "output_schema_version": "2.0.0",
  "symbol": "NVDA",
  "dataset": "OPRA.PILLAR",
  "schema": "cmbp-1",
  "n_days": 8,
  "total_events": 32000000,
  "runtime_secs": 7.8,
  "throughput_events_per_sec": 4100000,
  "diagnostics_schema_version": "1.0.0",
  "diagnostics": {
    "total_decoded": 32000100,
    "unknown_instrument": 100,
    "dispatched": 32000000,
    "decode_errors": 0,
    "observability": {
      "action_trade": 5000000,
      "action_quote": 26999500,
      "action_other": 500,
      "moneyness_fallback": 0,
      "bad_book_flagged": 3,
      "bad_ts_flagged": 17,
      "snapshot_flagged": 0
    }
  },
  "conservation_check": true,
  "config": { "...full config..." }
}
```

---

### src/diagnostics.rs

Dispatch-loop diagnostic counters for the profiler, serialized into every output's `_provenance.diagnostics` block. Structure/contract only — the full field surface is enumerated in the §2 provenance JSON block and §9.

**Constant:** `PROFILER_DIAGNOSTICS_SCHEMA_VERSION` (`&str`, see the constant in `diagnostics.rs`) — SemVer for the diagnostics JSON contract (MAJOR = field removed/reinterpreted, MINOR = additive optional field, PATCH = docs-only), aligned with the MBO `PRODUCER_DIAGNOSTICS_SCHEMA_VERSION` policy.

**Types:**

| Type | Role |
|------|------|
| `ProfileDiagnostics` | Terminal dispatch buckets: `total_decoded`, `unknown_instrument`, `dispatched`, `decode_errors`, and nested `observability`. `#[non_exhaustive]` + `#[serde(default)]` for forward-compat. |
| `DispatchObservability` | OVERLAPPING sub-counters partitioning subsets of `dispatched` (`action_trade`/`action_quote`/`action_other`, `moneyness_fallback`, `bad_book_flagged`/`bad_ts_flagged`/`snapshot_flagged`) — observations, NOT filter gates (every counted event was still dispatched). |

**Conservation law:** `conservation_check()` asserts `total_decoded == unknown_instrument + dispatched` (`decode_errors` are counted at the loader level and never enter the dispatch loop). Enforced by `debug_assert!` in `run()`.

---

### src/os_ratio.rs

Option-to-stock volume ratio (O/S) — Johnson & So (2012), *Journal of Financial Economics* 106(2):262-286, Eq. 9; wiki `theory:option_to_stock_volume_ratio`. This is the module's **second output kind**: a pure post-loop day-level aggregate, NOT an `OptionsTracker` (see §5.9 for the architectural framing).

**What it computes** (per trading day `d`):

| Quantity | Definition |
|----------|------------|
| `O/S_d` | `OPVOL_d / (EQVOL_d / 100)` — DTE-banded option-contract trade volume over equity SHARE volume expressed in round lots of 100 (commensurate with the 100-share option multiplier). Stored as a dimensionless **fraction** (Johnson-So mean 4.34% = 0.0434; ×100 for a percent at display time only). |
| `ΔO/S_d` | `(O/S_d − mean(prior `rolling_window_days` O/S)) / mean(...)` — the single-name daily-cadence analog of Johnson-So's prior-6-month deviation. Uses ONLY the strictly-prior window (no look-ahead, §9). Warmup days (`d < window`) emit `null`; `n_warmup_null = min(window, n_days)`. |

**Function:** `compute_os_series(opvol_per_day, eqvol_per_day, rolling_window_days, dte_min, dte_max) -> serde_json::Value`. Pure (takes `(NaiveDate, u64)` pairs, no profiler/dbn types → golden-testable). Joins numerator↔denominator by date, sorts by date for window correctness + determinism, and returns the `OsRatio` report object. `dte_min`/`dte_max` are echoed into the report's `dte_band` for provenance (the `opvol_per_day` is already band-filtered by the caller in `run()`).

**Sentinel policy (each recorded, never silently dropped — §8):**

| Condition | `os_ratio` result |
|-----------|-------------------|
| `eqvol_d == 0` (fallback / open-close-only feed) | `null` |
| `opvol_d == 0` (thin/empty band on a day) | `0.0` (FINITE — zero relative option activity, not missing) |
| date in OPVOL but absent from EQVOL | `null` + increments `n_join_miss` (unreachable in the profiler path — `run()` hard-errors on a missing underlying, so EQVOL ⊇ OPVOL; reachable in unit tests) |

**Report keys:** `tracker: "OsRatio"`, `n_days`, `rolling_window_days`, `dte_band`, `eqvol_round_lot_size`, `n_join_miss`, `per_day[]` (`date`/`opvol`/`eqvol_shares`/`eqvol_round_lots`/`os_ratio`/`delta_os`), `os_ratio_summary` + `delta_os_summary` (`mean`/`std`/`min`/`max`/`n_finite`; the delta summary also carries `n_warmup_null`), and a load-bearing `_caveats` string.

**Embedded honesty caveats** (`_caveats`, per §13 discipline): **AD-OS-1** — Johnson-So's headline alpha is CROSS-SECTIONAL (a decile spread over ~1,700 firms); single-name is an UNESTABLISHED extrapolation, credible analog is ΔO/S with UNCALIBRATED magnitude. **AD-OS-2** — the profiled name may be the WEAKEST short-sale-cost + leverage tail (near-zero single-name edge). **AD-OS-4** — the 5-35-DTE band is the opposite end of the maturity spectrum from a 0DTE-dominated window. **No forward-IC claim** — this module emits the per-day instrument, not a measured IC (a short pilot leaves only `n_days − window` ΔO/S points, far below the §13 IC gate). `os_ratio_summary.n_finite == 0` signals the underlying volume was unavailable (fallback mode) — provide an EQUS `underlying_prices_file`.

**Test coverage** (categories; run `cargo test` for the live count): the Eq. 9 fraction/round-lot golden, a worked ΔO/S series + warmup-boundary off-by-one, `opvol=0`→finite-zero, `eqvol=0`→null, join-miss recorded-not-dropped, determinism/date-sort byte-equality, `n_warmup_null` clamp, and a no-look-ahead invariant.

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

### src/bin/profile_opra.rs (142 lines)

CLI binary entry point.

**Usage:** `profile_opra --config <path>`

**Flow:**
1. Parse `--config` argument
2. Load `ProfilerConfig` from TOML file
3. Load underlying prices: from EQUS file (hard error on load failure), or fallback to hardcoded 8-day NVDA Nov 2025 prices ONLY when `symbol = "NVDA"` (any other symbol without explicit prices file is rejected with a hard error)
4. Instantiate enabled trackers based on `config.trackers.*` booleans
5. Call `profiler::run()` then `profiler::write_output()`
6. Log final summary with day count, event count, throughput

**Tracker instantiation order:** QualityTracker, SpreadTracker, ZeroDteTracker, PremiumDecayTracker, VolumeTracker, GreeksTracker, PutCallRatioTracker, OptionsEffectiveSpreadTracker.

**Fallback prices:** 8 NVDA trading days (2025-11-13 to 2025-11-24), open/close from EQUS OHLCV. Used when no `underlying_prices_file` is configured AND `symbol = "NVDA"`. Any other symbol without explicit prices file is rejected with a hard error.

---

## 4. Options Math

### BSM (src/options_math/bsm.rs, 438 lines, 19 tests)

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
| Vega | `S * sqrt(T) * N'(d1)` | Same for calls and puts |

> **Theta** was removed in the 2026-05-08 F-017 cleanup. The prior implementation divided
> `annual_theta / 365.0` (calendar-day units) while `minutes_to_years()` uses 252×390
> trading-time units — composing them in production would have produced systematically
> mis-scaled per-day theta. The function had zero production callers, so the unit mismatch
> was latent. Re-introducing theta MUST align the unit system with `minutes_to_years()`
> after the project makes an explicit time-system policy decision (see `bsm.rs` comment).

**Edge case handling:** When `t < MIN_T` or `sigma < MIN_IV` or `s <= 0` or `k <= 0`, pricing functions return intrinsic value (`max(S-K, 0)` for calls, `max(K-S, 0)` for puts), Greeks return 0 (or 1/-1 for deep ITM/OTM delta).

**norm_cdf (Standard Normal CDF):**

Delegates to `hft_statistics::statistics::phi` (Abramowitz & Stegun 7.1.26, Hart 1968 coefficients) with a NaN-propagation guard. `phi(NaN)` returns 0.0 by convention, but BSM callers need NaN sentinel propagation — the thin wrapper preserves this contract while eliminating local polynomial duplication (Phase 2A-3, 2026-05-27).
- Accuracy: |error| < 7.5e-8 (verified: max observed error 6.92e-8)

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

**Purpose:** Premium decay curve for 0DTE ATM options. Tracks how observed option premium decays throughout the trading day. Key deliverable: empirical premium-decay data for the 0DTE strategy's time decay budget. (Comparison against BSM-theoretical theta is reserved for a future feature — see D5 — and would require re-introducing `bsm::theta` with aligned unit conventions per the 2026-05-08 F-017 cleanup.)

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

**Stored fields:** `_risk_free_rate: f64` -- stored for a future BSM-theoretical comparison feature (see D5). Currently unused because `bsm::theta` was removed in the 2026-05-08 F-017 cleanup pending a time-system unit-policy decision.

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

### 5.5 GreeksTracker (src/trackers/greeks.rs, 274 lines, 4 tests)

**Purpose:** Implied volatility and Greek computation for ATM options. Computes IV from BSM for ATM contracts by DTE bucket. Tracks delta, gamma, vega distributions for 0DTE ATM.

**Key metrics:**
- 0DTE ATM call/put IV distributions (reservoir)
- IV by DTE bucket (4 buckets, Welford)
- Intraday 0DTE ATM IV curve (390-bin)
- 0DTE ATM call/put delta (signed, separate distributions), gamma, vega distributions (reservoir)
- IV computation success rate: `iv_computed / (iv_computed + iv_failed)`
- ATM quote count and sample interval

**Filters:** Only processes ATM events with valid BBO and positive mid. Non-ATM events are entirely skipped.

**IV sampling gate:** IV computation is expensive. Only computed every Nth qualifying ATM quote (`atm_sample_counter % iv_sample_interval != 0` -> skip). Default interval: 100. The sample counter is initialized to `iv_sample_interval - 1` so the first ATM event of each day triggers IV computation, and reset to `iv_sample_interval - 1` in `reset_day()` (Phase 2B-3 fix).

**`atm_quote_count`** is a monotonically increasing counter of total qualifying ATM events seen (never reset). `atm_sample_counter` is the internal sampling gate counter (reset per day).

**Time-to-expiry calculation:**
- 0DTE: Estimate minutes remaining from timestamp. Close at 16:00 ET. `remaining_ns = (close_utc_ns - event_tod_ns).max(0)`. Convert to minutes, **clamp to `min(390.0)`** so that pre-market events (e.g., 04:00 ET) correctly map to one full RTH session of trading time rather than the inflated calendar interval. Then `bsm::minutes_to_years()` (252-day year, 390 min/day).
- Non-0DTE: `dte as f64 / 365.0` (calendar days, see D2). Clamped to min 1e-6.

**IV filter:** Only accepts IV where `sigma.is_finite() && sigma > 0.0 && sigma < self.max_iv` (default 5.0, configurable; see D3). Outside this range counts as `iv_failed`.

**Greek computation:** Call and put deltas are stored in separate signed distributions (`dte0_atm_delta_call` ∈ [0,1], `dte0_atm_delta_put` ∈ [-1,0]). Phase 2B-1 removed the prior `.abs()` which collapsed both into an uninformative magnitude-only distribution. Gamma and vega are stored as-is. All require finite values to be accepted into distributions.

**hft-statistics types:**
- `StreamingDistribution` (6 instances: dte0_atm_call_iv, dte0_atm_put_iv, delta_call, delta_put, gamma, vega)
- `WelfordAccumulator` (4 instances: per-DTE-bucket IV)
- `IntradayCurveAccumulator` (1 instance: dte0_atm_iv_curve)

**Constructors:**
- `GreeksTracker::new(risk_free_rate, reservoir_capacity)` -- default iv_sample_interval=100
- `GreeksTracker::with_sample_interval(risk_free_rate, reservoir_capacity, iv_sample_interval)` -- explicit interval (clamped to min 1)

**Output JSON keys:** `tracker`, `n_days`, `atm_quote_count`, `iv_sample_interval`, `iv_computed`, `iv_failed`, `iv_success_rate`, `dte0_atm_call_iv`, `dte0_atm_put_iv`, `dte0_atm_delta_call`, `dte0_atm_delta_put`, `dte0_atm_gamma`, `dte0_atm_vega`, `iv_by_dte` (0dte/1dte/2_7dte/other, each with mean/std/count), `intraday_dte0_atm_iv_curve`.

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

### 5.9 O/S Ratio — the non-tracker aggregate output

The profiler has **two output kinds**. The 8 sections above are `OptionsTracker` implementations dispatched per-event through the trait. The O/S ratio is the **second kind**: a day-level aggregate that does NOT implement `OptionsTracker` and does NOT flow through the trait. Instead, `run()` accrues the DTE-banded per-day option trade volume inline in the dispatch loop, and — after all files — joins it with the per-day equity volume (`OhlcvMsg.volume`) and appends the report via `os_ratio::compute_os_series()` (§3 `src/os_ratio.rs`).

- **Emission:** whenever `[os_ratio].enabled` (default `true`), the report lands as the last numbered file (e.g. `09_OsRatio.json` with the default tracker set — the numeric prefix is positional). It is fully absent from the output **only** when `enabled = false`.
- **Fallback degradation:** on built-in-fallback / open-close-only underlying data (`volume == 0`) the report is still written but every `os_ratio`/`delta_os` is `null` (`os_ratio_summary.n_finite == 0`), with a loud `run()` warning. A runnable O/S requires a real EQUS `underlying_prices_file` (§2 "EQUS feed's dual role").
- **Config:** `[os_ratio]` (`enabled`, `rolling_window_days`, `dte_min`, `dte_max`) — see §7.

Formula, sentinels, report keys, and the embedded honesty caveats are documented in the §3 `src/os_ratio.rs` entry.

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

Total instances across all trackers: ~71.

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
data_dir = "../data/OPRA/NVDA/cmbp1_2025-10-29_to_2025-11-24"

# Filename pattern with {date} placeholder (YYYYMMDD).
# REQUIRED - no default.
filename_pattern = "opra-pillar-{date}.cmbp-1.dbn.zst"

# Underlying symbol for logging and provenance.
# Default: "NVDA"
symbol = "NVDA"

# Path to EQUS OHLCV .dbn.zst file for underlying open/close prices.
# Optional. Falls back to hardcoded NVDA 8-day prices ONLY when symbol = "NVDA";
# any other symbol without this file produces a hard error.
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

[os_ratio]
# Option-to-stock volume ratio (Johnson & So 2012). Emits the NN_OsRatio.json
# aggregate report (§5.9). The whole section is optional — omit it to accept
# all defaults (O/S ENABLED). Requires an EQUS underlying_prices_file for a
# non-null result (the fallback carries no equity volume → O/S is null).
enabled = true            # Default: true. false → no OsRatio report at all.
rolling_window_days = 6   # Default: 6. Trailing window (trading days) for the ΔO/S rolling-mean deviation.
dte_min = 5               # Default: 5.  Inclusive lower DTE bound for the OPVOL numerator (JS footnote 9: the 5-35 band).
dte_max = 35              # Default: 35. Inclusive upper DTE bound.
```

**Note:** `reservoir_capacity` is a top-level field of `ProfilerConfig`, not part of `[buckets]`.
It must appear before any `[section]` header in the TOML file:

```toml
reservoir_capacity = 10000

[input]
...
```

---

## 8. Design Decisions and Documented Limitations

### D1: Static underlying price

`underlying_estimate = underlying_open` for the entire trading day. OPRA data contains only option quotes and trades -- there are no equity quotes to track intraday underlying price changes. The open price from EQUS is the best available estimate. This affects moneyness classification (a borderline ATM/OTM option may be misclassified if the underlying moves significantly intraday) and IV computation (BSM S parameter is stale).

### D2: Time-to-expiry convention (0DTE vs non-0DTE)

For 0DTE contracts, time-to-expiry is computed using 252 trading days per year and 390 minutes per trading day (via `bsm::minutes_to_years()`). For non-0DTE contracts, time-to-expiry uses 365 calendar days (`dte as f64 / 365.0`). This split follows industry convention: 0DTE requires trading-day granularity because the remaining lifetime is measured in hours, while longer-dated options use calendar days. The 252/365 discontinuity at the 0DTE/1DTE boundary is accepted.

### D3: IV cap — configurable dual threshold (Phase 2B-2)

The GreeksTracker filters computed IV at `sigma < self.max_iv` (default 5.0 = 500% annualized), rejecting anything above as unreasonable for statistical profiling. The BSM solver itself allows up to `MAX_IV = 10.0` (1000%) during Newton-Raphson iteration. This two-tier approach is intentional: the solver should not artificially constrain convergence, but the profiler should not pollute aggregate statistics with extreme outliers. The tracker-level threshold is now configurable via the `max_iv` field (Phase 2B-2).

### D4: IV sampling — per-day reset (Phase 2B-3, supersedes original design)

The `GreeksTracker.atm_sample_counter` (used for IV sampling: compute every Nth ATM quote) is initialized to `iv_sample_interval - 1` and reset to `iv_sample_interval - 1` in `reset_day()`. This ensures the FIRST qualifying ATM event of EACH trading day triggers IV computation, eliminating the prior systematic early-day sampling bias (adversarial review caught that resetting to 0 would exclude the first ~100 ATM events per day — the highest-volatility open period). `atm_quote_count` is a separate monotonically increasing counter of total ATM events (never reset). The IV sample interval can be configured via `with_sample_interval()` but is not exposed in the TOML config (see D7).

### D5: _risk_free_rate stored for future use

`PremiumDecayTracker._risk_free_rate` is stored but not currently used. It is reserved for a future feature comparing observed premium decay against BSM-theoretical theta decay. Implementing that feature requires re-introducing `bsm::theta` (removed 2026-05-08 F-017 cleanup) with unit conventions aligned to `minutes_to_years()` (trading-time, not calendar-time). The leading underscore signals intentional unused storage.

### D6: BBO accessor strictness — aligned STRICT (2026-05-08, F-NEW-R5-1)

All four BBO-derived accessors apply STRICT `ask > bid` gating uniformly:

| Accessor (`src/event.rs`) | Gate | Behavior at `ask == bid` |
|---|---|---|
| `option_mid()` (:91) | STRICT `ask > bid` | NaN |
| `spread()` (:105)    | STRICT `ask > bid` (was `>=` pre-fix) | NaN |
| `spread_pct()` (:114) | STRICT via `option_mid()` early-return | NaN |
| `has_valid_bbo()` (:139) | STRICT `ask > bid` AND `bid > 0` | false |

**Rationale:** the prior mixed-strictness scheme (option_mid strict, spread non-strict) was a contract-plane asymmetry surfaced by Round 5 of the OPRA adversarial-validation cycle. A locked market (`ask == bid`) is not a valid BBO observation per hft-rules §8 — silently emitting `spread=0` while `option_mid` returned NaN produced an inconsistent internal state in `spread_pct` (which depends on both). Aligning to STRICT closes the asymmetry. In practice all 8 tracker consumers gate on `has_valid_bbo()` (already STRICT pre-fix), so observable output is empirically unchanged — the fix is contract-plane correctness, not output drift. Locked behavior across all four accessors is locked by a parametric invariant test in `event.rs::tests::test_invariant_finiteness_agrees_across_option_mid_spread_spread_pct`.

### D7: iv_sample_interval not in TOML

The IV computation sampling interval (every Nth ATM quote) is configurable via the `GreeksTracker::with_sample_interval()` API but is not exposed as a TOML config field. The CLI binary hardcodes the default (100) via `GreeksTracker::new()`. This is a known limitation -- adding it to `ProfilerConfig` requires extending `TrackerConfig` or adding a `[greeks]` section.

### D8: day_epoch_ns unused by trackers

`DayContext.day_epoch_ns` (midnight UTC in nanoseconds) is computed by the profiler using `hft_statistics::time::regime::day_epoch_ns()` and passed to all trackers via `begin_day()`, but no tracker currently reads it. It is reserved for future extensions that may need absolute day-boundary timestamps (e.g., pre-market/post-market analysis, cross-day sequence alignment).

### D9: Strict TOML parsing (deny_unknown_fields)

Every config struct (`ProfilerConfig`, `InputConfig`, `TrackerConfig`, `OutputConfig`, `BucketConfig`, `OsRatioConfig`) uses `#[serde(deny_unknown_fields)]`. This rejects typos and misplaced keys at parse time with a clear error like `"unknown field reservoir_capacity, expected atm_range_pct or deep_range_pct"`. The motivation: a previous incarnation of the configs placed `reservoir_capacity` under `[buckets]` (incorrect — it is a top-level field), and serde silently ignored it. The bug was invisible because the configured value matched the default. Strict parsing eliminates this entire class of silent-misconfiguration bugs. Trade-off: introducing a new optional field in a future version becomes a "backward-incompatible" change for users with stricter older parsers — schema versioning may be added later if needed.

### D10: NVDA-only fallback safety constraint

The CLI binary's built-in fallback prices (`load_nvda_fallback_prices()` in `src/bin/profile_opra.rs`) are NVDA-specific (8 trading days in November 2025). The binary REFUSES to use the fallback for any symbol other than `"NVDA"` (`profile_opra.rs:45-51` exits with a hard error). It also REFUSES to silently fall back to NVDA prices when an explicit `underlying_prices_file` fails to load (`profile_opra.rs:33-39`). And the profiler library itself REFUSES to silently use a 0.0 underlying price when no daily price is found (`profiler.rs:75-83`). The motivation: silent symbol mismatch produced silently wrong moneyness classification and silently wrong Greeks. All three guardrails turn previously silent failures into hard errors with actionable messages. The fallback survives only as a development convenience for the original NVDA Nov 2025 dataset; removal is planned for v0.2.0.

---

## 9. Test Coverage

All tests are inline (`#[cfg(test)]` modules within each source file). No integration tests.

| File | Tests | Description |
|------|-------|-------------|
| `contract.rs` | 12 | OCC parsing: standard call/put, fractional/small/large strikes, LEAPS expiry, DTE, 0DTE, malformed (too short, bad type, bad date, empty root) |
| `bsm.rs` | 18 | norm_cdf known values + NaN propagation (Phase 2A-3), BSM call/put prices, put-call parity (5 strikes), delta (ATM/deep ITM/deep OTM), gamma (positive, peak at ATM), vega (positive), IV recovery (call, put), IV edge cases (zero price, below intrinsic, 0DTE ATM), minutes_to_years, near-expiry no-panic. (Theta test removed with F-017 cleanup 2026-05-08.) |
| `diagnostics.rs` | 5 | Conservation law valid/violated, serde roundtrip, forward-compat partial JSON, default is zero. (Phase 2A-2.) |
| `event.rs` | 6 | F-NEW-R5-1 BBO accessor strictness invariant: boundary triplet `{ask > bid, ask == bid, ask < bid}` + NaN bid / NaN ask, parametric cross-accessor finiteness lock. Plus 1 ignored `test_intraday_underlying_reflected_in_event_underlying_price` (F-013 FIND-UFO contract-lock, blocked on Axis L) |
| `loader.rs` | 0 | F-003 decode_errors counter is regression-tested at the integration level — DEFERRED to Phase 2 (requires corrupt-DBN file-system fixture under `tests/fixtures/`) |
| `moneyness.rs` | 11 | Call ATM/ATM-edge/ITM/deep-ITM/OTM/deep-OTM, put ATM/ITM/OTM, invalid underlying (0, negative, NaN), ratio computation |
| `report_utils.rs` | 4 | dte_bucket_index (including negative DTE), moneyness_index, DTE labels match indices, moneyness labels match indices |
| `quality.rs` | 4 | Quote/trade counting, DTE bucketing, day rollover, finalize structure |
| `spread.rs` | 4 | Spread computation (0.05 USD), sentinel filtering, 0DTE ATM tracking, finalize structure |
| `premium_decay.rs` | 3 | First/last premium tracking with decay calculation (~73%), empty day handling, finalize structure |
| `volume.rs` | 4 | Trade-only counting, PCR with zero calls, DTE bucketing, finalize structure |
| `greeks.rs` | 6 | IV computation fires for ATM, IV sampling with interval (500/10=50), non-ATM ignored, finalize structure, call delta positive / put delta negative (Phase 2B-1), IV sampling resets per day (Phase 2B-3) |
| `zero_dte.rs` | 4 | Non-0DTE filtered, non-ATM filtered, 0DTE ATM counting, call/put separation |
| `put_call.rs` | 4 | PCR computation (50/100=0.5), no-calls-no-PCR, day rollover, finalize structure |
| `effective_spread.rs` | 6 | size_bucket_index, effective spread formula (2*|1.08-1.05|=0.06), trade-through classification (inside/at/outside BBO), quotes ignored, zero-size ignored, finalize structure |
| **Total** | **91 + 1 ignored** | |

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
    09_OsRatio.json            # non-tracker aggregate, when [os_ratio].enabled (§5.9)
```

Each per-tracker JSON contains the tracker's report object plus an injected `_provenance` object with full config, runtime stats, and version; the trailing `NN_OsRatio.json` (the non-tracker O/S aggregate, §5.9) carries the same `_provenance`.
