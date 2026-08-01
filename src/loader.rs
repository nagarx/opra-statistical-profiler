//! CMBP-1 / CBBO file loader for OPRA options data.
//!
//! Reads `.dbn.zst` files and yields [`Cmbp1Record`] — this crate's internal,
//! decoder-independent record type — with metadata access for symbology
//! resolution.
//!
//! # Record types: why this dispatches on rtype
//!
//! Until dbn v0.20.x a **single** `CbboMsg` struct was annotated for four
//! rtypes — 0xB1 `CMBP_1`, 0xC0 `CBBO_1S`, 0xC1 `CBBO_1M`, 0xC2 `TCBBO` — so
//! `decode_record::<CbboMsg>()` transparently accepted every OPRA file this
//! crate is pointed at. dbn **0.21.0** ("Changed the layout of `CbboMsg` to
//! better match `BboMsg`") SPLIT that type in two:
//!
//! | rtype | v0.20.0 type | v0.64.0 type |
//! |---|---|---|
//! | 0xB1 CMBP-1, 0xC2 TCBBO | `CbboMsg` | [`dbn::Cmbp1Msg`] |
//! | 0xC0 CBBO-1s, 0xC1 CBBO-1m | `CbboMsg` | [`dbn::CbboMsg`] |
//!
//! A single `decode_record::<T>()` can therefore no longer serve both, so this
//! loader decodes a `RecordRef` first and picks the type by rtype. Field
//! offsets did NOT move (all three structs are 80 bytes with identical
//! offsets), so every value read here is bit-identical to what v0.20.0
//! produced — verified over 80,000,000 OPRA CMBP-1 records and 31,039,682
//! CBBO-1m records in `.session_state/2026-08-01-blast-radius/
//! DBN_VERSION_DIFF.md` §2.
//!
//! # What the split takes away on 0xC0/0xC1
//!
//! dbn 0.21.0 also reclassified three byte ranges as **reserved** on CBBO-1s/1m
//! — `action` \[28..29\), `ts_in_delta` \[40..44\) and `sequence` \[44..48\).
//! v0.20.0 exposed them as fields; v0.64.0 does not, because the vendor no
//! longer defines them there (corroborated by dbn 0.35.0's bug-fix bullet
//! "Removed incorrect `sequence` and `depth` Python type stubs for `CMBP1Msg`
//! and `CBBOMsg`"). This loader therefore does NOT read those bytes for CBBO
//! records: `action` is set to [`ACTION_UNAVAILABLE`] and `ts_in_delta` to 0,
//! and every such record is counted in
//! [`cbbo_action_unavailable`](Cmbp1RecordIterator::cbbo_action_unavailable)
//! and warned about once per file. Nothing is dropped silently.
//!
//! Measured on real OPRA data (2026-08-01): the vendor is itself inconsistent
//! about that byte on 0xC1 — the NVDA 0DTE panel carries `0` on 100% of 77,220
//! records (so this crate already classified them all `Action::Other`, and the
//! change is a no-op there), while the SPX/SPY/SPXW INDEX acquisition carries
//! `'A'`/`'T'`. That inconsistency is exactly what "reserved" means, and is why
//! the byte is not read.
//!
//! # Performance
//!
//! Uses a 1 MB I/O buffer matching the MBO profiler pattern. The bottleneck
//! is zstd decompression (single-threaded per file stream).

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use dbn::decode::{DbnMetadata, DecodeRecordRef, DynDecoder};
use dbn::enums::rtype;
use dbn::enums::VersionUpgradePolicy;
// `Record` is the trait that provides `RecordRef::header()`.
use dbn::{CbboMsg, Cmbp1Msg, Metadata, Record};

/// I/O buffer size: 1 MB for optimal throughput on modern SSDs.
const IO_BUFFER_SIZE: usize = 1024 * 1024;

/// Sentinel written to [`Cmbp1Record::action`] when the source schema has no
/// `action` field at all (CBBO-1s/1m — see the module docs).
///
/// `0` is deliberately not a valid DBN action code (`A`/`C`/`F`/`M`/`R`/`T`),
/// so it falls through every action `match` to the profiler's `Action::Other`
/// bucket rather than impersonating a quote or a trade.
pub const ACTION_UNAVAILABLE: u8 = 0;

/// How many per-record `unsupported_rtype` warnings to emit before suppressing
/// the rest for the remainder of the file.
///
/// A wrong-schema file is wrong on EVERY record, so an uncapped per-record warn
/// would emit one line per record — on a 29 GB OPRA day that is ~10^9 log lines.
/// The condition stays fully observable without them: the
/// [`unsupported_rtype`](Cmbp1RecordIterator::unsupported_rtype) counter is
/// exact regardless of the cap, is surfaced in every tracker JSON, and the
/// suppression itself is announced. This bounds log VOLUME, never information.
const MAX_UNSUPPORTED_RTYPE_WARNINGS: u64 = 10;

/// Which DBN record type a [`Cmbp1Record`] was decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordSource {
    /// rtype 0xB1 (CMBP-1) or 0xC2 (TCBBO), decoded as [`dbn::Cmbp1Msg`].
    /// Every field of [`Cmbp1Record`] is a real field of the source schema.
    Cmbp1,
    /// rtype 0xC0/0xC1 (CBBO-1s/1m), decoded as [`dbn::CbboMsg`].
    /// `action` is [`ACTION_UNAVAILABLE`] and `ts_in_delta` is 0 — neither
    /// exists in that schema.
    Cbbo,
}

/// Internal representation of a consolidated top-of-book record.
///
/// Decoder-independent by design: the profiler works with this type only, so a
/// future dbn type reshuffle is contained in this file. Prices stay as i64
/// nanodollars; conversion to f64 USD happens in the profiler.
#[derive(Debug, Clone)]
pub struct Cmbp1Record {
    /// Which schema this came from — see [`RecordSource`].
    pub source: RecordSource,
    /// `hd.instrument_id`.
    pub instrument_id: u32,
    /// `hd.ts_event` — exchange timestamp, UTC nanoseconds.
    pub ts_event: u64,
    /// `hd.publisher_id`.
    pub publisher_id: u16,
    /// Trade price in nanodollars (i64 fixed-point). On CBBO this is the
    /// interval's last trade price.
    pub price: i64,
    /// Trade size in contracts. On CBBO this is the interval's last trade size.
    pub size: u32,
    /// Event action byte (`b'T'`, `b'A'`, ...), or [`ACTION_UNAVAILABLE`] when
    /// the source schema has no `action` field.
    pub action: u8,
    /// Event side byte (`b'A'`, `b'B'`, `b'N'`). Real in both schemas.
    pub side: u8,
    /// Raw `FlagSet` byte.
    pub flags: u8,
    /// Matching-engine-sending delta in ns. `0` for CBBO (no such field).
    pub ts_in_delta: i32,
    /// Capture-server-received timestamp, UTC nanoseconds.
    pub ts_recv: u64,
    /// `levels[0].bid_px`, nanodollars.
    pub bid_px: i64,
    /// `levels[0].ask_px`, nanodollars.
    pub ask_px: i64,
    /// `levels[0].bid_sz`.
    pub bid_sz: u32,
    /// `levels[0].ask_sz`.
    pub ask_sz: u32,
    /// `levels[0].bid_pb` — venue holding the best bid.
    pub bid_pb: u16,
    /// `levels[0].ask_pb` — venue holding the best ask.
    pub ask_pb: u16,
}

impl Cmbp1Record {
    /// Convert from [`dbn::Cmbp1Msg`] (rtype 0xB1 CMBP-1 / 0xC2 TCBBO).
    ///
    /// Every field below is a real, documented field at dbn v0.64.0. The bytes
    /// at \[44..48\) — `CbboMsg::sequence` under the v0.20.0 pin — are NOT read:
    /// they are `_reserved2` from dbn 0.21.0 on, and nothing in this crate ever
    /// consumed the value (it was written to `OptionsEvent.sequence` and read
    /// by no tracker and no exporter).
    fn from_cmbp1(msg: &Cmbp1Msg) -> Self {
        Self {
            source: RecordSource::Cmbp1,
            instrument_id: msg.hd.instrument_id,
            ts_event: msg.hd.ts_event,
            publisher_id: msg.hd.publisher_id,
            price: msg.price,
            size: msg.size,
            action: msg.action as u8,
            side: msg.side as u8,
            flags: msg.flags.raw(),
            ts_in_delta: msg.ts_in_delta,
            ts_recv: msg.ts_recv,
            bid_px: msg.levels[0].bid_px,
            ask_px: msg.levels[0].ask_px,
            bid_sz: msg.levels[0].bid_sz,
            ask_sz: msg.levels[0].ask_sz,
            bid_pb: msg.levels[0].bid_pb,
            ask_pb: msg.levels[0].ask_pb,
        }
    }

    /// Convert from [`dbn::CbboMsg`] (rtype 0xC0 CBBO-1s / 0xC1 CBBO-1m).
    ///
    /// `action` and `ts_in_delta` are absent from this schema at dbn v0.64.0
    /// (`_reserved1` and `_reserved3` respectively), so they are filled with
    /// [`ACTION_UNAVAILABLE`] and `0` rather than read out of reserved bytes.
    fn from_cbbo(msg: &CbboMsg) -> Self {
        Self {
            source: RecordSource::Cbbo,
            instrument_id: msg.hd.instrument_id,
            ts_event: msg.hd.ts_event,
            publisher_id: msg.hd.publisher_id,
            price: msg.price,
            size: msg.size,
            action: ACTION_UNAVAILABLE,
            side: msg.side as u8,
            flags: msg.flags.raw(),
            ts_in_delta: 0,
            ts_recv: msg.ts_recv,
            bid_px: msg.levels[0].bid_px,
            ask_px: msg.levels[0].ask_px,
            bid_sz: msg.levels[0].bid_sz,
            ask_sz: msg.levels[0].ask_sz,
            bid_pb: msg.levels[0].bid_pb,
            ask_pb: msg.levels[0].ask_pb,
        }
    }
}

/// Loader for OPRA CMBP-1 / CBBO `.dbn.zst` files.
pub struct Cmbp1Loader {
    path: PathBuf,
}

impl Cmbp1Loader {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(format!("File not found: {}", path.display()).into());
        }
        Ok(Self { path })
    }

    /// Open the file and return metadata + a streaming record iterator.
    ///
    /// The metadata contains symbology mappings needed to resolve instrument_ids
    /// to OCC option symbols.
    ///
    /// `VersionUpgradePolicy::AsIs` is load-bearing and must NOT be changed:
    /// dbn's default flipped to `UpgradeToV3` between the v0.20.0 and v0.64.0
    /// pins, and that policy rewrites Definition and Statistics records
    /// (measured: 414-422 differing lines). `AsIs` is what produced every
    /// number on disk.
    pub fn open(&self) -> Result<(Metadata, Cmbp1RecordIterator<'_>), Box<dyn std::error::Error>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::with_capacity(IO_BUFFER_SIZE, file);
        let decoder = DynDecoder::inferred_with_buffer(reader, VersionUpgradePolicy::AsIs)?;
        let metadata = decoder.metadata().clone();

        Ok((
            metadata,
            Cmbp1RecordIterator {
                decoder,
                count: 0,
                decode_errors: 0,
                unsupported_rtype: 0,
                cbbo_action_unavailable: 0,
                cbbo_warned: false,
            },
        ))
    }
}

/// Decode outcome for one record — owned, so the decoder borrow ends before
/// the iterator touches its own counters.
enum Decoded {
    Record(Cmbp1Record),
    Eof,
    DecodeError(dbn::Error),
    /// rtype is a known CMBP-1/CBBO type but the encoded record is shorter than
    /// the struct (truncated, or an older record revision).
    ShortRecord(u8),
    /// rtype belongs to no schema this profiler decodes.
    UnsupportedRtype(u8),
}

/// Streaming iterator over consolidated top-of-book records.
///
/// Exposes four cumulative counters consumable post-iteration:
/// - [`count`](Self::count): records successfully decoded and yielded.
/// - [`decode_errors`](Self::decode_errors): records whose decoding failed
///   mid-stream (warn-logged + skipped). Closes F-003 per hft-rules §8 (record
///   diagnostics, never silently drop). Read AFTER iteration completes; the
///   field is owned by the iterator, so callers must keep the iterator alive
///   (use `by_ref()` with `for`).
/// - [`unsupported_rtype`](Self::unsupported_rtype): records whose rtype is
///   neither CMBP-1/TCBBO nor CBBO-1s/1m. Warn-logged and skipped — never a
///   silent `_ => {}`.
/// - [`cbbo_action_unavailable`](Self::cbbo_action_unavailable): records that
///   came from CBBO-1s/1m and therefore carry no `action`. See the module docs.
pub struct Cmbp1RecordIterator<'a> {
    decoder: DynDecoder<'a, BufReader<File>>,
    count: u64,
    decode_errors: u64,
    unsupported_rtype: u64,
    cbbo_action_unavailable: u64,
    cbbo_warned: bool,
}

impl Cmbp1RecordIterator<'_> {
    /// Number of records yielded so far.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Number of records whose decoding failed and were skipped so far.
    ///
    /// Increments alongside the `log::warn!` emission in the error arm of
    /// [`Iterator::next`]. Closes F-003 (silent decode-error swallow) per
    /// hft-rules §8.
    pub fn decode_errors(&self) -> u64 {
        self.decode_errors
    }

    /// Number of records skipped because their rtype is not one this profiler
    /// decodes. Non-zero means the loader was pointed at a file whose schema it
    /// does not understand — a configuration error, not a data property.
    pub fn unsupported_rtype(&self) -> u64 {
        self.unsupported_rtype
    }

    /// Number of yielded records that came from CBBO-1s/1m and therefore have
    /// `action == ACTION_UNAVAILABLE`. Equals `count()` for a pure CBBO file
    /// and `0` for a pure CMBP-1 file.
    pub fn cbbo_action_unavailable(&self) -> u64 {
        self.cbbo_action_unavailable
    }
}

impl Iterator for Cmbp1RecordIterator<'_> {
    type Item = Cmbp1Record;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Decode + convert inside one expression so the `RecordRef` borrow
            // of `self.decoder` is released before the counters below are hit.
            let decoded = match self.decoder.decode_record_ref() {
                Ok(Some(rec)) => {
                    let rt = rec.header().rtype;
                    match rt {
                        // 0xB1 CMBP-1 and 0xC2 TCBBO: dbn::Cmbp1Msg since 0.21.0.
                        rtype::CMBP_1 | rtype::TCBBO => match rec.get::<Cmbp1Msg>() {
                            Some(m) => Decoded::Record(Cmbp1Record::from_cmbp1(m)),
                            None => Decoded::ShortRecord(rt),
                        },
                        // 0xC0/0xC1 CBBO-1s/1m: dbn::CbboMsg since 0.21.0.
                        rtype::CBBO_1S | rtype::CBBO_1M => match rec.get::<CbboMsg>() {
                            Some(m) => Decoded::Record(Cmbp1Record::from_cbbo(m)),
                            None => Decoded::ShortRecord(rt),
                        },
                        // NOT a silent fallthrough: rtype is a u8, so a catch-all
                        // is unavoidable, but every record reaching it is counted
                        // and warn-logged. hft-rules §8: never silently drop.
                        other => Decoded::UnsupportedRtype(other),
                    }
                }
                Ok(None) => Decoded::Eof,
                Err(e) => Decoded::DecodeError(e),
            };

            match decoded {
                Decoded::Record(record) => {
                    if record.source == RecordSource::Cbbo {
                        self.cbbo_action_unavailable += 1;
                        if !self.cbbo_warned {
                            self.cbbo_warned = true;
                            log::warn!(
                                "CBBO-1s/1m input: the CBBO schema has no `action` field \
                                 (dbn 0.21.0 reclassified byte 28 as reserved), so every \
                                 record from this file is dispatched as Action::Other and \
                                 contributes 0 trade volume. Point the profiler at a CMBP-1 \
                                 (.cmbp-1) file for trade-aware statistics."
                            );
                        }
                    }
                    self.count += 1;
                    return Some(record);
                }
                Decoded::Eof => return None,
                Decoded::DecodeError(e) => {
                    self.decode_errors += 1;
                    log::warn!(
                        "Failed to decode record #{} (decode_errors={}): {}",
                        self.count,
                        self.decode_errors,
                        e
                    );
                    continue;
                }
                Decoded::ShortRecord(rt) => {
                    self.decode_errors += 1;
                    log::warn!(
                        "Record #{} has rtype 0x{:02X} but is shorter than the struct it \
                         maps to (truncated or an older record revision); skipped \
                         (decode_errors={})",
                        self.count,
                        rt,
                        self.decode_errors
                    );
                    continue;
                }
                Decoded::UnsupportedRtype(rt) => {
                    self.unsupported_rtype += 1;
                    // Bounded warn, exact counter — see MAX_UNSUPPORTED_RTYPE_WARNINGS.
                    if self.unsupported_rtype <= MAX_UNSUPPORTED_RTYPE_WARNINGS {
                        log::warn!(
                            "Skipped record with rtype 0x{:02X}, which this profiler does not \
                             decode (expected 0x{:02X} CMBP-1, 0x{:02X} TCBBO, 0x{:02X} CBBO-1s \
                             or 0x{:02X} CBBO-1m); unsupported_rtype={}",
                            rt,
                            rtype::CMBP_1,
                            rtype::TCBBO,
                            rtype::CBBO_1S,
                            rtype::CBBO_1M,
                            self.unsupported_rtype
                        );
                        if self.unsupported_rtype == MAX_UNSUPPORTED_RTYPE_WARNINGS {
                            log::warn!(
                                "Suppressing further unsupported-rtype warnings for this file \
                                 after {}. This file's schema is not one this profiler decodes — \
                                 the exact total is reported as \
                                 `_provenance.diagnostics.unsupported_rtype`.",
                                MAX_UNSUPPORTED_RTYPE_WARNINGS
                            );
                        }
                    }
                    continue;
                }
            }
        }
    }
}
