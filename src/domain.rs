//! Domain model for the construction-project ledger (M4, `chart-of-accounts.md`).
//!
//! Pure module — no I/O. Account path conventions, transaction-input builders, and AP aging
//! computation are all property-testable without a live hledger.

use crate::hledger::{Amount, BalanceReport, Transaction};
use crate::write::input::{PostingAmount, PostingInput, TransactionInput};

// ---- Account path conventions -------------------------------------------------------

pub const CHECKING_ACCOUNT: &str = "assets:checking";
pub const OWNER_CAPITAL_ACCOUNT: &str = "equity:owner capital";
pub const INTEREST_INCOME_ACCOUNT: &str = "income:interest";

/// The AP account for a vendor: `liabilities:ap:vendor:{vendor}`.
pub fn vendor_ap_account(vendor: &str) -> String {
    format!("liabilities:ap:vendor:{vendor}")
}

/// Shared expense account for a trade type (multiple vendors share one account):
/// `expenses:construction:{trade}`.
pub fn trade_expense_account(trade: &str) -> String {
    format!("expenses:construction:{trade}")
}

/// Dedicated expense account for a professional vendor (one account per vendor):
/// `expenses:professional - {vendor}`.
pub fn professional_expense_account(vendor: &str) -> String {
    format!("expenses:professional - {vendor}")
}

// ---- Transaction-input builders (pure) ---------------------------------------------
//
// Each function returns a `TransactionInput` ready to pass to `write::post_transaction`.
// The accounting convention (CLAUDE.md — *The hledger interface*, write-path):
//   - All amounts are positive numbers as supplied by the caller.
//   - The "balancer" posting (amount = None) lets hledger infer the offsetting amount.

fn posting(
    account: impl Into<String>,
    qty: impl Into<String>,
    commodity: impl Into<String>,
) -> PostingInput {
    PostingInput {
        account: account.into(),
        amount: Some(PostingAmount {
            quantity: qty.into(),
            commodity: commodity.into(),
        }),
    }
}

fn balancer(account: impl Into<String>) -> PostingInput {
    PostingInput {
        account: account.into(),
        amount: None,
    }
}

/// `fund_project`: Dr `assets:checking` / Cr `equity:owner capital`.
pub fn fund_project_input(
    date: String,
    amount: String,
    commodity: String,
    idem: Option<String>,
) -> TransactionInput {
    TransactionInput {
        date,
        description: "Fund project".to_string(),
        postings: vec![
            posting(CHECKING_ACCOUNT, amount, commodity),
            balancer(OWNER_CAPITAL_ACCOUNT),
        ],
        tags: vec![],
        idem,
    }
}

/// `receive_invoice`: Dr `expense_account` / Cr `liabilities:ap:vendor:{vendor}`.
///
/// Tags `invoice:{invoice_ref}` and `vendor:{vendor}` on the transaction.
pub fn receive_invoice_input(
    date: String,
    vendor: &str,
    expense_account: String,
    amount: String,
    commodity: String,
    invoice_ref: String,
    idem: Option<String>,
) -> TransactionInput {
    TransactionInput {
        date,
        description: format!("{vendor} invoice"),
        postings: vec![
            posting(expense_account, amount, commodity),
            balancer(vendor_ap_account(vendor)),
        ],
        tags: vec![
            ("invoice".to_string(), invoice_ref),
            ("vendor".to_string(), vendor.to_string()),
        ],
        idem,
    }
}

/// `pay_invoice`: Dr `liabilities:ap:vendor:{vendor}` / Cr `assets:checking`.
///
/// The `amount` clears (debits) the AP liability; checking is the balancer.
pub fn pay_invoice_input(
    date: String,
    vendor: &str,
    amount: String,
    commodity: String,
    idem: Option<String>,
) -> TransactionInput {
    TransactionInput {
        date,
        description: format!("pay {vendor}"),
        postings: vec![
            posting(vendor_ap_account(vendor), amount, commodity),
            balancer(CHECKING_ACCOUNT),
        ],
        tags: vec![("vendor".to_string(), vendor.to_string())],
        idem,
    }
}

/// `post_interest`: Dr `assets:checking` / Cr `income:interest`.
pub fn post_interest_input(
    date: String,
    amount: String,
    commodity: String,
    idem: Option<String>,
) -> TransactionInput {
    TransactionInput {
        date,
        description: "Interest earned".to_string(),
        postings: vec![
            posting(CHECKING_ACCOUNT, amount, commodity),
            balancer(INTEREST_INCOME_ACCOUNT),
        ],
        tags: vec![],
        idem,
    }
}

// ---- Date arithmetic ---------------------------------------------------------------

use chrono::{Local, NaiveDate};

/// Today's date as `YYYY-MM-DD` (local time).
pub fn today_iso() -> String {
    Local::now().date_naive().format("%Y-%m-%d").to_string()
}

/// Days from `earlier` to `later` (positive = later is after earlier).
/// Returns `None` if either string is not a valid `YYYY-MM-DD`.
pub fn days_between(earlier: &str, later: &str) -> Option<i64> {
    let e = NaiveDate::parse_from_str(earlier, "%Y-%m-%d").ok()?;
    let l = NaiveDate::parse_from_str(later, "%Y-%m-%d").ok()?;
    Some((l - e).num_days())
}

// ---- AP aging ----------------------------------------------------------------------

/// Age bucket for an outstanding AP balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgeCategory {
    /// 0–30 days.
    Current,
    Days31to60,
    Days61to90,
    /// 90+ days — soft-invariant flag is surfaced for these.
    Over90Days,
}

impl AgeCategory {
    pub fn label(&self) -> &'static str {
        match self {
            AgeCategory::Current => "current (0-30 days)",
            AgeCategory::Days31to60 => "31-60 days",
            AgeCategory::Days61to90 => "61-90 days",
            AgeCategory::Over90Days => "90+ days (overdue)",
        }
    }

    pub fn is_overdue(&self) -> bool {
        matches!(self, AgeCategory::Over90Days)
    }
}

/// Classify a non-negative age in days into a bucket.
pub fn age_category(days: u64) -> AgeCategory {
    match days {
        d if d <= 30 => AgeCategory::Current,
        d if d <= 60 => AgeCategory::Days31to60,
        d if d <= 90 => AgeCategory::Days61to90,
        _ => AgeCategory::Over90Days,
    }
}

/// One row in an AP aging report.
#[derive(Debug, Clone)]
pub struct ApAgingEntry {
    /// The vendor AP account, e.g. `"liabilities:ap:vendor:Acme"`.
    pub vendor_account: String,
    /// Outstanding balance (from `hledger balance --flat liabilities:ap`).
    pub outstanding: Vec<Amount>,
    /// Date of the oldest `invoice:`-tagged transaction for this account.
    pub oldest_invoice_date: Option<String>,
    /// Age category based on `oldest_invoice_date` and the `as_of` date.
    pub age: Option<AgeCategory>,
}

/// Compute AP aging: for each vendor AP account with a non-zero balance, find the
/// oldest outstanding invoice date and classify its age.
///
/// `balance` — from `hledger balance --flat liabilities:ap -O json`.
/// `transactions` — from `hledger print liabilities:ap -O json` (all AP transactions).
/// `as_of` — the reference date for age calculation (`YYYY-MM-DD`).
pub fn compute_ap_aging(
    balance: &BalanceReport,
    transactions: &[Transaction],
    as_of: &str,
) -> Vec<ApAgingEntry> {
    balance
        .rows
        .iter()
        .filter(|row| {
            row.account.starts_with("liabilities:ap:")
                && row.amounts.iter().any(|a| a.quantity.mantissa != 0)
        })
        .map(|row| {
            let oldest = oldest_invoice_date_for(&row.account, transactions);
            let age = oldest
                .as_deref()
                .and_then(|d| days_between(d, as_of))
                .and_then(|n| {
                    u64::try_from(n)
                        .map_err(|_| {
                            tracing::warn!(
                                account = %row.account,
                                days = n,
                                "invoice date is in the future relative to as_of; age unknown"
                            );
                        })
                        .ok()
                })
                .map(age_category);
            ApAgingEntry {
                vendor_account: row.account.clone(),
                outstanding: row.amounts.clone(),
                oldest_invoice_date: oldest,
                age,
            }
        })
        .collect()
}

/// The date of the oldest transaction that (a) has a posting to `account` and (b) carries
/// an `invoice:` tag at the transaction level.
fn oldest_invoice_date_for(account: &str, transactions: &[Transaction]) -> Option<String> {
    transactions
        .iter()
        .filter(|txn| {
            let has_account_posting = txn.postings.iter().any(|p| p.account == account);
            let has_invoice_tag = txn.tags.iter().any(|(k, _)| k == "invoice")
                || txn
                    .postings
                    .iter()
                    .any(|p| p.account == account && p.tags.iter().any(|(k, _)| k == "invoice"));
            has_account_posting && has_invoice_tag
        })
        .map(|txn| txn.date.as_str())
        .min()
        .map(str::to_string)
}

// ---- Render helpers ----------------------------------------------------------------

use crate::hledger::amount::render_amounts;

/// Render a [`CompositeReport`] (balancesheet or incomestatement) as a compact text block.
pub fn render_composite(report: &crate::hledger::CompositeReport) -> String {
    let mut lines = vec![report.title.clone()];
    for sub in &report.subreports {
        lines.push(format!("{}:", sub.name));
        for row in &sub.rows {
            lines.push(format!("  {}  {}", row.account, render_amounts(&row.total)));
        }
        if sub.rows.is_empty() {
            lines.push("  (none)".to_string());
        }
        lines.push(format!(
            "  Subtotal: {}",
            if sub.totals.is_empty() {
                "0".to_string()
            } else {
                render_amounts(&sub.totals)
            }
        ));
    }
    lines.push(format!(
        "Net: {}",
        if report.totals.is_empty() {
            "0".to_string()
        } else {
            render_amounts(&report.totals)
        }
    ));
    lines.join("\n")
}

/// Render AP aging entries as a text table.
pub fn render_ap_aging(entries: &[ApAgingEntry], as_of: &str) -> String {
    if entries.is_empty() {
        return format!("AP aging as of {as_of}: (no outstanding payables)");
    }
    let mut lines = vec![format!("AP aging as of {as_of}:")];
    for e in entries {
        let age_label = e
            .age
            .as_ref()
            .map(|a| a.label())
            .unwrap_or("(no invoice date)");
        let oldest = e.oldest_invoice_date.as_deref().unwrap_or("(unknown)");
        lines.push(format!(
            "  {}  {}  oldest invoice: {}  [{}]",
            e.vendor_account,
            render_amounts(&e.outstanding),
            oldest,
            age_label,
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hledger::{AccountBalance, Amount, Quantity};
    use crate::write::input::PostingInput;

    // ---- Account conventions ----

    #[test]
    fn vendor_ap_account_format() {
        assert_eq!(vendor_ap_account("Acme"), "liabilities:ap:vendor:Acme");
        assert_eq!(
            vendor_ap_account("Bob Engineer"),
            "liabilities:ap:vendor:Bob Engineer"
        );
    }

    #[test]
    fn trade_expense_account_format() {
        let acct = trade_expense_account("plumbing");
        assert!(
            acct.starts_with("expenses:construction:"),
            "trade expense under construction: {acct}"
        );
        assert_eq!(acct, "expenses:construction:plumbing");
    }

    #[test]
    fn professional_expense_account_format() {
        let acct = professional_expense_account("Bob Engineer");
        assert!(
            acct.starts_with("expenses:professional - "),
            "professional expense has dedicated name: {acct}"
        );
        assert_eq!(acct, "expenses:professional - Bob Engineer");
    }

    // ---- Input builders ----

    fn has_balancer(input: &TransactionInput) -> bool {
        input
            .postings
            .iter()
            .any(|p: &PostingInput| p.amount.is_none())
    }

    #[test]
    fn fund_project_input_structure() {
        let inp = fund_project_input("2026-01-01".into(), "50000.00".into(), "$".into(), None);
        assert_eq!(inp.postings.len(), 2);
        assert_eq!(inp.postings[0].account, CHECKING_ACCOUNT);
        assert!(inp.postings[0].amount.is_some());
        assert_eq!(inp.postings[1].account, OWNER_CAPITAL_ACCOUNT);
        assert!(has_balancer(&inp));
    }

    #[test]
    fn receive_invoice_input_structure() {
        let inp = receive_invoice_input(
            "2026-02-01".into(),
            "Acme",
            "expenses:construction:plumbing".into(),
            "8000.00".into(),
            "$".into(),
            "INV-001".into(),
            None,
        );
        assert_eq!(inp.postings.len(), 2);
        assert_eq!(inp.postings[0].account, "expenses:construction:plumbing");
        assert_eq!(inp.postings[1].account, vendor_ap_account("Acme"));
        assert!(has_balancer(&inp));
        assert!(
            inp.tags
                .iter()
                .any(|(k, v)| k == "invoice" && v == "INV-001")
        );
        assert!(inp.tags.iter().any(|(k, v)| k == "vendor" && v == "Acme"));
    }

    #[test]
    fn pay_invoice_input_structure() {
        let inp = pay_invoice_input(
            "2026-02-20".into(),
            "Acme",
            "8000.00".into(),
            "$".into(),
            None,
        );
        assert_eq!(inp.postings.len(), 2);
        assert_eq!(inp.postings[0].account, vendor_ap_account("Acme"));
        assert_eq!(inp.postings[1].account, CHECKING_ACCOUNT);
        assert!(has_balancer(&inp));
    }

    #[test]
    fn post_interest_input_structure() {
        let inp = post_interest_input("2026-03-01".into(), "125.00".into(), "$".into(), None);
        assert_eq!(inp.postings.len(), 2);
        assert_eq!(inp.postings[0].account, CHECKING_ACCOUNT);
        assert_eq!(inp.postings[1].account, INTEREST_INCOME_ACCOUNT);
        assert!(has_balancer(&inp));
    }

    // ---- Date arithmetic ----

    #[test]
    fn days_between_known_dates() {
        // 2026-01-01 → 2026-02-01 = 31 days
        assert_eq!(days_between("2026-01-01", "2026-02-01"), Some(31));
        // Same date = 0
        assert_eq!(days_between("2026-06-01", "2026-06-01"), Some(0));
        // Reversed = negative
        assert_eq!(days_between("2026-02-01", "2026-01-01"), Some(-31));
        // Leap year: 2024 has a Feb 29
        assert_eq!(days_between("2024-02-28", "2024-03-01"), Some(2));
        // Non-leap year: 2025 Feb 28 → Mar 1 = 1 day
        assert_eq!(days_between("2025-02-28", "2025-03-01"), Some(1));
    }

    #[test]
    fn days_between_invalid_dates_return_none() {
        assert_eq!(days_between("not-a-date", "2026-01-01"), None);
        assert_eq!(days_between("2026-01-01", "garbage"), None);
        assert_eq!(days_between("2026-13-01", "2026-01-01"), None);
    }

    #[test]
    fn today_iso_is_well_formed() {
        let today = today_iso();
        // YYYY-MM-DD = 10 chars
        assert_eq!(today.len(), 10, "today: {today}");
        let parts: Vec<&str> = today.split('-').collect();
        assert_eq!(parts.len(), 3);
        let y: u32 = parts[0].parse().expect("year");
        let m: u32 = parts[1].parse().expect("month");
        let d: u32 = parts[2].parse().expect("day");
        assert!(y >= 2024, "year looks plausible: {y}");
        assert!((1..=12).contains(&m));
        assert!((1..=31).contains(&d));
    }

    // ---- AP aging ----

    #[test]
    fn age_category_buckets() {
        assert_eq!(age_category(0), AgeCategory::Current);
        assert_eq!(age_category(30), AgeCategory::Current);
        assert_eq!(age_category(31), AgeCategory::Days31to60);
        assert_eq!(age_category(60), AgeCategory::Days31to60);
        assert_eq!(age_category(61), AgeCategory::Days61to90);
        assert_eq!(age_category(90), AgeCategory::Days61to90);
        assert_eq!(age_category(91), AgeCategory::Over90Days);
        assert_eq!(age_category(365), AgeCategory::Over90Days);
    }

    #[test]
    fn age_category_is_monotone() {
        // Larger days → at least as late a bucket
        let categories: Vec<AgeCategory> = [0, 30, 31, 60, 61, 90, 91, 200]
            .iter()
            .map(|&d| age_category(d))
            .collect();
        // Non-decreasing in ordinal order
        let ordinal = |c: &AgeCategory| match c {
            AgeCategory::Current => 0,
            AgeCategory::Days31to60 => 1,
            AgeCategory::Days61to90 => 2,
            AgeCategory::Over90Days => 3,
        };
        for w in categories.windows(2) {
            assert!(ordinal(&w[0]) <= ordinal(&w[1]));
        }
    }

    fn make_amount(mantissa: i128) -> Amount {
        Amount {
            commodity: "$".to_string(),
            quantity: Quantity::new(mantissa, 2),
            commodity_left: true,
            spaced: false,
        }
    }

    fn ap_balance(account: &str, mantissa: i128) -> AccountBalance {
        AccountBalance {
            account: account.to_string(),
            amounts: vec![make_amount(mantissa)],
        }
    }

    fn invoice_txn(date: &str, account: &str) -> Transaction {
        use crate::hledger::Posting;
        Transaction {
            date: date.to_string(),
            description: "test invoice".to_string(),
            index: 1,
            status: "Unmarked".to_string(),
            comment: String::new(),
            tags: vec![("invoice".to_string(), "INV-001".to_string())],
            postings: vec![Posting {
                account: account.to_string(),
                amounts: vec![make_amount(-80000)],
                comment: String::new(),
                tags: vec![],
            }],
        }
    }

    #[test]
    fn compute_ap_aging_basic() {
        let balance = BalanceReport {
            rows: vec![
                ap_balance("liabilities:ap:vendor:Acme", -800000), // $-8000
                ap_balance("liabilities:ap:vendor:Bob", 0),        // zero — excluded
            ],
            totals: vec![],
        };
        let txns = vec![invoice_txn("2026-01-01", "liabilities:ap:vendor:Acme")];
        // As-of 2026-04-15 = 104 days from 2026-01-01 → Over90Days
        let entries = compute_ap_aging(&balance, &txns, "2026-04-15");
        assert_eq!(entries.len(), 1, "zero-balance row excluded");
        assert_eq!(entries[0].vendor_account, "liabilities:ap:vendor:Acme");
        assert_eq!(
            entries[0].oldest_invoice_date.as_deref(),
            Some("2026-01-01")
        );
        assert_eq!(entries[0].age, Some(AgeCategory::Over90Days));
    }

    #[test]
    fn compute_ap_aging_no_invoice_date() {
        let balance = BalanceReport {
            rows: vec![ap_balance("liabilities:ap:vendor:X", -100)],
            totals: vec![],
        };
        // No transactions tagged invoice: → oldest_invoice_date = None, age = None
        let entries = compute_ap_aging(&balance, &[], "2026-06-01");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].oldest_invoice_date.is_none());
        assert!(entries[0].age.is_none());
    }

    #[test]
    fn age_category_labels_are_exact() {
        assert_eq!(AgeCategory::Current.label(), "current (0-30 days)");
        assert_eq!(AgeCategory::Days31to60.label(), "31-60 days");
        assert_eq!(AgeCategory::Days61to90.label(), "61-90 days");
        assert_eq!(AgeCategory::Over90Days.label(), "90+ days (overdue)");
    }

    #[test]
    fn compute_ap_aging_excludes_non_invoice_tagged_transactions() {
        use crate::hledger::Posting;
        let balance = BalanceReport {
            rows: vec![ap_balance("liabilities:ap:vendor:X", -5000)],
            totals: vec![],
        };
        let non_invoice = Transaction {
            date: "2026-01-01".to_string(),
            description: "payment".to_string(),
            index: 1,
            status: "Unmarked".to_string(),
            comment: String::new(),
            tags: vec![("other".to_string(), "value".to_string())],
            postings: vec![Posting {
                account: "liabilities:ap:vendor:X".to_string(),
                amounts: vec![make_amount(-5000)],
                comment: String::new(),
                tags: vec![],
            }],
        };
        let entries = compute_ap_aging(&balance, &[non_invoice], "2026-06-01");
        assert!(
            entries[0].oldest_invoice_date.is_none(),
            "non-invoice txn must not count as invoice date"
        );
    }

    #[test]
    fn compute_ap_aging_excludes_invoice_txns_for_other_accounts() {
        let balance = BalanceReport {
            rows: vec![ap_balance("liabilities:ap:vendor:X", -5000)],
            totals: vec![],
        };
        // Invoice txn for a different account: should not contribute to X's date
        let other = invoice_txn("2026-01-01", "liabilities:ap:vendor:OTHER");
        let entries = compute_ap_aging(&balance, &[other], "2026-06-01");
        assert!(entries[0].oldest_invoice_date.is_none());
    }

    #[test]
    fn render_composite_basic() {
        use crate::hledger::{CompositeReport, ReportRow, Subreport};
        let report = CompositeReport {
            title: "Balance Sheet".to_string(),
            subreports: vec![Subreport {
                name: "Assets".to_string(),
                rows: vec![ReportRow {
                    account: "assets:checking".to_string(),
                    total: vec![make_amount(10000)],
                }],
                totals: vec![make_amount(10000)],
                is_positive: true,
            }],
            totals: vec![make_amount(10000)],
        };
        let text = render_composite(&report);
        assert!(text.starts_with("Balance Sheet"), "title: {text}");
        assert!(text.contains("Assets:"), "subreport name: {text}");
        assert!(text.contains("assets:checking"), "account row: {text}");
        assert!(text.contains("$100.00"), "amount: {text}");
        assert!(text.contains("Net:"), "net total line: {text}");
    }

    #[test]
    fn render_ap_aging_empty_returns_no_payables_message() {
        let text = render_ap_aging(&[], "2026-06-01");
        assert_eq!(text, "AP aging as of 2026-06-01: (no outstanding payables)");
    }

    #[test]
    fn render_ap_aging_with_entry_contains_key_fields() {
        let entries = vec![ApAgingEntry {
            vendor_account: "liabilities:ap:vendor:Acme".to_string(),
            outstanding: vec![make_amount(-800000)],
            oldest_invoice_date: Some("2026-01-01".to_string()),
            age: Some(AgeCategory::Over90Days),
        }];
        let text = render_ap_aging(&entries, "2026-06-01");
        assert!(text.contains("AP aging as of 2026-06-01"), "header: {text}");
        assert!(
            text.contains("liabilities:ap:vendor:Acme"),
            "vendor: {text}"
        );
        assert!(text.contains("90+ days (overdue)"), "age label: {text}");
        assert!(text.contains("2026-01-01"), "oldest invoice date: {text}");
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn age_category_handles_all_non_negative_days(days in 0u64..10000) {
            let cat = age_category(days);
            // Every non-negative input maps to exactly one bucket
            let _ = cat.label();
        }

        #[test]
        fn days_between_roundtrips_unix_days(year in 2000i32..=2100, month in 1u32..=12, day in 1u32..=28) {
            // Use days 1-28 so we never hit an invalid date (Feb only has 28 days in some years)
            let date = format!("{year:04}-{month:02}-{day:02}");
            // days_between(d, d) == 0 for any valid date
            let diff = days_between(&date, &date);
            prop_assert_eq!(diff, Some(0));
        }

        #[test]
        fn trade_expense_always_under_construction(trade in "[a-z]{3,20}") {
            let acct = trade_expense_account(&trade);
            prop_assert!(acct.starts_with("expenses:construction:"));
            prop_assert!(acct.ends_with(&trade));
        }

        #[test]
        fn professional_expense_always_dedicated(vendor in "[A-Za-z ]{3,20}") {
            let acct = professional_expense_account(&vendor);
            prop_assert!(acct.starts_with("expenses:professional - "));
        }

        #[test]
        fn vendor_ap_always_under_liabilities_ap(vendor in "[A-Za-z ]{2,20}") {
            let acct = vendor_ap_account(&vendor);
            prop_assert!(acct.starts_with("liabilities:ap:vendor:"));
        }
    }
}
