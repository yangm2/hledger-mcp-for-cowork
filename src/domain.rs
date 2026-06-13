//! Domain model for the construction-project ledger (M4, `chart-of-accounts.md`).
//!
//! Pure module — no I/O. Account path conventions, transaction-input builders, and AP aging
//! computation are all property-testable without a live hledger.

use chrono::NaiveDate;

use crate::hledger::amount::Commodity;
use crate::hledger::{Amount, BalanceReport, Transaction};
use crate::write::input::{PostingAmount, PostingInput, TransactionInput};

// ---- Account path conventions -------------------------------------------------------

pub const CHECKING_ACCOUNT: &str = "assets:checking";
pub const OWNER_CAPITAL_ACCOUNT: &str = "equity:owner capital";
pub const INTEREST_INCOME_ACCOUNT: &str = "income:interest";

/// Root of the accounts-payable subtree (the `get_ap_aging` query scope).
pub const AP_ROOT: &str = "liabilities:ap";

/// Prefix of every per-vendor AP account (see [`vendor_ap_account`]).
pub const VENDOR_AP_PREFIX: &str = "liabilities:ap:vendor:";

/// The AP account for a vendor: `liabilities:ap:vendor:{vendor}`.
pub fn vendor_ap_account(vendor: &str) -> String {
    format!("{VENDOR_AP_PREFIX}{vendor}")
}

/// Whether `account` is inside the AP subtree (an aging-report row).
pub fn is_ap_account(account: &str) -> bool {
    account
        .strip_prefix(AP_ROOT)
        .is_some_and(|rest| rest.starts_with(':'))
}

/// Whether the AP balance report shows any outstanding payable. Double-entry sign
/// convention: a liability owed appears as a **negative** amount, so "outstanding"
/// means some AP row carries a negative quantity. `pay_invoice`'s precondition.
pub fn has_outstanding_ap(balance: &BalanceReport) -> bool {
    balance
        .rows
        .iter()
        .any(|row| row.amounts.iter().any(|a| a.quantity.mantissa < 0))
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

/// The two vendor kinds, deciding which expense account `vendor_add` declares.
/// Doubles as the MCP arg type (the schema advertises the two lowercase values).
#[derive(Debug, Clone, Copy, serde::Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum VendorType {
    /// Shared trade expense account (`expenses:construction:{trade}`).
    Trade,
    /// Dedicated per-vendor account (`expenses:professional - {vendor}`).
    Professional,
}

/// Resolve the expense account `vendor_add` declares for a vendor, enforcing the
/// "trade name required for trade vendors" rule. `Err` is a correctable input error.
pub fn vendor_expense_account(
    vendor_type: VendorType,
    vendor: &str,
    trade: Option<&str>,
) -> Result<String, String> {
    match vendor_type {
        VendorType::Trade => trade
            .map(trade_expense_account)
            .ok_or_else(|| "`trade` is required when vendor_type is \"trade\"".to_string()),
        VendorType::Professional => Ok(professional_expense_account(vendor)),
    }
}

// ---- Change orders (ECO, M5) ---------------------------------------------------------
//
// The CO lifecycle is a pure account-and-tag state machine over existing write primitives —
// no new journal grammar:
//   - `eco_create` posts the CO amount into the **pending** subtree
//     (`expenses:change orders:pending:{trade}`) against the vendor's AP — committed
//     exposure is visible immediately, but outside the budget-tracked per-trade account.
//   - `eco_approve` (the **decide** call) transfers pending → the budget-tracked
//     `expenses:change orders:{trade}`.
//   - `eco_void` reverses the CO's unreversed transactions (append-only, as ever).
// State is queryable by tag: every CO transaction carries `eco:{id}` and an
// `eco_event:created|approved` marker.

/// Root of the change-order (parallel) expense hierarchy.
pub const CO_ROOT: &str = "expenses:change orders";

/// Prefix of the **pending** (created, not yet approved) CO subtree. `pending` is therefore
/// a reserved trade name under `change orders` (documented in `ledger://eco-guide`).
pub const CO_PENDING_PREFIX: &str = "expenses:change orders:pending:";

/// The budget-tracked CO account for a trade: `expenses:change orders:{trade}`.
pub fn eco_account(trade: &str) -> String {
    format!("{CO_ROOT}:{trade}")
}

/// The pending CO account for a trade: `expenses:change orders:pending:{trade}`.
pub fn eco_pending_account(trade: &str) -> String {
    format!("{CO_PENDING_PREFIX}{trade}")
}

/// The `eco_event` tag values marking each lifecycle step.
pub const ECO_EVENT_CREATED: &str = "created";
pub const ECO_EVENT_APPROVED: &str = "approved";

/// `eco_create`: Dr `expenses:change orders:pending:{trade}` / Cr the vendor's AP.
// One arg per CO field, mirroring the other input builders — a param struct would just
// rename the same eight fields.
#[allow(clippy::too_many_arguments)]
pub fn eco_create_input(
    date: NaiveDate,
    eco: &str,
    trade: &str,
    vendor: &str,
    description: &str,
    amount: String,
    commodity: Commodity,
    idem: Option<String>,
) -> TransactionInput {
    TransactionInput {
        date,
        description: format!("ECO {eco}: {description}"),
        postings: vec![
            posting(eco_pending_account(trade), amount, commodity),
            balancer(vendor_ap_account(vendor)),
        ],
        tags: vec![
            ("eco".to_string(), eco.to_string()),
            ("eco_event".to_string(), ECO_EVENT_CREATED.to_string()),
            ("vendor".to_string(), vendor.to_string()),
        ],
        idem,
    }
}

/// `eco_approve`: transfer the CO amount pending → budget-tracked.
/// Dr `expenses:change orders:{trade}` / Cr `expenses:change orders:pending:{trade}`.
pub fn eco_approve_input(
    date: NaiveDate,
    eco: &str,
    trade: &str,
    amount: String,
    commodity: Commodity,
) -> TransactionInput {
    TransactionInput {
        date,
        description: format!("ECO {eco}: approved"),
        postings: vec![
            posting(eco_account(trade), amount, commodity),
            balancer(eco_pending_account(trade)),
        ],
        tags: vec![
            ("eco".to_string(), eco.to_string()),
            ("eco_event".to_string(), ECO_EVENT_APPROVED.to_string()),
        ],
        idem: None,
    }
}

/// The CO details recoverable from a created transaction: the trade (from the pending
/// account path) and the posted amount. `None` if the transaction has no pending-CO posting
/// with an amount (not a CO create transaction).
pub fn eco_details(txn: &Transaction) -> Option<(String, &Amount)> {
    txn.postings.iter().find_map(|p| {
        let trade = p.account.strip_prefix(CO_PENDING_PREFIX)?;
        let amount = p.amounts.first()?;
        Some((trade.to_string(), amount))
    })
}

// ---- Budget vs actual (M5) -----------------------------------------------------------

/// Whether `actual` strictly exceeds `goal`, aligning the two quantities' decimal places
/// (`300.00` vs `500` compares 30000 vs 50000 at 2 places). The over-budget predicate.
pub fn exceeds(actual: &crate::hledger::Quantity, goal: &crate::hledger::Quantity) -> bool {
    let places = actual.places.max(goal.places);
    let scale =
        |q: &crate::hledger::Quantity| q.mantissa * 10i128.pow(u32::from(places - q.places));
    scale(actual) > scale(goal)
}

/// Render a [`BudgetReport`] as text: one `account  actual <amts> | budget <amts>` line per
/// row, then the totals line. Unbudgeted rows show `budget (none)`.
pub fn render_budget(report: &crate::hledger::BudgetReport) -> String {
    use crate::hledger::amount::render_amounts;
    let budget_cell = |goal: &[Amount]| {
        if goal.is_empty() {
            "(none)".to_string()
        } else {
            render_amounts(goal)
        }
    };
    let mut lines: Vec<String> = report
        .rows
        .iter()
        .map(|row| {
            format!(
                "{}  actual {} | budget {}",
                row.account,
                render_amounts(&row.actual),
                budget_cell(&row.goal)
            )
        })
        .collect();
    if report.rows.is_empty() {
        lines.push("(no budgeted accounts and no activity)".to_string());
    }
    lines.push(format!(
        "total  actual {} | budget {}",
        render_amounts(&report.total_actual),
        budget_cell(&report.total_goal)
    ));
    lines.join("\n")
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
    commodity: impl Into<Commodity>,
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
    date: NaiveDate,
    amount: String,
    commodity: Commodity,
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
    date: NaiveDate,
    vendor: &str,
    expense_account: String,
    amount: String,
    commodity: Commodity,
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
    date: NaiveDate,
    vendor: &str,
    amount: String,
    commodity: Commodity,
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
    date: NaiveDate,
    amount: String,
    commodity: Commodity,
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

use chrono::Local;

/// Today's date (local time).
pub fn today() -> NaiveDate {
    Local::now().date_naive()
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
    pub oldest_invoice_date: Option<NaiveDate>,
    /// Age category based on `oldest_invoice_date` and the `as_of` date.
    pub age: Option<AgeCategory>,
}

/// Compute AP aging: for each vendor AP account with a non-zero balance, find the
/// oldest outstanding invoice date and classify its age.
///
/// `balance` — from `hledger balance --flat liabilities:ap -O json`.
/// `transactions` — from `hledger print liabilities:ap -O json` (all AP transactions).
/// `as_of` — the reference date for age calculation.
pub fn compute_ap_aging(
    balance: &BalanceReport,
    transactions: &[Transaction],
    as_of: NaiveDate,
) -> Vec<ApAgingEntry> {
    balance
        .rows
        .iter()
        .filter(|row| {
            is_ap_account(&row.account) && row.amounts.iter().any(|a| a.quantity.mantissa != 0)
        })
        .map(|row| {
            let oldest = oldest_invoice_date_for(&row.account, transactions);
            let age = oldest
                .and_then(|d| {
                    let n = (as_of - d).num_days();
                    u64::try_from(n)
                        .map_err(|_| {
                            tracing::warn!(
                                account = %row.account,
                                date = %d,
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
fn oldest_invoice_date_for(account: &str, transactions: &[Transaction]) -> Option<NaiveDate> {
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
        .map(|txn| txn.date)
        .min()
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
        lines.push(format!("  Subtotal: {}", render_amounts(&sub.totals)));
    }
    lines.push(format!("Net: {}", render_amounts(&report.totals)));
    lines.join("\n")
}

/// Render AP aging entries as a text table.
pub fn render_ap_aging(entries: &[ApAgingEntry], as_of: NaiveDate) -> String {
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
        let oldest = e
            .oldest_invoice_date
            .map_or_else(|| "(unknown)".to_string(), |d| d.to_string());
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
        let inp = fund_project_input(nd(2026, 1, 1), "50000.00".into(), "$".into(), None);
        assert_eq!(inp.postings.len(), 2);
        assert_eq!(inp.postings[0].account, CHECKING_ACCOUNT);
        assert!(inp.postings[0].amount.is_some());
        assert_eq!(inp.postings[1].account, OWNER_CAPITAL_ACCOUNT);
        assert!(has_balancer(&inp));
    }

    #[test]
    fn receive_invoice_input_structure() {
        let inp = receive_invoice_input(
            nd(2026, 2, 1),
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
        let inp = pay_invoice_input(nd(2026, 2, 20), "Acme", "8000.00".into(), "$".into(), None);
        assert_eq!(inp.postings.len(), 2);
        assert_eq!(inp.postings[0].account, vendor_ap_account("Acme"));
        assert_eq!(inp.postings[1].account, CHECKING_ACCOUNT);
        assert!(has_balancer(&inp));
    }

    #[test]
    fn post_interest_input_structure() {
        let inp = post_interest_input(nd(2026, 3, 1), "125.00".into(), "$".into(), None);
        assert_eq!(inp.postings.len(), 2);
        assert_eq!(inp.postings[0].account, CHECKING_ACCOUNT);
        assert_eq!(inp.postings[1].account, INTEREST_INCOME_ACCOUNT);
        assert!(has_balancer(&inp));
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
            commodity: "$".into(),
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

    fn nd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn invoice_txn(date: &str, account: &str) -> Transaction {
        use crate::hledger::Posting;
        Transaction {
            date: NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap(),
            description: "test invoice".to_string(),
            index: 1,
            status: crate::hledger::Status::Unmarked,
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
        let entries = compute_ap_aging(&balance, &txns, nd(2026, 4, 15));
        assert_eq!(entries.len(), 1, "zero-balance row excluded");
        assert_eq!(entries[0].vendor_account, "liabilities:ap:vendor:Acme");
        assert_eq!(entries[0].oldest_invoice_date, Some(nd(2026, 1, 1)));
        assert_eq!(entries[0].age, Some(AgeCategory::Over90Days));
    }

    #[test]
    fn compute_ap_aging_no_invoice_date() {
        let balance = BalanceReport {
            rows: vec![ap_balance("liabilities:ap:vendor:X", -100)],
            totals: vec![],
        };
        // No transactions tagged invoice: → oldest_invoice_date = None, age = None
        let entries = compute_ap_aging(&balance, &[], nd(2026, 6, 1));
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
            date: nd(2026, 1, 1),
            description: "payment".to_string(),
            index: 1,
            status: crate::hledger::Status::Unmarked,
            comment: String::new(),
            tags: vec![("other".to_string(), "value".to_string())],
            postings: vec![Posting {
                account: "liabilities:ap:vendor:X".to_string(),
                amounts: vec![make_amount(-5000)],
                comment: String::new(),
                tags: vec![],
            }],
        };
        let entries = compute_ap_aging(&balance, &[non_invoice], nd(2026, 6, 1));
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
        let entries = compute_ap_aging(&balance, &[other], nd(2026, 6, 1));
        assert!(entries[0].oldest_invoice_date.is_none());
    }

    /// Build a two-posting txn with **no** transaction-level tags: one untagged posting to
    /// `account`, plus a posting to `other_account` carrying `(tag_key: INV-001)`.
    /// Exercises the posting-level half of the invoice-tag check in isolation.
    fn posting_tagged_txn(account: &str, other_account: &str, tag_key: &str) -> Transaction {
        use crate::hledger::Posting;
        let posting = |acct: &str, tags: Vec<(String, String)>| Posting {
            account: acct.to_string(),
            amounts: vec![make_amount(-5000)],
            comment: String::new(),
            tags,
        };
        Transaction {
            date: nd(2026, 1, 1),
            description: "posting-tagged".to_string(),
            index: 1,
            status: crate::hledger::Status::Unmarked,
            comment: String::new(),
            tags: vec![],
            postings: vec![
                posting(account, vec![]),
                posting(
                    other_account,
                    vec![(tag_key.to_string(), "INV-001".to_string())],
                ),
            ],
        }
    }

    #[test]
    fn posting_level_invoice_tag_counts_only_on_the_target_account() {
        let balance = BalanceReport {
            rows: vec![ap_balance("liabilities:ap:vendor:X", -5000)],
            totals: vec![],
        };
        let x = "liabilities:ap:vendor:X";

        // Invoice tag on X's own posting → counted.
        let on_target = posting_tagged_txn("expenses:misc", x, "invoice");
        let entries = compute_ap_aging(&balance, &[on_target], nd(2026, 6, 1));
        assert_eq!(entries[0].oldest_invoice_date, Some(nd(2026, 1, 1)));

        // Invoice tag on a *different* account's posting (X's posting untagged) → excluded.
        let on_other = posting_tagged_txn(x, "expenses:misc", "invoice");
        let entries = compute_ap_aging(&balance, &[on_other], nd(2026, 6, 1));
        assert!(entries[0].oldest_invoice_date.is_none());

        // A non-invoice posting-level tag on X's posting → excluded.
        let wrong_tag = posting_tagged_txn("expenses:misc", x, "vendor");
        let entries = compute_ap_aging(&balance, &[wrong_tag], nd(2026, 6, 1));
        assert!(entries[0].oldest_invoice_date.is_none());
    }

    #[test]
    fn is_ap_account_requires_a_subtree_segment() {
        assert!(is_ap_account("liabilities:ap:vendor:Acme"));
        assert!(is_ap_account("liabilities:ap:other"));
        // Bare root, sibling prefix, and unrelated accounts are all outside the subtree.
        assert!(!is_ap_account("liabilities:ap"));
        assert!(!is_ap_account("liabilities:apx:foo"));
        assert!(!is_ap_account("assets:checking"));
    }

    #[test]
    fn has_outstanding_ap_requires_a_strictly_negative_amount() {
        let report = |mantissa: i128| BalanceReport {
            rows: vec![ap_balance("liabilities:ap:vendor:X", mantissa)],
            totals: vec![],
        };
        assert!(has_outstanding_ap(&report(-1)));
        // Zero is settled, not outstanding; positive (overpayment) isn't outstanding either.
        assert!(!has_outstanding_ap(&report(0)));
        assert!(!has_outstanding_ap(&report(100)));
        assert!(!has_outstanding_ap(&BalanceReport {
            rows: vec![],
            totals: vec![]
        }));
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
        let text = render_ap_aging(&[], nd(2026, 6, 1));
        assert_eq!(text, "AP aging as of 2026-06-01: (no outstanding payables)");
    }

    #[test]
    fn render_ap_aging_with_entry_contains_key_fields() {
        let entries = vec![ApAgingEntry {
            vendor_account: "liabilities:ap:vendor:Acme".to_string(),
            outstanding: vec![make_amount(-800000)],
            oldest_invoice_date: Some(nd(2026, 1, 1)),
            age: Some(AgeCategory::Over90Days),
        }];
        let text = render_ap_aging(&entries, nd(2026, 6, 1));
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

    // ---- M5: change orders + budget ------------------------------------------------

    #[test]
    fn eco_account_paths() {
        assert_eq!(
            eco_account("electrical"),
            "expenses:change orders:electrical"
        );
        assert_eq!(
            eco_pending_account("electrical"),
            "expenses:change orders:pending:electrical"
        );
    }

    #[test]
    fn eco_create_input_posts_pending_against_vendor_ap_with_lifecycle_tags() {
        let input = eco_create_input(
            nd(2026, 2, 1),
            "7",
            "electrical",
            "GC",
            "add outlets",
            "1500.00".to_string(),
            "$".into(),
            None,
        );
        assert_eq!(input.description, "ECO 7: add outlets");
        assert_eq!(input.postings.len(), 2);
        assert_eq!(
            input.postings[0].account,
            "expenses:change orders:pending:electrical"
        );
        assert_eq!(input.postings[1].account, "liabilities:ap:vendor:GC");
        assert!(input.postings[1].amount.is_none(), "AP is the balancer");
        assert!(input.tags.contains(&("eco".to_string(), "7".to_string())));
        assert!(
            input
                .tags
                .contains(&("eco_event".to_string(), "created".to_string()))
        );
    }

    #[test]
    fn eco_approve_input_transfers_pending_to_tracked() {
        let input = eco_approve_input(
            nd(2026, 2, 5),
            "7",
            "electrical",
            "1500.00".to_string(),
            "$".into(),
        );
        assert_eq!(
            input.postings[0].account,
            "expenses:change orders:electrical"
        );
        assert_eq!(
            input.postings[1].account,
            "expenses:change orders:pending:electrical"
        );
        assert!(input.postings[1].amount.is_none());
        assert!(
            input
                .tags
                .contains(&("eco_event".to_string(), "approved".to_string()))
        );
    }

    #[test]
    fn eco_details_finds_the_pending_posting_or_nothing() {
        use crate::hledger::Posting;
        let pending = Posting {
            account: "expenses:change orders:pending:hvac".to_string(),
            amounts: vec![make_amount(200000)],
            comment: String::new(),
            tags: vec![],
        };
        let other = Posting {
            account: "liabilities:ap:vendor:GC".to_string(),
            amounts: vec![make_amount(-200000)],
            comment: String::new(),
            tags: vec![],
        };
        let txn = |postings: Vec<Posting>| Transaction {
            date: nd(2026, 2, 1),
            description: "ECO 9".to_string(),
            index: 1,
            status: crate::hledger::Status::Unmarked,
            comment: String::new(),
            tags: vec![],
            postings,
        };
        let create = txn(vec![other.clone(), pending]);
        let (trade, amount) = eco_details(&create).expect("pending posting found");
        assert_eq!(trade, "hvac");
        assert_eq!(amount.quantity.mantissa, 200000);
        assert!(
            eco_details(&txn(vec![other])).is_none(),
            "no pending-CO posting → not a CO create txn"
        );
    }

    #[test]
    fn exceeds_aligns_decimal_places_and_is_strict() {
        use crate::hledger::Quantity;
        // 300.00 (mantissa 30000, 2dp) vs 500 (mantissa 500, 0dp).
        assert!(!exceeds(&Quantity::new(30000, 2), &Quantity::new(500, 0)));
        assert!(exceeds(&Quantity::new(80000, 2), &Quantity::new(500, 0)));
        // Equal after alignment → NOT exceeding (strict).
        assert!(!exceeds(&Quantity::new(50000, 2), &Quantity::new(500, 0)));
        assert!(!exceeds(&Quantity::new(500, 0), &Quantity::new(50000, 2)));
        // Mirrored places on the other side.
        assert!(exceeds(&Quantity::new(501, 0), &Quantity::new(50000, 2)));
    }

    #[test]
    fn render_budget_lists_rows_unbudgeted_and_totals() {
        use crate::hledger::{BudgetReport, BudgetRow};
        let report = BudgetReport {
            rows: vec![
                BudgetRow {
                    account: "expenses:construction:plumbing".to_string(),
                    actual: vec![make_amount(80000)],
                    goal: vec![make_amount(50000)],
                },
                BudgetRow {
                    account: "<unbudgeted>".to_string(),
                    actual: vec![make_amount(100)],
                    goal: vec![],
                },
            ],
            total_actual: vec![make_amount(80100)],
            total_goal: vec![make_amount(50000)],
        };
        let text = render_budget(&report);
        assert!(
            text.contains("expenses:construction:plumbing  actual $800.00 | budget $500.00"),
            "{text}"
        );
        assert!(
            text.contains("<unbudgeted>  actual $1.00 | budget (none)"),
            "{text}"
        );
        assert!(
            text.contains("total  actual $801.00 | budget $500.00"),
            "{text}"
        );

        let empty = render_budget(&BudgetReport {
            rows: vec![],
            total_actual: vec![],
            total_goal: vec![],
        });
        assert!(
            empty.contains("(no budgeted accounts and no activity)"),
            "{empty}"
        );
    }
}
