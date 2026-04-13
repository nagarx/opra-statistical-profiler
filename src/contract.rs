//! OCC option symbol parser and contract metadata.
//!
//! Parses standard OCC option symbols into structured contract information.
//!
//! # OCC Symbol Format
//!
//! ```text
//! NVDA  251114C00185000
//! ^^^^  ^^^^^^ ^^^^^^^^
//! root  YYMMDD strike*1000
//!       expiry C/P
//! ```
//!
//! - Root: 1-6 characters, left-padded with spaces to 6 chars
//! - Expiry: YYMMDD
//! - Type: C = Call, P = Put
//! - Strike: 8 digits, price * 1000 (e.g., 00185000 = $185.000)
//!
//! Reference: OCC Symbology Standard (Options Clearing Corporation)

use ahash::AHashMap;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Option contract type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContractType {
    Call,
    Put,
}

/// Parsed option contract metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractInfo {
    pub instrument_id: u32,
    pub underlying: String,
    pub expiration: NaiveDate,
    pub contract_type: ContractType,
    /// Strike price in USD.
    pub strike: f64,
    pub occ_symbol: String,
}

impl ContractInfo {
    /// Days to expiration from a given trading date.
    pub fn dte(&self, trading_date: NaiveDate) -> i64 {
        (self.expiration - trading_date).num_days()
    }

    /// Whether this contract expires on the given date (0DTE).
    pub fn is_zero_dte(&self, trading_date: NaiveDate) -> bool {
        self.expiration == trading_date
    }
}

/// Parse an OCC option symbol into contract metadata.
///
/// Returns `None` if the symbol is malformed or too short.
///
/// # Arguments
/// * `occ_symbol` — Raw OCC symbol, possibly with trailing spaces (21 chars padded)
/// * `instrument_id` — The dbn instrument_id for this contract
pub fn parse_occ_symbol(occ_symbol: &str, instrument_id: u32) -> Option<ContractInfo> {
    let s = occ_symbol.trim_end();
    if s.len() < 15 {
        return None;
    }

    let root = s[..6].trim();
    if root.is_empty() {
        return None;
    }

    let date_str = &s[6..12];
    let cp_char = s.as_bytes()[12];
    let strike_str = &s[13..];

    if strike_str.len() < 8 {
        return None;
    }

    let year = 2000 + date_str[0..2].parse::<i32>().ok()?;
    let month = date_str[2..4].parse::<u32>().ok()?;
    let day = date_str[4..6].parse::<u32>().ok()?;
    let expiration = NaiveDate::from_ymd_opt(year, month, day)?;

    let contract_type = match cp_char {
        b'C' => ContractType::Call,
        b'P' => ContractType::Put,
        _ => return None,
    };

    let strike_raw: u64 = strike_str.parse().ok()?;
    let strike = strike_raw as f64 / 1000.0;

    Some(ContractInfo {
        instrument_id,
        underlying: root.to_string(),
        expiration,
        contract_type,
        strike,
        occ_symbol: s.to_string(),
    })
}

/// Map from instrument_id to ContractInfo, built from dbn metadata symbology.
pub struct ContractMap {
    map: AHashMap<u32, ContractInfo>,
}

impl ContractMap {
    /// Build from dbn `Metadata` symbology for a specific trading date.
    ///
    /// Parses all `SymbolMapping` entries where the raw_symbol maps to an
    /// instrument_id valid for the given date.
    pub fn from_dbn_metadata(metadata: &dbn::Metadata, date: time::Date) -> Self {
        let mut map = AHashMap::new();

        let pit_map = match metadata.symbol_map_for_date(date) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to build symbol map for {}: {}", date, e);
                return Self { map };
            }
        };

        for (&iid, raw_symbol) in pit_map.inner() {
            if let Some(info) = parse_occ_symbol(raw_symbol, iid) {
                map.insert(iid, info);
            }
        }

        log::info!(
            "ContractMap: {} contracts parsed for {}",
            map.len(),
            date
        );
        Self { map }
    }

    /// Look up contract info by instrument_id.
    pub fn get(&self, instrument_id: u32) -> Option<&ContractInfo> {
        self.map.get(&instrument_id)
    }

    /// Total number of contracts.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterate over all contracts.
    pub fn iter(&self) -> impl Iterator<Item = (&u32, &ContractInfo)> {
        self.map.iter()
    }

    /// Count contracts by type.
    pub fn count_by_type(&self) -> (usize, usize) {
        let calls = self
            .map
            .values()
            .filter(|c| c.contract_type == ContractType::Call)
            .count();
        let puts = self.map.len() - calls;
        (calls, puts)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_standard_call() {
        let info = parse_occ_symbol("NVDA  251114C00185000", 12345).unwrap();
        assert_eq!(info.underlying, "NVDA");
        assert_eq!(info.expiration, NaiveDate::from_ymd_opt(2025, 11, 14).unwrap());
        assert_eq!(info.contract_type, ContractType::Call);
        assert!((info.strike - 185.0).abs() < 1e-10, "Strike: {}", info.strike);
        assert_eq!(info.instrument_id, 12345);
    }

    #[test]
    fn test_parse_standard_put() {
        let info = parse_occ_symbol("NVDA  251114P00190000", 99999).unwrap();
        assert_eq!(info.contract_type, ContractType::Put);
        assert!((info.strike - 190.0).abs() < 1e-10);
    }

    #[test]
    fn test_parse_fractional_strike() {
        // $152.50 = 00152500
        let info = parse_occ_symbol("NVDA  251121C00152500", 1).unwrap();
        assert!((info.strike - 152.5).abs() < 1e-10, "Strike: {}", info.strike);
    }

    #[test]
    fn test_parse_small_strike() {
        // $3.50 = 00003500
        let info = parse_occ_symbol("NVDA  251219C00003500", 1).unwrap();
        assert!((info.strike - 3.5).abs() < 1e-10, "Strike: {}", info.strike);
    }

    #[test]
    fn test_parse_large_strike() {
        // $420 = 00420000
        let info = parse_occ_symbol("NVDA  251114C00420000", 1).unwrap();
        assert!((info.strike - 420.0).abs() < 1e-10);
    }

    #[test]
    fn test_parse_leaps_expiry() {
        // 2028-01-21
        let info = parse_occ_symbol("NVDA  280121C00130000", 1).unwrap();
        assert_eq!(info.expiration, NaiveDate::from_ymd_opt(2028, 1, 21).unwrap());
    }

    #[test]
    fn test_dte_calculation() {
        let info = parse_occ_symbol("NVDA  251121C00150000", 1).unwrap();
        let trading_date = NaiveDate::from_ymd_opt(2025, 11, 14).unwrap();
        assert_eq!(info.dte(trading_date), 7);
    }

    #[test]
    fn test_zero_dte() {
        let info = parse_occ_symbol("NVDA  251114C00190000", 1).unwrap();
        let trading_date = NaiveDate::from_ymd_opt(2025, 11, 14).unwrap();
        assert!(info.is_zero_dte(trading_date));
        assert_eq!(info.dte(trading_date), 0);
    }

    #[test]
    fn test_parse_malformed_too_short() {
        assert!(parse_occ_symbol("NV", 1).is_none());
    }

    #[test]
    fn test_parse_malformed_bad_type() {
        assert!(parse_occ_symbol("NVDA  251114X00190000", 1).is_none());
    }

    #[test]
    fn test_parse_malformed_bad_date() {
        assert!(parse_occ_symbol("NVDA  251332C00190000", 1).is_none());
    }

    #[test]
    fn test_parse_empty_root() {
        assert!(parse_occ_symbol("      251114C00190000", 1).is_none());
    }
}
