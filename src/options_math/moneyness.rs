//! Moneyness classification for option contracts.
//!
//! Classifies options as ATM, ITM, OTM, deep ITM, or deep OTM based on
//! the ratio of strike price to underlying price.

use serde::{Deserialize, Serialize};

use crate::contract::ContractType;

/// Moneyness classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Moneyness {
    DeepItm,
    Itm,
    Atm,
    Otm,
    DeepOtm,
}

/// Moneyness classification boundaries (configurable).
///
/// For a call option with underlying S and strike K:
/// - moneyness_ratio = K / S
/// - ATM: ratio in [1 - atm_range, 1 + atm_range]
/// - ITM: ratio < 1 - atm_range (for calls)
/// - OTM: ratio > 1 + atm_range (for calls)
///
/// For puts, ITM/OTM are reversed.
pub struct MoneynessBuckets {
    /// Half-width of the ATM band. Default 0.02 = +/- 2%.
    pub atm_range: f64,
    /// Boundary for deep ITM/OTM. Default 0.10 = 10% away from ATM.
    pub deep_range: f64,
}

impl Default for MoneynessBuckets {
    fn default() -> Self {
        Self {
            atm_range: 0.02,
            deep_range: 0.10,
        }
    }
}

impl MoneynessBuckets {
    /// Classify an option's moneyness given strike, underlying price, and contract type.
    ///
    /// Returns `None` if underlying_price is non-positive.
    pub fn classify(
        &self,
        strike: f64,
        underlying_price: f64,
        contract_type: ContractType,
    ) -> Option<Moneyness> {
        if underlying_price <= 0.0 || !underlying_price.is_finite() || !strike.is_finite() {
            return None;
        }

        let ratio = strike / underlying_price;

        let is_itm_side = match contract_type {
            ContractType::Call => ratio < 1.0 - self.atm_range,
            ContractType::Put => ratio > 1.0 + self.atm_range,
        };

        let is_otm_side = match contract_type {
            ContractType::Call => ratio > 1.0 + self.atm_range,
            ContractType::Put => ratio < 1.0 - self.atm_range,
        };

        let is_deep_itm = match contract_type {
            ContractType::Call => ratio < 1.0 - self.deep_range,
            ContractType::Put => ratio > 1.0 + self.deep_range,
        };

        let is_deep_otm = match contract_type {
            ContractType::Call => ratio > 1.0 + self.deep_range,
            ContractType::Put => ratio < 1.0 - self.deep_range,
        };

        if is_deep_itm {
            Some(Moneyness::DeepItm)
        } else if is_deep_otm {
            Some(Moneyness::DeepOtm)
        } else if is_itm_side {
            Some(Moneyness::Itm)
        } else if is_otm_side {
            Some(Moneyness::Otm)
        } else {
            Some(Moneyness::Atm)
        }
    }

    /// Compute the moneyness ratio: strike / underlying_price.
    pub fn ratio(strike: f64, underlying_price: f64) -> f64 {
        if underlying_price > 0.0 && underlying_price.is_finite() {
            strike / underlying_price
        } else {
            f64::NAN
        }
    }
}

impl std::fmt::Display for Moneyness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Moneyness::DeepItm => write!(f, "deep_itm"),
            Moneyness::Itm => write!(f, "itm"),
            Moneyness::Atm => write!(f, "atm"),
            Moneyness::Otm => write!(f, "otm"),
            Moneyness::DeepOtm => write!(f, "deep_otm"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buckets() -> MoneynessBuckets {
        MoneynessBuckets::default()
    }

    #[test]
    fn test_call_atm() {
        let m = buckets()
            .classify(190.0, 190.0, ContractType::Call)
            .unwrap();
        assert_eq!(m, Moneyness::Atm);
    }

    #[test]
    fn test_call_atm_edge() {
        // 2% range: 190 * 0.98 = 186.2, 190 * 1.02 = 193.8
        // Strike 192 is within ATM range
        let m = buckets()
            .classify(192.0, 190.0, ContractType::Call)
            .unwrap();
        assert_eq!(m, Moneyness::Atm, "192/190 = 1.0105, within 2% ATM");
    }

    #[test]
    fn test_call_itm() {
        // Strike 180, stock 190 → ratio = 0.947, below 0.98 ATM boundary
        let m = buckets()
            .classify(180.0, 190.0, ContractType::Call)
            .unwrap();
        assert_eq!(m, Moneyness::Itm);
    }

    #[test]
    fn test_call_deep_itm() {
        // Strike 150, stock 190 → ratio = 0.789, below 0.90 deep boundary
        let m = buckets()
            .classify(150.0, 190.0, ContractType::Call)
            .unwrap();
        assert_eq!(m, Moneyness::DeepItm);
    }

    #[test]
    fn test_call_otm() {
        // Strike 200, stock 190 → ratio = 1.053, above 1.02 ATM boundary
        let m = buckets()
            .classify(200.0, 190.0, ContractType::Call)
            .unwrap();
        assert_eq!(m, Moneyness::Otm);
    }

    #[test]
    fn test_call_deep_otm() {
        // Strike 250, stock 190 → ratio = 1.316, above 1.10 deep boundary
        let m = buckets()
            .classify(250.0, 190.0, ContractType::Call)
            .unwrap();
        assert_eq!(m, Moneyness::DeepOtm);
    }

    #[test]
    fn test_put_atm() {
        let m = buckets()
            .classify(190.0, 190.0, ContractType::Put)
            .unwrap();
        assert_eq!(m, Moneyness::Atm);
    }

    #[test]
    fn test_put_itm() {
        // Put ITM: strike > underlying. Strike 200, stock 190 → ratio 1.053
        let m = buckets()
            .classify(200.0, 190.0, ContractType::Put)
            .unwrap();
        assert_eq!(m, Moneyness::Itm);
    }

    #[test]
    fn test_put_otm() {
        // Put OTM: strike < underlying. Strike 180, stock 190 → ratio 0.947
        let m = buckets()
            .classify(180.0, 190.0, ContractType::Put)
            .unwrap();
        assert_eq!(m, Moneyness::Otm);
    }

    #[test]
    fn test_invalid_underlying() {
        assert!(buckets().classify(190.0, 0.0, ContractType::Call).is_none());
        assert!(buckets()
            .classify(190.0, -10.0, ContractType::Call)
            .is_none());
        assert!(buckets()
            .classify(190.0, f64::NAN, ContractType::Call)
            .is_none());
    }

    #[test]
    fn test_moneyness_ratio() {
        let r = MoneynessBuckets::ratio(185.0, 190.0);
        assert!((r - 185.0 / 190.0).abs() < 1e-10);
    }
}
