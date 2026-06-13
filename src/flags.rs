//! Soft-invariant flags (M3, `concurrency-model.md` "Soft invariants → flags").
//!
//! Budget overruns, overdrafts, and AP-aging are **computed and surfaced** in read /
//! report output — **never enforced**: a record call is never rejected for violating
//! one (C-6). This module is the mechanism plus the one flag whose data exists in M3,
//! **overdraft**; the AP-aging flag lands with M4's `get_ap_aging`, over-budget with
//! M5's `get_budget_vs_actual`.
//!
//! Pure (report in, flags out) — unit- and mutation-testable without hledger.

use crate::domain::ApAgingEntry;
use crate::hledger::{BalanceReport, amount::render_amounts};

/// The closed set of soft invariants we surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagKind {
    Overdraft,
    ApAging,
    OverBudget,
}

impl FlagKind {
    /// The label used in rendered report footers.
    pub fn label(self) -> &'static str {
        match self {
            FlagKind::Overdraft => "overdraft",
            FlagKind::ApAging => "ap-aging",
            FlagKind::OverBudget => "over-budget",
        }
    }
}

/// One surfaced soft-invariant violation. Informational only, by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    /// The invariant kind.
    pub kind: FlagKind,
    /// The account the flag is about.
    pub account: String,
    /// Human-readable detail (rendered amounts).
    pub detail: String,
}

impl std::fmt::Display for Flag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "flag {}: {} {}",
            self.kind.label(),
            self.account,
            self.detail
        )
    }
}

/// Whether `account` is an asset account (`assets` or any sub-account of it).
fn is_asset(account: &str) -> bool {
    account == "assets" || account.starts_with("assets:")
}

/// Compute **overdraft** flags from a balance report: any asset account holding a
/// negative amount (in any commodity) is flagged. Surfaced alongside the report,
/// never used to reject the write that caused it (C-6).
pub fn overdraft_flags(report: &BalanceReport) -> Vec<Flag> {
    let mut flags = Vec::new();
    for row in &report.rows {
        if !is_asset(&row.account) {
            continue;
        }
        for amount in &row.amounts {
            if amount.quantity.mantissa < 0 {
                flags.push(Flag {
                    kind: FlagKind::Overdraft,
                    account: row.account.clone(),
                    detail: format!("balance {}", amount.render()),
                });
            }
        }
    }
    flags
}

/// Compute **AP-aging** flags: any vendor AP account with an outstanding balance 90+ days
/// old is flagged as overdue. Surfaced alongside aging reports, never enforced (C-6).
pub fn ap_aging_flags(entries: &[ApAgingEntry]) -> Vec<Flag> {
    entries
        .iter()
        .filter(|e| e.age.as_ref().map(|a| a.is_overdue()).unwrap_or(false))
        .map(|e| Flag {
            kind: FlagKind::ApAging,
            account: e.vendor_account.clone(),
            detail: format!(
                "outstanding {} since {} (90+ days overdue)",
                render_amounts(&e.outstanding),
                e.oldest_invoice_date
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "(unknown)".to_string())
            ),
        })
        .collect()
}

/// Compute **over-budget** flags from a budget report: any budgeted row whose actual
/// exceeds its goal (per commodity). Surfaced alongside `get_budget_vs_actual` output,
/// never enforced — over-budget is information, not a write rejection (C-6).
pub fn over_budget_flags(report: &crate::hledger::BudgetReport) -> Vec<Flag> {
    let mut flags = Vec::new();
    for row in &report.rows {
        for goal in &row.goal {
            // Only positive (expense-style) goals are overrun targets — the budget rule's
            // equity balancing posting shows up as a *negative* goal and must not flag.
            if goal.quantity.mantissa <= 0 {
                continue;
            }
            let Some(actual) = row.actual.iter().find(|a| a.commodity == goal.commodity) else {
                continue;
            };
            if crate::domain::exceeds(&actual.quantity, &goal.quantity) {
                flags.push(Flag {
                    kind: FlagKind::OverBudget,
                    account: row.account.clone(),
                    detail: format!("actual {} > budget {}", actual.render(), goal.render()),
                });
            }
        }
    }
    flags
}

/// Render flags as report-footer lines (empty string when there are none).
pub fn render_flags(flags: &[Flag]) -> String {
    flags
        .iter()
        .map(Flag::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hledger::{AccountBalance, Amount, Quantity};

    fn row(account: &str, mantissa: i128) -> AccountBalance {
        AccountBalance {
            account: account.to_string(),
            amounts: vec![Amount {
                commodity: "$".into(),
                quantity: Quantity::new(mantissa, 2),
                commodity_left: true,
                spaced: false,
            }],
        }
    }

    fn report(rows: Vec<AccountBalance>) -> BalanceReport {
        BalanceReport {
            rows,
            totals: vec![],
        }
    }

    #[test]
    fn negative_asset_balance_is_flagged() {
        let flags = overdraft_flags(&report(vec![row("assets:checking", -1250)]));
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].kind, FlagKind::Overdraft);
        assert_eq!(flags[0].account, "assets:checking");
        assert_eq!(flags[0].detail, "balance $-12.50");
    }

    #[test]
    fn positive_and_zero_asset_balances_are_not_flagged() {
        let flags = overdraft_flags(&report(vec![
            row("assets:checking", 1250),
            row("assets:savings", 0),
        ]));
        assert!(flags.is_empty());
    }

    #[test]
    fn non_asset_negatives_are_not_overdrafts() {
        // Liabilities/equity/income are conventionally negative — not an overdraft.
        let flags = overdraft_flags(&report(vec![
            row("liabilities:ap:vendor", -5000),
            row("equity:opening", -100),
            row("income:interest", -1),
        ]));
        assert!(flags.is_empty());
    }

    #[test]
    fn asset_prefix_must_be_a_path_segment() {
        // `assetsy:…` is not an asset account; bare `assets` is.
        assert!(overdraft_flags(&report(vec![row("assetsy:x", -1)])).is_empty());
        assert_eq!(overdraft_flags(&report(vec![row("assets", -1)])).len(), 1);
    }

    #[test]
    fn renders_as_footer_lines() {
        let flags = overdraft_flags(&report(vec![row("assets:a", -100), row("assets:b", -200)]));
        let text = render_flags(&flags);
        assert_eq!(
            text,
            "flag overdraft: assets:a balance $-1.00\nflag overdraft: assets:b balance $-2.00"
        );
        assert_eq!(render_flags(&[]), "");
    }

    #[test]
    fn ap_aging_flags_only_overdue() {
        use crate::domain::{AgeCategory, ApAgingEntry};
        fn aging_entry(account: &str, age: Option<AgeCategory>) -> ApAgingEntry {
            ApAgingEntry {
                vendor_account: account.to_string(),
                outstanding: vec![Amount {
                    commodity: "$".into(),
                    quantity: crate::hledger::Quantity::new(250000, 2),
                    commodity_left: true,
                    spaced: false,
                }],
                oldest_invoice_date: Some(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
                age,
            }
        }
        let entries = vec![
            aging_entry("liabilities:ap:vendor:A", Some(AgeCategory::Over90Days)),
            aging_entry("liabilities:ap:vendor:B", Some(AgeCategory::Current)),
            aging_entry("liabilities:ap:vendor:C", None),
        ];
        let flags = ap_aging_flags(&entries);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].kind, FlagKind::ApAging);
        assert_eq!(flags[0].account, "liabilities:ap:vendor:A");
        assert!(flags[0].detail.contains("overdue"), "{}", flags[0].detail);
    }

    fn amount(mantissa: i128) -> Amount {
        Amount {
            commodity: "$".into(),
            quantity: Quantity::new(mantissa, 2),
            commodity_left: true,
            spaced: false,
        }
    }

    fn budget_report(rows: Vec<crate::hledger::BudgetRow>) -> crate::hledger::BudgetReport {
        crate::hledger::BudgetReport {
            rows,
            total_actual: vec![],
            total_goal: vec![],
        }
    }

    fn budget_row(account: &str, actual: i128, goal: i128) -> crate::hledger::BudgetRow {
        crate::hledger::BudgetRow {
            account: account.to_string(),
            actual: vec![amount(actual)],
            goal: vec![amount(goal)],
        }
    }

    #[test]
    fn over_budget_flags_only_strictly_over_positive_goals() {
        // Over → flagged, with both amounts in the detail.
        let flags = over_budget_flags(&budget_report(vec![budget_row("expenses:x", 80000, 50000)]));
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].kind, FlagKind::OverBudget);
        assert_eq!(flags[0].account, "expenses:x");
        assert_eq!(flags[0].detail, "actual $800.00 > budget $500.00");

        // Exactly at goal, or under → no flag (strictly-over predicate).
        assert!(
            over_budget_flags(&budget_report(vec![budget_row("expenses:x", 50000, 50000)]))
                .is_empty(),
            "at goal is not over"
        );
        assert!(
            over_budget_flags(&budget_report(vec![budget_row("expenses:x", 100, 50000)]))
                .is_empty()
        );
    }

    #[test]
    fn over_budget_ignores_negative_goals_unbudgeted_rows_and_other_commodities() {
        // The budget rule's equity balancer carries a NEGATIVE goal — never an overrun target.
        assert!(
            over_budget_flags(&budget_report(vec![budget_row("equity:budget", 0, -50000)]))
                .is_empty(),
            "negative goal (the equity balancer) must not flag"
        );
        // Unbudgeted row (empty goal) → nothing to exceed.
        let unbudgeted = crate::hledger::BudgetRow {
            account: "<unbudgeted>".to_string(),
            actual: vec![amount(99999)],
            goal: vec![],
        };
        assert!(over_budget_flags(&budget_report(vec![unbudgeted])).is_empty());
        // Goal in a commodity the actuals don't carry → skipped, not a false flag.
        let mismatched = crate::hledger::BudgetRow {
            account: "expenses:x".to_string(),
            actual: vec![Amount {
                commodity: "EUR".into(),
                ..amount(80000)
            }],
            goal: vec![amount(50000)],
        };
        assert!(over_budget_flags(&budget_report(vec![mismatched])).is_empty());
    }
}
