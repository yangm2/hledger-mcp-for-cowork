//! Exact monetary amounts decoded from hledger's `-O json` output.
//!
//! hledger encodes every quantity as `{ decimalMantissa, decimalPlaces, floatingPoint }`.
//! This is a **money** ledger, so we decode the exact integer **`decimalMantissa` /
//! 10^`decimalPlaces`** and *never* touch `floatingPoint` (a lossy IEEE-754 view that would
//! turn `0.10` into `0.1000000000000000055…`). [`Quantity::render`] reconstructs the decimal
//! string by integer math alone — no float ever participates.

use std::fmt;

/// An exact decimal quantity: `mantissa × 10^-places`.
///
/// Maps hledger's `aquantity.decimalMantissa` (→ [`mantissa`](Self::mantissa)) and
/// `aquantity.decimalPlaces` (→ [`places`](Self::places)). `floatingPoint` is deliberately
/// not represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantity {
    /// The unscaled integer value (signed). `8766` with `places = 2` is `87.66`.
    pub mantissa: i128,
    /// Number of fractional decimal places to scale [`mantissa`](Self::mantissa) by.
    pub places: u32,
}

impl Quantity {
    /// Construct a quantity from its mantissa and decimal-place count.
    pub fn new(mantissa: i128, places: u32) -> Self {
        Self { mantissa, places }
    }

    /// Render the exact decimal as a string, by integer math only (no float).
    ///
    /// Examples: `(8766, 2) → "87.66"`, `(-15000, 2) → "-150.00"`, `(5, 2) → "0.05"`,
    /// `(0, 2) → "0.00"`, `(42, 0) → "42"`.
    pub fn render(&self) -> String {
        let places = self.places as usize;
        // Work in the unsigned magnitude, then re-apply the sign, so digit-slicing never has
        // to reason about a leading '-'.
        let digits = self.mantissa.unsigned_abs().to_string();
        let body = if places == 0 {
            digits
        } else if digits.len() <= places {
            // Pure fraction: pad with leading zeros after "0." (e.g. 5 places=2 -> "0.05").
            format!("0.{:0>width$}", digits, width = places)
        } else {
            let point = digits.len() - places;
            format!("{}.{}", &digits[..point], &digits[point..])
        };
        if self.mantissa < 0 {
            format!("-{body}")
        } else {
            body
        }
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// A commodity-tagged amount, e.g. `$87.66` or `40.00 EUR`.
///
/// Carries just enough of hledger's `astyle` (`ascommodityside`, `ascommodityspaced`) to
/// render the amount on the side the ledger uses; the rest of `astyle` is ignored (the
/// canonical write-side formatter is M2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amount {
    /// The commodity symbol (hledger `acommodity`), e.g. `"$"` or `"EUR"`.
    pub commodity: String,
    /// The exact quantity.
    pub quantity: Quantity,
    /// `true` when the commodity prints to the left of the number (`astyle.ascommodityside`
    /// `"L"`, the `$100.00` style); `false` prints it on the right (`100.00 EUR`).
    pub commodity_left: bool,
    /// Whether a space separates commodity and number (`astyle.ascommodityspaced`).
    pub spaced: bool,
}

impl Amount {
    /// Render the amount with its commodity on the configured side, e.g. `"$87.66"` or
    /// `"40.00 EUR"`.
    pub fn render(&self) -> String {
        let q = self.quantity.render();
        let sep = if self.spaced { " " } else { "" };
        if self.commodity_left {
            format!("{}{sep}{q}", self.commodity)
        } else {
            format!("{q}{sep}{}", self.commodity)
        }
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// Render a list of amounts (a single posting/balance cell can hold several commodities)
/// as a comma-separated string, e.g. `"$10.00, 5.00 EUR"`. An empty list renders as `"0"`,
/// matching hledger's display of a zero balance.
pub fn render_amounts(amounts: &[Amount]) -> String {
    if amounts.is_empty() {
        return "0".to_string();
    }
    amounts
        .iter()
        .map(Amount::render)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_whole_and_fractional() {
        assert_eq!(Quantity::new(8766, 2).render(), "87.66");
        assert_eq!(Quantity::new(42, 0).render(), "42");
        assert_eq!(Quantity::new(5000, 2).render(), "50.00");
    }

    #[test]
    fn renders_negative() {
        assert_eq!(Quantity::new(-15000, 2).render(), "-150.00");
        assert_eq!(Quantity::new(-1, 2).render(), "-0.01");
    }

    #[test]
    fn renders_pure_fraction_with_leading_zeros() {
        assert_eq!(Quantity::new(5, 2).render(), "0.05");
        assert_eq!(Quantity::new(7, 3).render(), "0.007");
        assert_eq!(Quantity::new(0, 2).render(), "0.00");
        assert_eq!(Quantity::new(0, 0).render(), "0");
    }

    #[test]
    fn amount_renders_on_configured_side() {
        let left = Amount {
            commodity: "$".into(),
            quantity: Quantity::new(8766, 2),
            commodity_left: true,
            spaced: false,
        };
        assert_eq!(left.render(), "$87.66");
        let right = Amount {
            commodity: "EUR".into(),
            quantity: Quantity::new(4000, 2),
            commodity_left: false,
            spaced: true,
        };
        assert_eq!(right.render(), "40.00 EUR");
    }

    #[test]
    fn display_matches_render() {
        let q = Quantity::new(8766, 2);
        assert_eq!(format!("{q}"), q.render());
        let a = Amount {
            commodity: "$".into(),
            quantity: q,
            commodity_left: true,
            spaced: false,
        };
        assert_eq!(format!("{a}"), a.render());
    }

    #[test]
    fn render_amounts_joins_and_handles_empty() {
        assert_eq!(render_amounts(&[]), "0");
        let amts = vec![
            Amount {
                commodity: "$".into(),
                quantity: Quantity::new(1000, 2),
                commodity_left: true,
                spaced: false,
            },
            Amount {
                commodity: "EUR".into(),
                quantity: Quantity::new(500, 2),
                commodity_left: false,
                spaced: true,
            },
        ];
        assert_eq!(render_amounts(&amts), "$10.00, 5.00 EUR");
    }
}
