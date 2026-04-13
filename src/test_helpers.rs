//! Test helpers for constructing synthetic OptionsEvents.

#[cfg(test)]
pub mod helpers {
    use chrono::NaiveDate;

    use crate::contract::{ContractInfo, ContractType};
    use crate::event::{Action, DayContext, OptionsEvent, Side};
    use crate::options_math::moneyness::Moneyness;

    const NS_PER_HOUR: i64 = 3_600_000_000_000;
    const NS_PER_MINUTE: i64 = 60_000_000_000;

    /// RTH open: 14:30 UTC = 09:30 ET (EST)
    pub const RTH_OPEN_UTC_NS: i64 = 14 * NS_PER_HOUR + 30 * NS_PER_MINUTE;

    pub fn make_day_context() -> DayContext {
        DayContext {
            trading_date: NaiveDate::from_ymd_opt(2025, 11, 14).unwrap(),
            utc_offset: -5,
            day_epoch_ns: 0,
            underlying_open: 190.0,
            underlying_close: 190.0,
        }
    }

    pub fn make_contract_call(strike: f64) -> ContractInfo {
        ContractInfo {
            instrument_id: 1,
            underlying: "NVDA".to_string(),
            expiration: NaiveDate::from_ymd_opt(2025, 11, 14).unwrap(),
            contract_type: ContractType::Call,
            strike,
            occ_symbol: format!("NVDA  251114C{:08}", (strike * 1000.0) as u64),
        }
    }

    pub fn make_contract_put(strike: f64) -> ContractInfo {
        ContractInfo {
            instrument_id: 2,
            underlying: "NVDA".to_string(),
            expiration: NaiveDate::from_ymd_opt(2025, 11, 14).unwrap(),
            contract_type: ContractType::Put,
            strike,
            occ_symbol: format!("NVDA  251114P{:08}", (strike * 1000.0) as u64),
        }
    }

    pub fn make_quote_event<'a>(
        contract: &'a ContractInfo,
        bid: f64,
        ask: f64,
        dte: i64,
        moneyness: Moneyness,
    ) -> OptionsEvent<'a> {
        OptionsEvent {
            ts_event: RTH_OPEN_UTC_NS + 5 * NS_PER_MINUTE,
            instrument_id: contract.instrument_id,
            contract,
            action: Action::Quote,
            side: Side::None,
            trade_price: f64::NAN,
            trade_size: 0,
            bid_px: bid,
            ask_px: ask,
            bid_sz: 10,
            ask_sz: 10,
            dte,
            moneyness,
            moneyness_ratio: contract.strike / 190.0,
            underlying_price: 190.0,
        }
    }

    pub fn make_trade_event<'a>(
        contract: &'a ContractInfo,
        price: f64,
        size: u32,
        bid: f64,
        ask: f64,
        dte: i64,
        moneyness: Moneyness,
    ) -> OptionsEvent<'a> {
        OptionsEvent {
            ts_event: RTH_OPEN_UTC_NS + 10 * NS_PER_MINUTE,
            instrument_id: contract.instrument_id,
            contract,
            action: Action::Trade,
            side: Side::Ask,
            trade_price: price,
            trade_size: size,
            bid_px: bid,
            ask_px: ask,
            bid_sz: 10,
            ask_sz: 10,
            dte,
            moneyness,
            moneyness_ratio: contract.strike / 190.0,
            underlying_price: 190.0,
        }
    }
}
