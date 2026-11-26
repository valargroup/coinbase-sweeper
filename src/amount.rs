//! Exact ZEC amounts. Every amount is held in zatoshis (1 ZEC = 100,000,000 zatoshis) and
//! converted to and from decimal text without floating point.

use std::fmt;

use anyhow::{bail, Context, Result};
use serde_json::Value;

pub const ZAT_PER_ZEC: u64 = 100_000_000;
/// The total ZEC supply cap. No real balance or transfer can exceed it.
pub const MAX_MONEY: u64 = 21_000_000 * ZAT_PER_ZEC;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Zat(pub u64);

impl Zat {
    pub const ZERO: Zat = Zat(0);

    /// Parses a non-negative decimal ZEC amount with at most 8 fractional digits.
    pub fn parse_zec(text: &str) -> Result<Zat> {
        let text = text.trim();
        let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
        // Trailing zeros carry no value: 3.1250000000 is 3.125.
        let frac = frac.trim_end_matches('0');
        if whole.is_empty() && frac.is_empty() {
            bail!("empty amount");
        }
        if !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
            bail!("amount {text:?} is not a non-negative decimal number");
        }
        if frac.len() > 8 {
            bail!("amount {text:?} has more than 8 decimal places");
        }
        let whole: u64 = if whole.is_empty() {
            0
        } else {
            whole
                .parse()
                .with_context(|| format!("amount {text:?} is too large"))?
        };
        let frac_zat: u64 = if frac.is_empty() {
            0
        } else {
            format!("{frac:0<8}")
                .parse()
                .expect("8 ASCII digits fit u64")
        };
        let zat = whole
            .checked_mul(ZAT_PER_ZEC)
            .and_then(|w| w.checked_add(frac_zat))
            .filter(|z| *z <= MAX_MONEY)
            .with_context(|| format!("amount {text:?} exceeds the 21 million ZEC supply"))?;
        Ok(Zat(zat))
    }

    /// Reads an amount from a JSON number (as wallets return it) without going through f64.
    pub fn from_json(v: &Value) -> Result<Zat> {
        match v {
            Value::Number(n) => {
                let text = n.to_string();
                Zat::parse_zec(&expand_exponent(&text)?)
            }
            other => bail!("expected an amount, got {other}"),
        }
    }

    /// The amount as a JSON number with exactly 8 decimal places, for RPC parameters.
    pub fn to_json(self) -> Value {
        serde_json::from_str(&self.to_string()).expect("a decimal amount is valid JSON")
    }

    pub fn saturating_sub(self, other: Zat) -> Zat {
        Zat(self.0.saturating_sub(other.0))
    }

    pub fn checked_add(self, other: Zat) -> Option<Zat> {
        self.0.checked_add(other.0).map(Zat)
    }
}

/// Rewrites `1.5e-3` as `0.0015` exactly, so amounts in scientific notation parse without f64.
fn expand_exponent(text: &str) -> Result<String> {
    let Some((mantissa, exp)) = text.split_once(['e', 'E']) else {
        return Ok(text.to_owned());
    };
    let exp: i32 = exp
        .trim_start_matches('+')
        .parse()
        .with_context(|| format!("bad exponent in {text}"))?;
    if !(-30..=30).contains(&exp) {
        bail!("amount {text} is out of range");
    }
    let (whole, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{frac}");
    // Position of the decimal point within `digits` after shifting.
    let point = whole.len() as i32 + exp;
    Ok(if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    })
}

impl fmt::Display for Zat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:08}", self.0 / ZAT_PER_ZEC, self.0 % ZAT_PER_ZEC)
    }
}

impl std::iter::Sum for Zat {
    fn sum<I: Iterator<Item = Zat>>(iter: I) -> Zat {
        // Summing more than the supply cap is impossible for real wallet data; saturate.
        Zat(iter.fold(0u64, |acc, z| acc.saturating_add(z.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_prints_exactly() {
        assert_eq!(Zat::parse_zec("6.25").unwrap(), Zat(625_000_000));
        assert_eq!(Zat::parse_zec("0.00000001").unwrap(), Zat(1));
        assert_eq!(Zat::parse_zec("3").unwrap(), Zat(300_000_000));
        assert_eq!(Zat::parse_zec(".5").unwrap(), Zat(50_000_000));
        assert_eq!(Zat::parse_zec("3.1250000000").unwrap(), Zat(312_500_000));
        assert_eq!(Zat::parse_zec("0.").unwrap(), Zat(0));
        assert_eq!(Zat(123_456_789).to_string(), "1.23456789");
        assert_eq!(Zat(1).to_json().to_string(), "0.00000001");
        assert_eq!(Zat::from_json(&Zat(1).to_json()).unwrap(), Zat(1));
        let sci = |t: &str| Zat::from_json(&serde_json::from_str::<Value>(t).unwrap()).unwrap();
        assert_eq!(sci("1e-8"), Zat(1));
        assert_eq!(sci("1.0e-8"), Zat(1));
        assert_eq!(sci("1.5E-3"), Zat(150_000));
        assert_eq!(sci("2.5e1"), Zat(2_500_000_000));
    }

    #[test]
    fn rejects_bad_amounts() {
        for bad in ["-1", "1.000000001", "abc", "", "1e3", "21000000.00000001"] {
            assert!(Zat::parse_zec(bad).is_err(), "{bad}");
        }
        assert!(Zat::from_json(&json!("1.0")).is_err());
        assert!(Zat::from_json(&serde_json::from_str::<Value>("1e-9").unwrap()).is_err());
        assert!(Zat::from_json(&serde_json::from_str::<Value>("-1").unwrap()).is_err());
    }
}
