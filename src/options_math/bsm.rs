//! Black-Scholes-Merton option pricing, implied volatility, and Greeks.
//!
//! # Formulas
//!
//! BSM call price (Black & Scholes 1973, Merton 1973):
//!
//! ```text
//! C = S * N(d1) - K * e^(-rT) * N(d2)
//! P = K * e^(-rT) * N(-d2) - S * N(-d1)
//!
//! d1 = [ln(S/K) + (r + sigma^2/2) * T] / (sigma * sqrt(T))
//! d2 = d1 - sigma * sqrt(T)
//! ```
//!
//! Implied volatility: Newton-Raphson on vega with Brenner-Subrahmanyam (1988) initial guess.
//!
//! # References
//!
//! - Black, F. & Scholes, M. (1973). "The Pricing of Options and Corporate Liabilities."
//!   Journal of Political Economy, 81(3), 637-654.
//! - Brenner, M. & Subrahmanyam, M.G. (1988). "A Simple Formula to Compute the
//!   Implied Standard Deviation." Financial Analysts Journal, 44(5), 80-83.

use std::f64::consts::PI;

/// Minimum time-to-expiry to avoid division by zero.
/// Equivalent to ~1 second of trading time.
const MIN_T: f64 = 1e-6;

/// Maximum IV iterations for Newton-Raphson.
const MAX_IV_ITER: usize = 100;

/// IV convergence tolerance.
const IV_TOL: f64 = 1e-8;

/// Maximum reasonable IV (1000% annualized).
const MAX_IV: f64 = 10.0;

/// Minimum reasonable IV.
const MIN_IV: f64 = 1e-6;

/// Standard normal CDF via Hart (1968) rational approximation.
///
/// Accuracy: |error| < 7.5e-8.
///
/// Uses the relation N(x) = 0.5 * erfc(-x / sqrt(2)) implemented via
/// the Horner-form rational approximation from Hart et al., "Computer
/// Approximations", Wiley, 1968.
fn norm_cdf(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    // Use the identity: N(x) = 0.5 * erfc(-x / sqrt(2))
    // Implement erfc via Abramowitz & Stegun 7.1.26 with constants from Hart (1968).
    0.5 * erfc_approx(-x * std::f64::consts::FRAC_1_SQRT_2)
}

/// Complementary error function approximation.
///
/// erfc(x) = 1 - erf(x), where erf is the error function.
/// For x >= 0, uses rational approximation. For x < 0, uses erfc(-x) = 2 - erfc(x).
fn erfc_approx(x: f64) -> f64 {
    if x < 0.0 {
        return 2.0 - erfc_approx(-x);
    }

    // Abramowitz & Stegun 7.1.26
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let poly = t
        * (0.254829592
            + t * (-0.284496736
                + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
    poly * (-x * x).exp()
}

/// Standard normal PDF.
fn norm_pdf(x: f64) -> f64 {
    (-x * x / 2.0).exp() / (2.0 * PI).sqrt()
}

/// Compute d1 and d2 for BSM formula.
fn d1_d2(s: f64, k: f64, t: f64, r: f64, sigma: f64) -> (f64, f64) {
    let sqrt_t = t.sqrt();
    let d1 = ((s / k).ln() + (r + sigma * sigma / 2.0) * t) / (sigma * sqrt_t);
    let d2 = d1 - sigma * sqrt_t;
    (d1, d2)
}

/// BSM European call price.
///
/// # Arguments
/// * `s` — Current underlying price (USD)
/// * `k` — Strike price (USD)
/// * `t` — Time to expiry in years (e.g., 1/252 for 1 trading day)
/// * `r` — Risk-free rate (annualized, e.g., 0.05 for 5%)
/// * `sigma` — Volatility (annualized, e.g., 0.30 for 30%)
pub fn call_price(s: f64, k: f64, t: f64, r: f64, sigma: f64) -> f64 {
    if t < MIN_T || sigma < MIN_IV || s <= 0.0 || k <= 0.0 {
        return intrinsic_call(s, k);
    }
    let (d1, d2) = d1_d2(s, k, t, r, sigma);
    s * norm_cdf(d1) - k * (-r * t).exp() * norm_cdf(d2)
}

/// BSM European put price.
pub fn put_price(s: f64, k: f64, t: f64, r: f64, sigma: f64) -> f64 {
    if t < MIN_T || sigma < MIN_IV || s <= 0.0 || k <= 0.0 {
        return intrinsic_put(s, k);
    }
    let (d1, d2) = d1_d2(s, k, t, r, sigma);
    k * (-r * t).exp() * norm_cdf(-d2) - s * norm_cdf(-d1)
}

fn intrinsic_call(s: f64, k: f64) -> f64 {
    (s - k).max(0.0)
}

fn intrinsic_put(s: f64, k: f64) -> f64 {
    (k - s).max(0.0)
}

/// BSM vega: dC/d(sigma) (same for calls and puts).
///
/// vega = S * sqrt(T) * N'(d1)
pub fn vega(s: f64, k: f64, t: f64, r: f64, sigma: f64) -> f64 {
    if t < MIN_T || sigma < MIN_IV || s <= 0.0 || k <= 0.0 {
        return 0.0;
    }
    let (d1, _) = d1_d2(s, k, t, r, sigma);
    s * t.sqrt() * norm_pdf(d1)
}

/// BSM delta.
///
/// Call delta = N(d1), Put delta = N(d1) - 1
pub fn delta(s: f64, k: f64, t: f64, r: f64, sigma: f64, is_call: bool) -> f64 {
    if t < MIN_T || sigma < MIN_IV || s <= 0.0 || k <= 0.0 {
        if is_call {
            return if s > k { 1.0 } else { 0.0 };
        } else {
            return if s < k { -1.0 } else { 0.0 };
        }
    }
    let (d1, _) = d1_d2(s, k, t, r, sigma);
    if is_call {
        norm_cdf(d1)
    } else {
        norm_cdf(d1) - 1.0
    }
}

/// BSM gamma (same for calls and puts).
///
/// gamma = N'(d1) / (S * sigma * sqrt(T))
pub fn gamma(s: f64, k: f64, t: f64, r: f64, sigma: f64) -> f64 {
    if t < MIN_T || sigma < MIN_IV || s <= 0.0 || k <= 0.0 {
        return 0.0;
    }
    let (d1, _) = d1_d2(s, k, t, r, sigma);
    norm_pdf(d1) / (s * sigma * t.sqrt())
}

/// BSM theta (per calendar day, negative for long positions).
///
/// Call theta = -S*N'(d1)*sigma/(2*sqrt(T)) - r*K*e^(-rT)*N(d2)
/// Put theta  = -S*N'(d1)*sigma/(2*sqrt(T)) + r*K*e^(-rT)*N(-d2)
///
/// Divided by 365 to express as per-calendar-day.
pub fn theta(s: f64, k: f64, t: f64, r: f64, sigma: f64, is_call: bool) -> f64 {
    if t < MIN_T || sigma < MIN_IV || s <= 0.0 || k <= 0.0 {
        return 0.0;
    }
    let (d1, d2) = d1_d2(s, k, t, r, sigma);
    let term1 = -s * norm_pdf(d1) * sigma / (2.0 * t.sqrt());
    let annual_theta = if is_call {
        term1 - r * k * (-r * t).exp() * norm_cdf(d2)
    } else {
        term1 + r * k * (-r * t).exp() * norm_cdf(-d2)
    };
    annual_theta / 365.0
}

/// Implied volatility via Newton-Raphson with Brenner-Subrahmanyam (1988) initial guess.
///
/// # Arguments
/// * `market_price` — Observed option price (mid or last trade)
/// * `s` — Underlying price
/// * `k` — Strike
/// * `t` — Time to expiry (years)
/// * `r` — Risk-free rate
/// * `is_call` — true for call, false for put
///
/// # Returns
/// `Some(iv)` if converged, `None` if price violates arbitrage bounds or solver fails.
pub fn implied_vol(
    market_price: f64,
    s: f64,
    k: f64,
    t: f64,
    r: f64,
    is_call: bool,
) -> Option<f64> {
    if !market_price.is_finite() || market_price <= 0.0 || s <= 0.0 || k <= 0.0 || t < MIN_T {
        return None;
    }

    let intrinsic = if is_call {
        intrinsic_call(s, k)
    } else {
        intrinsic_put(s, k)
    };
    if market_price < intrinsic - 0.001 {
        return None;
    }

    // Brenner-Subrahmanyam (1988) initial guess: sigma_0 = sqrt(2*pi/T) * C/S
    let mut sigma = ((2.0 * PI / t).sqrt() * market_price / s).clamp(0.10, MAX_IV);

    for _ in 0..MAX_IV_ITER {
        let price = if is_call {
            call_price(s, k, t, r, sigma)
        } else {
            put_price(s, k, t, r, sigma)
        };

        let diff = price - market_price;
        if diff.abs() < IV_TOL {
            return Some(sigma);
        }

        let v = vega(s, k, t, r, sigma);
        if v < 1e-12 {
            return None;
        }

        sigma -= diff / v;
        sigma = sigma.clamp(MIN_IV, MAX_IV);
    }

    None
}

/// Time to expiry in years for a given number of minutes remaining in the trading day.
///
/// Uses 252 trading days per year, 390 minutes per trading day.
/// Clamps to MIN_T to avoid division by zero near expiry.
pub fn minutes_to_years(minutes_remaining: f64) -> f64 {
    (minutes_remaining / (252.0 * 390.0)).max(MIN_T)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_norm_cdf_known_values() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((norm_cdf(1.0) - 0.8413447).abs() < 1e-5);
        assert!((norm_cdf(-1.0) - 0.1586553).abs() < 1e-5);
        assert!((norm_cdf(2.0) - 0.9772499).abs() < 1e-5);
    }

    #[test]
    fn test_call_price_known() {
        // S=100, K=100, T=1, r=0.05, sigma=0.20
        // Expected: ~10.4506 (textbook BSM ATM call)
        let c = call_price(100.0, 100.0, 1.0, 0.05, 0.20);
        assert!(
            (c - 10.4506).abs() < 0.01,
            "BSM ATM call: expected ~10.45, got {:.4}",
            c
        );
    }

    #[test]
    fn test_put_price_known() {
        // S=100, K=100, T=1, r=0.05, sigma=0.20
        // Put = Call - S + K*e^(-rT) (put-call parity)
        let c = call_price(100.0, 100.0, 1.0, 0.05, 0.20);
        let p = put_price(100.0, 100.0, 1.0, 0.05, 0.20);
        let parity = c - p - 100.0 + 100.0 * (-0.05_f64).exp();
        assert!(
            parity.abs() < 1e-8,
            "Put-call parity violated: residual = {:.10}",
            parity
        );
    }

    #[test]
    fn test_put_call_parity() {
        // Test across multiple strikes
        for k in [80.0, 90.0, 100.0, 110.0, 120.0] {
            let s = 100.0;
            let t = 0.25;
            let r = 0.05;
            let sigma = 0.30;
            let c = call_price(s, k, t, r, sigma);
            let p = put_price(s, k, t, r, sigma);
            let parity = c - p - s + k * (-r * t).exp();
            assert!(
                parity.abs() < 1e-8,
                "Parity violated at K={}: residual = {:.10}",
                k,
                parity
            );
        }
    }

    #[test]
    fn test_delta_atm_near_half() {
        let d = delta(100.0, 100.0, 0.25, 0.05, 0.20, true);
        assert!(
            (d - 0.5).abs() < 0.1,
            "ATM call delta should be near 0.5, got {}",
            d
        );
    }

    #[test]
    fn test_delta_deep_itm_call_near_one() {
        let d = delta(200.0, 100.0, 0.25, 0.05, 0.20, true);
        assert!(d > 0.99, "Deep ITM call delta should be ~1.0, got {}", d);
    }

    #[test]
    fn test_delta_deep_otm_call_near_zero() {
        let d = delta(100.0, 200.0, 0.25, 0.05, 0.20, true);
        assert!(d < 0.01, "Deep OTM call delta should be ~0.0, got {}", d);
    }

    #[test]
    fn test_gamma_positive() {
        let g = gamma(100.0, 100.0, 0.25, 0.05, 0.20);
        assert!(g > 0.0, "Gamma should be positive, got {}", g);
    }

    #[test]
    fn test_gamma_highest_at_atm() {
        let g_atm = gamma(100.0, 100.0, 0.25, 0.05, 0.30);
        let g_itm = gamma(100.0, 80.0, 0.25, 0.05, 0.30);
        let g_otm = gamma(100.0, 120.0, 0.25, 0.05, 0.30);
        assert!(
            g_atm > g_itm && g_atm > g_otm,
            "Gamma should peak at ATM: ATM={}, ITM={}, OTM={}",
            g_atm,
            g_itm,
            g_otm
        );
    }

    #[test]
    fn test_theta_negative_for_long_call() {
        let th = theta(100.0, 100.0, 0.25, 0.05, 0.20, true);
        assert!(th < 0.0, "Long call theta should be negative, got {}", th);
    }

    #[test]
    fn test_vega_positive() {
        let v = vega(100.0, 100.0, 0.25, 0.05, 0.20);
        assert!(v > 0.0, "Vega should be positive, got {}", v);
    }

    #[test]
    fn test_implied_vol_recovery() {
        let true_sigma = 0.30;
        let price = call_price(100.0, 100.0, 0.25, 0.05, true_sigma);
        let recovered = implied_vol(price, 100.0, 100.0, 0.25, 0.05, true).unwrap();
        assert!(
            (recovered - true_sigma).abs() < 1e-6,
            "IV recovery: expected {}, got {}",
            true_sigma,
            recovered
        );
    }

    #[test]
    fn test_implied_vol_recovery_put() {
        let true_sigma = 0.25;
        let price = put_price(100.0, 105.0, 0.5, 0.05, true_sigma);
        let recovered = implied_vol(price, 100.0, 105.0, 0.5, 0.05, false).unwrap();
        assert!(
            (recovered - true_sigma).abs() < 1e-6,
            "Put IV recovery: expected {}, got {}",
            true_sigma,
            recovered
        );
    }

    #[test]
    fn test_implied_vol_zero_price() {
        assert!(implied_vol(0.0, 100.0, 100.0, 0.25, 0.05, true).is_none());
    }

    #[test]
    fn test_implied_vol_below_intrinsic() {
        // Market price below intrinsic (arbitrage) → None
        // S=110, K=100, intrinsic call = 10. Market price = 5 → below intrinsic.
        assert!(implied_vol(5.0, 110.0, 100.0, 0.25, 0.05, true).is_none());
    }

    #[test]
    fn test_implied_vol_0dte_atm() {
        // 0DTE ATM: S=190, K=190, T=2h=120min, premium=$1.50
        let t = minutes_to_years(120.0);
        let price = 1.50;
        let iv = implied_vol(price, 190.0, 190.0, t, 0.05, true);
        assert!(iv.is_some(), "Should find IV for 0DTE ATM");
        let sigma = iv.unwrap();
        assert!(sigma > 0.10 && sigma < 5.0, "0DTE IV={} seems unreasonable", sigma);
    }

    #[test]
    fn test_minutes_to_years() {
        let t = minutes_to_years(390.0);
        assert!(
            (t - 1.0 / 252.0).abs() < 1e-10,
            "390 min = 1 trading day = 1/252 years, got {}",
            t
        );
    }

    #[test]
    fn test_minutes_to_years_clamped() {
        let t = minutes_to_years(0.0);
        assert!(t >= MIN_T, "Should clamp to MIN_T");
    }

    #[test]
    fn test_near_expiry_no_panic() {
        let t = MIN_T;
        let _ = call_price(190.0, 190.0, t, 0.05, 0.30);
        let _ = put_price(190.0, 190.0, t, 0.05, 0.30);
        let _ = delta(190.0, 190.0, t, 0.05, 0.30, true);
        let _ = gamma(190.0, 190.0, t, 0.05, 0.30);
        let _ = theta(190.0, 190.0, t, 0.05, 0.30, true);
        let _ = vega(190.0, 190.0, t, 0.05, 0.30);
    }
}
