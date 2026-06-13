//! Budget rules (M5) — the `~` periodic-transaction file and its wholesale-replace pipeline.
//!
//! **Design (the M5 "`~`-rules vs append-only" decision):** periodic rules are *directives*,
//! not transactions — appending a revised rule for the same account/period **accumulates**
//! instead of replacing, and the reversing-entry correction idiom doesn't apply to
//! directives. So budget rules live in a dedicated [`BUDGET_FILE`] `include`d by the main
//! journal, and `budget_set` replaces that file **wholesale** inside the epoch-commit
//! pipeline (the M3 tombstone precedent: the *journal* stays append-only; budget revisions
//! are append-only at the **git** level — every revision is a commit). The main journal
//! gains one appended `include budget.journal` line the first time a budget is set, in the
//! same commit as the budget file (one validated write = one commit).
//!
//! The candidate is validated before the swap by copying the journal + candidate budget
//! into a scratch directory and running `hledger check --strict` there (`include` resolves
//! relative to the including file, so the copy validates the *candidate* pair while the
//! live files stay untouched — fail-closed, same as the append path).

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::git::GitRepo;
use crate::hledger::HledgerError;
use crate::hledger::amount::{Commodity, Quantity};

use super::{
    CANDIDATE_PREFIX, CommitOutcome, WriteContext, WriteError, build_candidate, declared_sets,
    ensure_journal_exists, journal_relpath, repo_dir,
};

/// The budget file name, beside the journal (and inside its repo).
pub const BUDGET_FILE: &str = "budget.journal";

/// The include directive appended (once) to the main journal.
const INCLUDE_LINE: &str = "include budget.journal";

/// The balancing account every budget rule posts against — declared inside the budget file
/// itself, so the file is self-contained under `check --strict`.
const BUDGET_EQUITY: &str = "equity:budget";

/// hledger periodic-rule periods we write (closed set). Doubles as the MCP arg type — the
/// advertised schema lists exactly these lowercase values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BudgetPeriod {
    Daily,
    Weekly,
    Monthly,
    Quarterly,
    Yearly,
}

impl BudgetPeriod {
    /// The hledger period expression keyword written after `~`.
    pub fn keyword(self) -> &'static str {
        match self {
            BudgetPeriod::Daily => "daily",
            BudgetPeriod::Weekly => "weekly",
            BudgetPeriod::Monthly => "monthly",
            BudgetPeriod::Quarterly => "quarterly",
            BudgetPeriod::Yearly => "yearly",
        }
    }

    /// Inverse of [`Self::keyword`] (the budget-file parser).
    fn from_keyword(s: &str) -> Option<Self> {
        match s {
            "daily" => Some(BudgetPeriod::Daily),
            "weekly" => Some(BudgetPeriod::Weekly),
            "monthly" => Some(BudgetPeriod::Monthly),
            "quarterly" => Some(BudgetPeriod::Quarterly),
            "yearly" => Some(BudgetPeriod::Yearly),
            _ => None,
        }
    }
}

impl std::fmt::Display for BudgetPeriod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.keyword())
    }
}

/// One budget rule: a per-`period` goal for one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetRule {
    pub account: String,
    pub period: BudgetPeriod,
    pub quantity: Quantity,
    pub commodity: Commodity,
}

/// Render the whole budget file from `rules`. The exact inverse of [`parse_budget_file`]
/// (round-trip property-tested) — this is a file we own outright.
pub fn render_budget_file(rules: &[BudgetRule]) -> String {
    let mut out = String::from(
        "; hledger-mcp budget — managed by budget_set; replaced wholesale (history in git).\n",
    );
    out.push_str(&format!("account {BUDGET_EQUITY}\n"));
    for rule in rules {
        out.push('\n');
        out.push_str(&format!(
            "~ {}  ; budget:\n    {}  {} {}\n    {}\n",
            rule.period.keyword(),
            rule.account,
            rule.quantity.render(),
            rule.commodity,
            BUDGET_EQUITY,
        ));
    }
    out
}

fn malformed(line: &str) -> WriteError {
    // We wrote this file; failing to parse it back is our bug (or external tampering) —
    // internal either way, never a correctable input error.
    WriteError::Internal(format!("malformed budget file line: {line:?}"))
}

/// Parse a budget file previously written by [`render_budget_file`].
pub fn parse_budget_file(text: &str) -> Result<Vec<BudgetRule>, WriteError> {
    let mut rules = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("~ ") else {
            continue; // header comment, account directive, blank, the equity posting
        };
        let keyword = rest.split_whitespace().next().unwrap_or_default();
        let period = BudgetPeriod::from_keyword(keyword).ok_or_else(|| malformed(line))?;
        let posting = lines.next().ok_or_else(|| malformed(line))?;
        let (account, amount) = posting
            .trim_start()
            .rsplit_once("  ")
            .ok_or_else(|| malformed(posting))?;
        let (qty, commodity) = amount.split_once(' ').ok_or_else(|| malformed(posting))?;
        let quantity = Quantity::parse(qty).ok_or_else(|| malformed(posting))?;
        rules.push(BudgetRule {
            account: account.to_string(),
            period,
            quantity,
            commodity: commodity.into(),
        });
    }
    Ok(rules)
}

/// Replace the rule matching `rule`'s (account, period) or append it. Returns `true` when an
/// existing rule was replaced (a revision) — the wholesale-replace semantics that motivated
/// this module: the same upsert as an *append* of `~` rules would have **accumulated** goals.
pub fn upsert_rule(rules: &mut Vec<BudgetRule>, rule: BudgetRule) -> bool {
    match rules
        .iter_mut()
        .find(|r| r.account == rule.account && r.period == rule.period)
    {
        Some(existing) => {
            *existing = rule;
            true
        }
        None => {
            rules.push(rule);
            false
        }
    }
}

/// Absolute path of the budget file beside `journal`.
pub fn budget_file_path(journal: &Path) -> PathBuf {
    repo_dir(journal).join(BUDGET_FILE)
}

/// Read + parse the current budget rules (empty when no budget file exists yet).
pub fn read_budget_rules(journal: &Path) -> Result<Vec<BudgetRule>, WriteError> {
    let path = budget_file_path(journal);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path).map_err(WriteError::io("read budget file"))?;
    parse_budget_file(&text)
}

/// What phase 1 stages for validation: the scratch dir holding the candidate pair (kept
/// alive until `check` ran), the path `check --strict` runs on, the new budget text, and the
/// candidate main journal when the `include` line still has to be added (first budget).
struct Staged {
    _scratch: tempfile::TempDir,
    check_target: PathBuf,
    new_budget: String,
    candidate_main: Option<String>,
}

/// `budget_set`: upsert one rule and swap in the re-rendered budget file — validate the
/// candidate pair in a scratch dir → atomic-rename the budget file (and the journal, when the
/// `include` line is first added) → **one** commit covering both paths.
pub async fn set_budget(
    ctx: &WriteContext<'_>,
    account: &str,
    period: BudgetPeriod,
    quantity: Quantity,
    commodity: Commodity,
) -> Result<CommitOutcome, WriteError> {
    let journal = ctx.journal().to_path_buf();

    // Require-pre-declare, exactly like a posting: the goal account and commodity must exist.
    ensure_journal_exists(&journal)?;
    let (accounts, commodities) = declared_sets(ctx.hledger).await?;
    if !accounts.contains(account) {
        return Err(WriteError::Input(format!(
            "account not declared: '{account}' — declare it first with declare_account"
        )));
    }
    if !commodities.contains(&commodity) {
        return Err(WriteError::Input(format!(
            "commodity not declared: '{commodity}' — declare it first with declare_commodity"
        )));
    }

    // Phase 1 (blocking pool): build the candidate pair and stage it for validation.
    let staged = {
        let journal = journal.clone();
        let account = account.to_string();
        let commodity = commodity.clone();
        tokio::task::spawn_blocking(move || -> Result<Staged, WriteError> {
            let live_main =
                std::fs::read_to_string(&journal).map_err(WriteError::io("read journal"))?;
            let mut rules = read_budget_rules(&journal)?;
            upsert_rule(
                &mut rules,
                BudgetRule {
                    account,
                    period,
                    quantity,
                    commodity,
                },
            );
            let new_budget = render_budget_file(&rules);
            let has_include = live_main.lines().any(|l| l.trim() == INCLUDE_LINE);
            let candidate_main =
                (!has_include).then(|| build_candidate(&live_main, &format!("{INCLUDE_LINE}\n")));

            let scratch = tempfile::tempdir().map_err(WriteError::io("create scratch dir"))?;
            let check_target = scratch.path().join(journal_relpath(&journal)?);
            std::fs::write(
                &check_target,
                candidate_main.as_deref().unwrap_or(&live_main),
            )
            .map_err(WriteError::io("write scratch journal"))?;
            std::fs::write(scratch.path().join(BUDGET_FILE), &new_budget)
                .map_err(WriteError::io("write scratch budget"))?;
            Ok(Staged {
                _scratch: scratch,
                check_target,
                new_budget,
                candidate_main,
            })
        })
        .await
        .map_err(|e| WriteError::Internal(format!("budget candidate task: {e}")))??
    };

    if let Err(err) = ctx.hledger.check_strict(&staged.check_target).await {
        let detail = match err {
            HledgerError::NonZero { stderr, .. } => stderr,
            other => other.to_string(),
        };
        tracing::error!(
            check = %detail,
            "internal error: hledger check --strict rejected a budget file we generated"
        );
        return Err(WriteError::Internal(format!(
            "hledger check --strict rejected our generated budget file (formatter bug):\n{detail}"
        )));
    }

    // Phase 2 (blocking pool): atomic swaps, then one commit covering both paths.
    let dir = repo_dir(&journal);
    let journal_rel = journal_relpath(&journal)?;
    let message = format!(
        "budget: set {account} {period} = {} {commodity}",
        quantity.render()
    );
    let commit = tokio::task::spawn_blocking(move || -> Result<_, WriteError> {
        let swap = |target: &Path, content: &str| -> Result<(), WriteError> {
            let tmp = dir.join(format!("{CANDIDATE_PREFIX}{}.journal", Uuid::new_v4()));
            std::fs::write(&tmp, content).map_err(WriteError::io("write candidate"))?;
            std::fs::rename(&tmp, target).map_err(WriteError::io("atomic replace"))
        };
        swap(&budget_file_path(&journal), &staged.new_budget)?;
        if let Some(main) = &staged.candidate_main {
            swap(&journal, main)?;
        }
        let repo = GitRepo::open_or_init(&dir)?;
        Ok(repo.commit_paths(&[&journal_rel, Path::new(BUDGET_FILE)], &message)?)
    })
    .await
    .map_err(|e| WriteError::Internal(format!("budget commit task: {e}")))??;

    Ok(CommitOutcome {
        id: account.to_string(),
        commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(account: &str, period: BudgetPeriod, mantissa: i128) -> BudgetRule {
        BudgetRule {
            account: account.to_string(),
            period,
            quantity: Quantity::new(mantissa, 2),
            commodity: "$".into(),
        }
    }

    #[test]
    fn renders_the_exact_file_shape() {
        let text = render_budget_file(&[rule(
            "expenses:construction:plumbing",
            BudgetPeriod::Monthly,
            50000,
        )]);
        assert_eq!(
            text,
            "; hledger-mcp budget — managed by budget_set; replaced wholesale (history in git).\n\
             account equity:budget\n\
             \n\
             ~ monthly  ; budget:\n    \
                 expenses:construction:plumbing  500.00 $\n    \
                 equity:budget\n"
        );
    }

    #[test]
    fn round_trips_through_parse() {
        let rules = vec![
            rule(
                "expenses:construction:plumbing",
                BudgetPeriod::Monthly,
                50000,
            ),
            rule(
                "expenses:professional - Bob Engineer",
                BudgetPeriod::Quarterly,
                250000,
            ),
        ];
        let parsed = parse_budget_file(&render_budget_file(&rules)).expect("parse back");
        assert_eq!(parsed, rules);
    }

    #[test]
    fn parse_rejects_tampered_lines() {
        assert!(parse_budget_file("~ fortnightly  ; budget:\n    a:b  1.00 $\n").is_err());
        assert!(
            parse_budget_file("~ monthly  ; budget:\n").is_err(),
            "missing posting"
        );
        assert!(
            parse_budget_file("~ monthly  ; budget:\n    no-double-space 1.00 $\n").is_err(),
            "posting without the account/amount separator"
        );
        assert!(
            parse_budget_file("~ monthly  ; budget:\n    a:b  one $\n").is_err(),
            "non-decimal quantity"
        );
    }

    #[test]
    fn upsert_replaces_matching_account_and_period_only() {
        let mut rules = vec![rule("a:b", BudgetPeriod::Monthly, 100)];
        // Same account, different period → appended, not replaced.
        assert!(!upsert_rule(
            &mut rules,
            rule("a:b", BudgetPeriod::Yearly, 200)
        ));
        assert_eq!(rules.len(), 2);
        // Same (account, period) → replaced in place.
        assert!(upsert_rule(
            &mut rules,
            rule("a:b", BudgetPeriod::Monthly, 999)
        ));
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].quantity, Quantity::new(999, 2));
    }

    use proptest::prelude::*;
    proptest! {
        /// Any set of rules over safe account names round-trips render → parse exactly.
        #[test]
        fn budget_file_round_trips(
            mantissas in proptest::collection::vec(0i64..1_000_000_000, 1..6),
        ) {
            let periods = [
                BudgetPeriod::Daily, BudgetPeriod::Weekly, BudgetPeriod::Monthly,
                BudgetPeriod::Quarterly, BudgetPeriod::Yearly,
            ];
            let rules: Vec<BudgetRule> = mantissas.iter().enumerate().map(|(i, m)| BudgetRule {
                account: format!("expenses:item {i}"),
                period: periods[i % periods.len()],
                quantity: Quantity::new(i128::from(*m), 2),
                commodity: "$".into(),
            }).collect();
            let parsed = parse_budget_file(&render_budget_file(&rules)).expect("round trip");
            prop_assert_eq!(parsed, rules);
        }
    }

    /// Resolve a runnable hledger for the pipeline tests, else `None` (test skips).
    fn hledger_bin() -> Option<String> {
        let runnable = |bin: &str| {
            std::process::Command::new(bin)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        match std::env::var("HLEDGER_EXECUTABLE_PATH") {
            Ok(p) if !p.is_empty() && runnable(&p) => Some(p),
            _ => runnable("hledger").then(|| "hledger".to_string()),
        }
    }

    /// The full `set_budget` pipeline against real hledger: require-pre-declare, the
    /// one-time `include` line, upsert-as-replace, the multi-path commit, and a clean tree.
    #[tokio::test]
    async fn set_budget_pipeline_includes_replaces_and_commits() {
        use crate::epoch::ToolClass;
        use crate::write::{self, WriteError, guarded_once};
        let Some(bin) = hledger_bin() else { return };
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = dir.path().join("main.journal");
        let hledger = crate::hledger::Hledger::new(bin, Some(journal.clone()));

        guarded_once(&hledger, ToolClass::Record, async |ctx| {
            write::declare_commodity(&ctx, "$", 2).await?;
            write::declare_account(&ctx, "expenses:a").await
        })
        .await
        .expect("bootstrap declarations");

        // Undeclared goal account → correctable input error, nothing written.
        let err = guarded_once(&hledger, ToolClass::Record, async |ctx| {
            set_budget(
                &ctx,
                "expenses:nope",
                BudgetPeriod::Monthly,
                Quantity::new(100, 2),
                "$".into(),
            )
            .await
        })
        .await
        .expect_err("undeclared account must be rejected");
        assert!(matches!(err, WriteError::Input(_)), "{err}");
        assert!(
            !budget_file_path(&journal).exists(),
            "no budget file on rejection"
        );

        let set = |mantissa: i128, period: BudgetPeriod| {
            let hledger = hledger.clone();
            async move {
                guarded_once(&hledger, ToolClass::Record, async |ctx| {
                    set_budget(
                        &ctx,
                        "expenses:a",
                        period,
                        Quantity::new(mantissa, 2),
                        "$".into(),
                    )
                    .await
                })
                .await
            }
        };

        let out = set(50000, BudgetPeriod::Monthly).await.expect("first set");
        assert_eq!(out.id, "expenses:a");
        let main = std::fs::read_to_string(&journal).expect("read main");
        assert_eq!(
            main.matches("include budget.journal").count(),
            1,
            "include appended once: {main}"
        );
        assert_eq!(
            read_budget_rules(&journal).expect("rules")[0].quantity,
            Quantity::new(50000, 2)
        );

        // Same (account, period) again → REPLACED (the design point), include not duplicated.
        set(100000, BudgetPeriod::Monthly).await.expect("replace");
        let rules = read_budget_rules(&journal).expect("rules");
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert_eq!(rules[0].quantity, Quantity::new(100000, 2));
        let main = std::fs::read_to_string(&journal).expect("read main");
        assert_eq!(main.matches("include budget.journal").count(), 1);

        // A different period for the same account → appended.
        set(900000, BudgetPeriod::Yearly).await.expect("append");
        assert_eq!(read_budget_rules(&journal).expect("rules").len(), 2);

        // Everything committed: neither the journal nor the budget file is dirty.
        let repo = crate::git::GitRepo::open(dir.path())
            .expect("open repo")
            .expect("repo exists");
        assert!(!repo.is_path_dirty(Path::new("main.journal")).unwrap());
        assert!(!repo.is_path_dirty(Path::new(BUDGET_FILE)).unwrap());
    }
}
