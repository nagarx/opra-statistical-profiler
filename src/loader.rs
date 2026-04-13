//! CMBP-1 file loader for OPRA options data.
//!
//! Reads `.dbn.zst` files containing `CbboMsg` (CMBP-1 schema) records and
//! provides streaming iteration with metadata access for symbology resolution.
//!
//! # Performance
//!
//! Uses a 1 MB I/O buffer matching the MBO profiler pattern. The bottleneck
//! is zstd decompression (single-threaded per file stream).

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use dbn::decode::{DbnMetadata, DecodeRecord, DynDecoder};
use dbn::enums::VersionUpgradePolicy;
use dbn::{CbboMsg, Metadata};

/// I/O buffer size: 1 MB for optimal throughput on modern SSDs.
const IO_BUFFER_SIZE: usize = 1024 * 1024;

/// Loader for OPRA CMBP-1 `.dbn.zst` files.
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
            },
        ))
    }
}

/// Streaming iterator over CMBP-1 records.
pub struct Cmbp1RecordIterator<'a> {
    decoder: DynDecoder<'a, BufReader<File>>,
    count: u64,
}

impl<'a> Cmbp1RecordIterator<'a> {
    /// Number of records yielded so far.
    pub fn count(&self) -> u64 {
        self.count
    }
}

impl<'a> Iterator for Cmbp1RecordIterator<'a> {
    type Item = CbboMsg;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.decoder.decode_record::<CbboMsg>() {
                Ok(Some(record)) => {
                    self.count += 1;
                    return Some(record.clone());
                }
                Ok(None) => return None,
                Err(e) => {
                    log::warn!("Failed to decode CMBP-1 record #{}: {}", self.count, e);
                    continue;
                }
            }
        }
    }
}
