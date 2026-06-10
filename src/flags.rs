//! Soft-invariant flags (M3, `concurrency-model.md` "Soft invariants → flags").
//!
//! Budget overruns, overdrafts, and AP-aging are **computed and surfaced** in read /
//! report output — **never enforced**: a record call is never rejected for violating
//! one (C-6). This module is the mechanism plus the one flag whose data exists in M3,
//! **overdraft**; the AP-aging flag lands with M4's `get_ap_aging`, over-budget with
//! M5's `get_budget_vs_actual`.
//!
//! Pure (report in, flags out) — unit- and mutation-testable without hledger.

use crate::hledger::BalanceReport;

/// One surfaced soft-invariant violation. Informational only, by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    /// The invariant kind, e.g. `"overdraft"` (M4/M5 add `"ap-aging"`, `"over-budget"`).
    pub kind: &'static str,
    /// The account the flag is about.
    pub account: String,
    /// Human-readable detail (rendered amounts).
    pub detail: String,
}

impl std::fmt::Display for Flag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "flag {}: {} {}", self.kind, self.account, self.detail)
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
                    kind: "overdraft",
                    account: row.account.clone(),
                    detail: format!("balance {}", amount.render()),
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
                commodity: "$".to_string(),
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
        assert_eq!(flags[0].kind, "overdraft");
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
}
