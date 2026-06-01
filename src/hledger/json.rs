//! Deserialization of `hledger … -O json` output (the read half of the §6 round-trip).
//!
//! **Parse only the fields we use; ignore unknowns.** None of these structs use
//! `#[serde(deny_unknown_fields)]`, so an hledger version that renames or adds a field (the
//! `ptype`→`preal` kind of change) only breaks us if it touches a field we actually read —
//! and that fix lands in this one file. The wire structs (`Json*`) are private; the public
//! surface is the domain types in [`super`], produced by the `From`/`from_*` conversions here.

use serde::Deserialize;

use super::amount::{Amount, Quantity};
use super::{AccountBalance, BalanceReport, Posting, Transaction};

/// hledger `aquantity`: an exact decimal. We read the integer mantissa + place count and
/// drop `floatingPoint` (lossy — see [`super::amount`]).
#[derive(Debug, Deserialize)]
struct JsonQuantity {
    #[serde(rename = "decimalMantissa")]
    mantissa: i128,
    #[serde(rename = "decimalPlaces")]
    places: u32,
}

/// hledger `astyle`: we keep only the commodity placement needed to render the amount.
#[derive(Debug, Deserialize)]
struct JsonStyle {
    /// `"L"` (left, `$1`) or `"R"` (right, `1 EUR`).
    ascommodityside: String,
    ascommodityspaced: bool,
}

/// hledger `Amount` object (in `pamount`, balance cells, register columns).
#[derive(Debug, Deserialize)]
struct JsonAmount {
    acommodity: String,
    aquantity: JsonQuantity,
    astyle: JsonStyle,
}

impl From<JsonAmount> for Amount {
    fn from(a: JsonAmount) -> Self {
        Amount {
            commodity: a.acommodity,
            quantity: Quantity::new(a.aquantity.mantissa, a.aquantity.places),
            commodity_left: a.astyle.ascommodityside == "L",
            spaced: a.astyle.ascommodityspaced,
        }
    }
}

fn into_amounts(raw: Vec<JsonAmount>) -> Vec<Amount> {
    raw.into_iter().map(Amount::from).collect()
}

// ---- balance (`hledger balance -O json`) ----------------------------------------------
//
// Shape: a 2-tuple `[rows, totals]`. Each row is the positional tuple
// `[full_account_name, display_name, indent, [amounts]]`; `totals` is `[amounts]`.

/// One balance row: `(full account name, display name, indent depth, amounts)`.
type JsonBalanceRow = (String, String, i64, Vec<JsonAmount>);
/// The whole `balance` document: `(rows, column totals)`.
type JsonBalance = (Vec<JsonBalanceRow>, Vec<JsonAmount>);

/// Parse `hledger balance -O json` output into a [`BalanceReport`].
pub fn parse_balance(stdout: &str) -> Result<BalanceReport, serde_json::Error> {
    let (rows, totals): JsonBalance = serde_json::from_str(stdout)?;
    Ok(BalanceReport {
        rows: rows
            .into_iter()
            .map(|(account, _display, _indent, amounts)| AccountBalance {
                account,
                amounts: into_amounts(amounts),
            })
            .collect(),
        totals: into_amounts(totals),
    })
}

// ---- print (`hledger print -O json`) --------------------------------------------------

#[derive(Debug, Deserialize)]
struct JsonPosting {
    paccount: String,
    pamount: Vec<JsonAmount>,
    #[serde(default)]
    pcomment: String,
    #[serde(default)]
    ptags: Vec<(String, String)>,
}

impl From<JsonPosting> for Posting {
    fn from(p: JsonPosting) -> Self {
        Posting {
            account: p.paccount,
            amounts: into_amounts(p.pamount),
            comment: p.pcomment,
            tags: p.ptags,
        }
    }
}

#[derive(Debug, Deserialize)]
struct JsonTransaction {
    tdate: String,
    tdescription: String,
    tindex: i64,
    #[serde(default)]
    tstatus: String,
    #[serde(default)]
    tcomment: String,
    #[serde(default)]
    ttags: Vec<(String, String)>,
    tpostings: Vec<JsonPosting>,
}

impl From<JsonTransaction> for Transaction {
    fn from(t: JsonTransaction) -> Self {
        Transaction {
            date: t.tdate,
            description: t.tdescription,
            index: t.tindex,
            status: t.tstatus,
            comment: t.tcomment,
            tags: t.ttags,
            postings: t.tpostings.into_iter().map(Posting::from).collect(),
        }
    }
}

/// Parse `hledger print -O json` output into a list of [`Transaction`]s.
pub fn parse_print(stdout: &str) -> Result<Vec<Transaction>, serde_json::Error> {
    let raw: Vec<JsonTransaction> = serde_json::from_str(stdout)?;
    Ok(raw.into_iter().map(Transaction::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden fixtures recorded from the pinned hledger 1.52 (`mise run golden`). These prove
    // the parser tracks the *real* wire shape, not a guess.
    const BALANCE_SINGLE: &str = include_str!("../../tests/fixtures/balance_single.json");
    const BALANCE_ALL: &str = include_str!("../../tests/fixtures/balance_all.json");
    const PRINT_BASIC: &str = include_str!("../../tests/fixtures/print_basic.json");

    #[test]
    fn parses_single_account_balance() {
        // assets:checking in sample.journal = $100.00 − $12.34 − $44.00 = $43.66.
        let report = parse_balance(BALANCE_SINGLE).expect("parse balance_single");
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].account, "assets:checking");
        assert_eq!(report.rows[0].amounts[0].render(), "$43.66");
        assert_eq!(report.totals[0].render(), "$43.66");
    }

    #[test]
    fn parses_multi_account_balance_and_commodities() {
        let report = parse_balance(BALANCE_ALL).expect("parse balance_all");
        let names: Vec<&str> = report.rows.iter().map(|r| r.account.as_str()).collect();
        assert!(names.contains(&"assets:savings account"), "{names:?}");
        assert!(names.contains(&"expenses:travel"), "{names:?}");
        // The EUR row renders commodity-on-right (astyle side R), proving per-amount styling.
        let travel = report
            .rows
            .iter()
            .find(|r| r.account == "expenses:travel")
            .expect("travel row");
        assert_eq!(travel.amounts[0].render(), "40.00 EUR");
    }

    #[test]
    fn parses_print_transactions_postings_and_tags() {
        let txns = parse_print(PRINT_BASIC).expect("parse print_basic");
        assert_eq!(txns.len(), 3);
        let acme = &txns[1];
        assert_eq!(acme.date, "2026-01-15");
        assert_eq!(acme.description, "Acme");
        assert_eq!(acme.postings.len(), 2);
        assert_eq!(acme.postings[0].account, "expenses:supplies");
        assert_eq!(acme.postings[0].amounts[0].render(), "$12.34");
        // Tags survive as (key, value) pairs.
        assert!(
            acme.tags.iter().any(|(k, v)| k == "vendor" && v == "Acme"),
            "tags: {:?}",
            acme.tags
        );
    }

    #[test]
    fn ignores_unknown_fields() {
        // A future hledger adds a field we don't read; we must still parse. Inject a bogus
        // top-level key into an amount object and a transaction.
        let doc = r#"[
          {"tdate":"2026-01-01","tdescription":"x","tindex":1,"tstatus":"Unmarked",
           "ttags":[],"tpostings":[
             {"paccount":"a:b","ptags":[],"pamount":[
               {"acommodity":"$","aquantity":{"decimalMantissa":100,"decimalPlaces":2,
                 "floatingPoint":1.0},"astyle":{"ascommodityside":"L",
                 "ascommodityspaced":false},"future_field":42}]}],
           "future_field":"ignored"}]"#;
        let txns = parse_print(doc).expect("unknown fields ignored");
        assert_eq!(txns[0].postings[0].amounts[0].render(), "$1.00");
    }

    #[test]
    fn parse_error_on_malformed_json() {
        assert!(parse_balance("not json").is_err());
        assert!(parse_print("{}").is_err());
    }

    // Round-trip property tests (the read half of the §6 round-trip contract): synthesize the
    // hledger amount JSON for arbitrary values, parse it through the real parser, and assert
    // the decoded amount matches — plus that exact-decimal rendering is lossless.
    use proptest::prelude::*;

    /// Build a one-row balance document embedding `amount_json` (so we exercise the real
    /// `parse_balance` path rather than a private struct).
    fn balance_doc(amount_json: &str) -> String {
        format!(r#"[[["a:b","a:b",0,[{amount_json}]]],[{amount_json}]]"#)
    }

    proptest! {
        #[test]
        fn quantity_render_is_lossless(mantissa in any::<i64>(), places in 0u32..=8) {
            let rendered = Quantity::new(mantissa as i128, places).render();
            // The rendered decimal must reconstruct the exact mantissa (no float drift).
            let digits: String = rendered.chars().filter(char::is_ascii_digit).collect();
            let mut value: i128 = digits.parse().expect("digits parse");
            if rendered.starts_with('-') {
                value = -value;
            }
            prop_assert_eq!(value, i128::from(mantissa));
            // And the fractional-digit count matches `places` exactly.
            if places == 0 {
                prop_assert!(!rendered.contains('.'));
            } else {
                let frac = rendered.split('.').nth(1).expect("fractional part");
                prop_assert_eq!(frac.len() as u32, places);
            }
        }

        #[test]
        fn amount_json_round_trips_through_parser(
            mantissa in any::<i64>(),
            places in 0u32..=6,
            commodity in "[A-Z$]{1,3}",
            left in any::<bool>(),
            spaced in any::<bool>(),
        ) {
            let side = if left { "L" } else { "R" };
            let amount_json = format!(
                r#"{{"acommodity":"{commodity}","acost":null,"acostbasis":null,"aquantity":{{"decimalMantissa":{mantissa},"decimalPlaces":{places},"floatingPoint":0}},"astyle":{{"ascommodityside":"{side}","ascommodityspaced":{spaced},"asprecision":{places}}}}}"#
            );
            let report = parse_balance(&balance_doc(&amount_json)).expect("parse generated doc");
            let parsed = &report.rows[0].amounts[0];
            prop_assert_eq!(&parsed.commodity, &commodity);
            prop_assert_eq!(parsed.quantity.mantissa, i128::from(mantissa));
            prop_assert_eq!(parsed.quantity.places, places);
            prop_assert_eq!(parsed.commodity_left, left);
            prop_assert_eq!(parsed.spaced, spaced);
        }
    }
}
