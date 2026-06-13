//! Deserialization of `hledger … -O json` output (the read half of the §6 round-trip).
//!
//! **Parse only the fields we use; ignore unknowns.** None of these structs use
//! `#[serde(deny_unknown_fields)]`, so an hledger version that renames or adds a field (the
//! `ptype`→`preal` kind of change) only breaks us if it touches a field we actually read —
//! and that fix lands in this one file. The wire structs (`Json*`) are private; the public
//! surface is the domain types in [`super`], produced by the `From`/`from_*` conversions here.

use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::Value;

use super::amount::{Amount, Quantity};
use super::{
    AccountBalance, BalanceReport, BudgetReport, BudgetRow, CompositeReport, Posting, ReportRow,
    Status, Subreport, Transaction,
};

/// hledger `aquantity`: an exact decimal. We read the integer mantissa + place count and
/// drop `floatingPoint` (lossy — see [`super::amount`]).
#[derive(Debug, Deserialize)]
struct JsonQuantity {
    #[serde(rename = "decimalMantissa")]
    mantissa: i128,
    #[serde(rename = "decimalPlaces")]
    places: u8,
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
            commodity: a.acommodity.into(),
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
// Shape: a 2-element array `[rows, totals]`. Each row is the positional array
// `[full_account_name, display_name, indent, [amounts]]`; `totals` is `[amounts]`.
//
// We navigate this as `serde_json::Value` by index rather than deserializing fixed-length
// tuples: a tuple rejects *extra* trailing elements, which would break the moment a future
// hledger appends a positional field — violating this module's parse-only-what-we-use /
// ignore-unknowns invariant. Indexing reads the positions we use (0 + 3 of a row, 0 + 1 of
// the document) and ignores anything beyond, so an additive shape change stays compatible.

/// Read a JSON value expected to be an array of hledger amounts into domain [`Amount`]s.
fn amounts_from_value(value: &Value) -> Result<Vec<Amount>, serde_json::Error> {
    let raw: Vec<JsonAmount> = serde_json::from_value(value.clone())?;
    Ok(into_amounts(raw))
}

/// Parse `hledger balance -O json` output into a [`BalanceReport`].
pub fn parse_balance(stdout: &str) -> Result<BalanceReport, serde_json::Error> {
    use serde::de::Error as _;
    let doc: Value = serde_json::from_str(stdout)?;
    let top = doc
        .as_array()
        .ok_or_else(|| serde_json::Error::custom("balance: expected a top-level array"))?;
    let rows_val = top
        .first()
        .ok_or_else(|| serde_json::Error::custom("balance: missing rows element"))?;
    let totals_val = top
        .get(1)
        .ok_or_else(|| serde_json::Error::custom("balance: missing totals element"))?;
    let rows_arr = rows_val
        .as_array()
        .ok_or_else(|| serde_json::Error::custom("balance: rows is not an array"))?;

    let mut rows = Vec::with_capacity(rows_arr.len());
    for row in rows_arr {
        let cells = row
            .as_array()
            .ok_or_else(|| serde_json::Error::custom("balance: row is not an array"))?;
        let account = cells
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| serde_json::Error::custom("balance: row missing account name"))?
            .to_string();
        let amounts_val = cells
            .get(3)
            .ok_or_else(|| serde_json::Error::custom("balance: row missing amounts"))?;
        rows.push(AccountBalance {
            account,
            amounts: amounts_from_value(amounts_val)?,
        });
    }
    Ok(BalanceReport {
        rows,
        totals: amounts_from_value(totals_val)?,
    })
}

// ---- balancesheet / incomestatement (`hledger balancesheet/incomestatement -O json`) ----
//
// Both commands produce the same "composite balance report" (cbr) shape:
// {
//   "cbrTitle": "...",
//   "cbrSubreports": [
//     ["Assets", { "prRows": [...], "prTotals": { "prrTotal": [...] } }, true],
//     ...
//   ],
//   "cbrTotals": { "prrTotal": [...] }
// }
//
// cbrSubreports is a positional 3-tuple; we index by position for the same reason we
// index balance rows — a future hledger appending a field stays compatible.
//
// prRows entries have "prrName" as a string (account name). The prTotals entry has
// "prrName": [] (empty array) — we never parse prrName from prTotals, so this doesn't bite.

/// Parse `hledger balancesheet -O json` or `hledger incomestatement -O json` output into
/// a [`CompositeReport`]. Both commands produce the same `cbr` wire shape.
pub fn parse_composite_report(stdout: &str) -> Result<CompositeReport, serde_json::Error> {
    use serde::de::Error as _;
    let doc: Value = serde_json::from_str(stdout)?;

    let title = doc["cbrTitle"]
        .as_str()
        .ok_or_else(|| serde_json::Error::custom("composite: missing cbrTitle string"))?
        .to_string();

    let subreports_arr = doc["cbrSubreports"]
        .as_array()
        .ok_or_else(|| serde_json::Error::custom("composite: cbrSubreports not array"))?;

    let mut subreports = Vec::with_capacity(subreports_arr.len());
    for entry in subreports_arr {
        let tuple = entry
            .as_array()
            .ok_or_else(|| serde_json::Error::custom("composite: subreport entry not array"))?;
        let name = tuple
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| serde_json::Error::custom("composite: subreport missing name"))?
            .to_string();
        let pr = tuple.get(1).ok_or_else(|| {
            serde_json::Error::custom("composite: subreport missing PeriodicReport")
        })?;
        let is_positive = tuple
            .get(2)
            .and_then(Value::as_bool)
            .ok_or_else(|| serde_json::Error::custom("composite: subreport missing isPositive"))?;

        let rows_arr = pr["prRows"]
            .as_array()
            .ok_or_else(|| serde_json::Error::custom("composite: prRows not array"))?;
        let mut rows = Vec::with_capacity(rows_arr.len());
        for row in rows_arr {
            let account = row["prrName"]
                .as_str()
                .ok_or_else(|| serde_json::Error::custom("composite: prrName not string"))?
                .to_string();
            let total = amounts_from_value(&row["prrTotal"])?;
            rows.push(ReportRow { account, total });
        }

        let totals = amounts_from_value(&pr["prTotals"]["prrTotal"])?;
        subreports.push(Subreport {
            name,
            rows,
            totals,
            is_positive,
        });
    }

    let totals = amounts_from_value(&doc["cbrTotals"]["prrTotal"])?;
    Ok(CompositeReport {
        title,
        subreports,
        totals,
    })
}

// ---- balance --budget (`hledger balance --budget -M -O json`) --------------------------
//
// Budget mode emits a bare PeriodicReport `{ "prRows": [...], "prTotals": {...} }` where
// every amounts cell is the positional pair `[actual_amounts, goal_amounts]` (goal absent
// or null for unbudgeted rows). Same index-navigation rationale as `parse_balance`:
// positions we use are read, anything appended later is ignored.

/// Read one `[actual, goal]` budget cell. A missing/null goal (unbudgeted row) is empty.
fn budget_pair(cell: &Value) -> Result<(Vec<Amount>, Vec<Amount>), serde_json::Error> {
    use serde::de::Error as _;
    let pair = cell
        .as_array()
        .ok_or_else(|| serde_json::Error::custom("budget: cell is not an [actual, goal] pair"))?;
    let actual = match pair.first() {
        Some(v) if !v.is_null() => amounts_from_value(v)?,
        _ => Vec::new(),
    };
    let goal = match pair.get(1) {
        Some(v) if !v.is_null() => amounts_from_value(v)?,
        _ => Vec::new(),
    };
    Ok((actual, goal))
}

/// Parse `hledger balance --budget -M -O json` output into a [`BudgetReport`].
pub fn parse_budget(stdout: &str) -> Result<BudgetReport, serde_json::Error> {
    use serde::de::Error as _;
    let doc: Value = serde_json::from_str(stdout)?;
    let rows_arr = doc["prRows"]
        .as_array()
        .ok_or_else(|| serde_json::Error::custom("budget: prRows not array"))?;
    let mut rows = Vec::with_capacity(rows_arr.len());
    for row in rows_arr {
        let account = row["prrName"]
            .as_str()
            .ok_or_else(|| serde_json::Error::custom("budget: prrName not string"))?
            .to_string();
        let (actual, goal) = budget_pair(&row["prrTotal"])?;
        rows.push(BudgetRow {
            account,
            actual,
            goal,
        });
    }
    let (total_actual, total_goal) = budget_pair(&doc["prTotals"]["prrTotal"])?;
    Ok(BudgetReport {
        rows,
        total_actual,
        total_goal,
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
    tdate: NaiveDate,
    tdescription: String,
    tindex: u64,
    #[serde(default)]
    tstatus: Status,
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

    // Golden fixtures recorded from the pinned hledger 1.52. These prove the parser tracks
    // the *real* wire shape, not a guess.
    const BALANCE_SINGLE: &str = include_str!("../../tests/fixtures/balance_single.json");
    const BALANCE_ALL: &str = include_str!("../../tests/fixtures/balance_all.json");
    const PRINT_BASIC: &str = include_str!("../../tests/fixtures/print_basic.json");
    const BALANCESHEET_BASIC: &str = include_str!("../../tests/fixtures/balancesheet_basic.json");
    const INCOMESTATEMENT_BASIC: &str =
        include_str!("../../tests/fixtures/incomestatement_basic.json");
    const BUDGET_BASIC: &str = include_str!("../../tests/fixtures/budget_basic.json");

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
        assert_eq!(acme.date, NaiveDate::from_ymd_opt(2026, 1, 15).unwrap());
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
    fn ignores_extra_balance_array_elements() {
        // A future hledger appends positional elements to a row and to the top-level array.
        // Indexing (not fixed-length tuples) must ignore the extras and still parse.
        let doc = r#"[
          [["assets:checking","assets:checking",0,
            [{"acommodity":"$","aquantity":{"decimalMantissa":4366,"decimalPlaces":2,
              "floatingPoint":43.66},"astyle":{"ascommodityside":"L","ascommodityspaced":false}}],
            "FUTURE_ROW_FIELD", 99]],
          [{"acommodity":"$","aquantity":{"decimalMantissa":4366,"decimalPlaces":2,
            "floatingPoint":43.66},"astyle":{"ascommodityside":"L","ascommodityspaced":false}}],
          "FUTURE_TOP_FIELD"]"#;
        let report = parse_balance(doc).expect("extra array elements ignored");
        assert_eq!(report.rows[0].account, "assets:checking");
        assert_eq!(report.rows[0].amounts[0].render(), "$43.66");
        assert_eq!(report.totals[0].render(), "$43.66");
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

    #[test]
    fn parses_balancesheet_golden() {
        // domain.journal: fund ($50k) - pay Acme ($8k) + interest ($125) = $42,125 checking.
        // Acme paid; Bob Engineer still owes $2,500 AP. Net equity = $39,625.
        let report = parse_composite_report(BALANCESHEET_BASIC).expect("parse balancesheet");
        assert_eq!(report.title, "Balance Sheet 2026-03-01");
        assert_eq!(report.subreports.len(), 2);

        let assets = &report.subreports[0];
        assert_eq!(assets.name, "Assets");
        assert!(assets.is_positive);
        assert_eq!(assets.rows.len(), 1);
        assert_eq!(assets.rows[0].account, "assets:checking");
        assert_eq!(assets.rows[0].total[0].render(), "$42125.00");
        assert_eq!(assets.totals[0].render(), "$42125.00");

        let liabilities = &report.subreports[1];
        assert_eq!(liabilities.name, "Liabilities");
        assert!(!liabilities.is_positive);
        assert_eq!(liabilities.rows.len(), 1);
        assert_eq!(
            liabilities.rows[0].account,
            "liabilities:ap:vendor:Bob Engineer"
        );
        assert_eq!(liabilities.rows[0].total[0].render(), "$2500.00");

        assert_eq!(report.totals[0].render(), "$39625.00");
    }

    #[test]
    fn parses_budget_golden() {
        // budget fixture: $300 actual against a $500 monthly goal on the plumbing account.
        let report = parse_budget(BUDGET_BASIC).expect("parse budget_basic");
        assert_eq!(report.rows.len(), 1);
        let row = &report.rows[0];
        assert_eq!(row.account, "expenses:construction:plumbing");
        assert_eq!(row.actual[0].render(), "300.00 $");
        assert_eq!(row.goal[0].render(), "500.00 $");
        assert_eq!(report.total_actual[0].render(), "300.00 $");
        assert_eq!(report.total_goal[0].render(), "500.00 $");
    }

    #[test]
    fn budget_cell_tolerates_missing_or_null_goal() {
        // An unbudgeted row's goal cell may be null (or absent in a future shape) — empty, not
        // an error (parse-only-what-we-use tolerance for the additive/optional case).
        let (actual, goal) = budget_pair(&serde_json::json!([[], null])).expect("null goal");
        assert!(actual.is_empty() && goal.is_empty());
        // A null ACTUAL cell must also read as empty, not be fed to the amounts parser.
        let (actual, goal) = budget_pair(&serde_json::json!([null, []])).expect("null actual");
        assert!(actual.is_empty() && goal.is_empty());
        let (_, goal) = budget_pair(&serde_json::json!([[]])).expect("absent goal");
        assert!(goal.is_empty());
        assert!(budget_pair(&serde_json::json!("x")).is_err(), "non-array");
    }

    #[test]
    fn parses_incomestatement_golden() {
        // domain.journal: $125 interest income, $8000 plumbing + $2500 professional expenses.
        let report = parse_composite_report(INCOMESTATEMENT_BASIC).expect("parse incomestatement");
        assert!(
            report.title.starts_with("Income Statement"),
            "title: {}",
            report.title
        );
        assert_eq!(report.subreports.len(), 2);

        let revenues = &report.subreports[0];
        assert_eq!(revenues.name, "Revenues");
        assert!(revenues.is_positive);
        assert_eq!(revenues.rows.len(), 1);
        assert_eq!(revenues.rows[0].account, "income:interest");
        assert_eq!(revenues.rows[0].total[0].render(), "$125.00");

        let expenses = &report.subreports[1];
        assert_eq!(expenses.name, "Expenses");
        assert!(!expenses.is_positive);
        assert_eq!(expenses.rows.len(), 2);
        let accounts: Vec<&str> = expenses.rows.iter().map(|r| r.account.as_str()).collect();
        assert!(
            accounts.contains(&"expenses:construction:plumbing"),
            "{accounts:?}"
        );
        assert!(
            accounts.contains(&"expenses:professional - Bob Engineer"),
            "{accounts:?}"
        );
    }

    #[test]
    fn composite_ignores_extra_subreport_tuple_elements() {
        // A future hledger appends a 4th element to the subreport tuple — must still parse.
        let doc = r#"{
          "cbrTitle":"T","cbrTotals":{"prrTotal":[]},"cbrSubreports":[
            ["Assets",{"prRows":[],"prTotals":{"prrTotal":[]},"prDates":[]},true,"FUTURE"]
          ]}"#;
        let report = parse_composite_report(doc).expect("extra tuple element ignored");
        assert_eq!(report.subreports[0].name, "Assets");
    }

    #[test]
    fn composite_rejects_missing_fields() {
        assert!(
            parse_composite_report(r#"{"cbrSubreports":[],"cbrTotals":{"prrTotal":[]}}"#).is_err()
        );
        assert!(parse_composite_report(r#"{"cbrTitle":"T","cbrTotals":{"prrTotal":[]}}"#).is_err());
    }

    #[test]
    fn balance_rejects_wrong_shapes() {
        // Not a top-level array.
        assert!(parse_balance(r#"{"rows":[]}"#).is_err());
        // Missing the totals element (only one top element).
        assert!(parse_balance(r#"[[]]"#).is_err());
        // Row isn't an array.
        assert!(parse_balance(r#"[[42],[]]"#).is_err());
        // Row missing the account-name cell (empty row).
        assert!(parse_balance(r#"[[[]],[]]"#).is_err());
        // Row present with account but missing the amounts cell (index 3).
        assert!(parse_balance(r#"[[["a:b","a:b",0]],[]]"#).is_err());
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
        fn quantity_render_is_lossless(mantissa in any::<i64>(), places in 0u8..=8) {
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
                prop_assert_eq!(frac.len(), usize::from(places));
            }
        }

        #[test]
        fn amount_json_round_trips_through_parser(
            mantissa in any::<i64>(),
            places in 0u8..=6,
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
