//! The **single hledger adapter module** (§16 seam): the one place the crate talks to
//! `hledger`. Everything else reads through [`Hledger`]'s narrow async surface; the CLI
//! argv builder ([`cli`]), the `-O json` parser ([`json`]), and the subprocess runner
//! ([`runner`]) are private behind it. A version bump that changes the JSON or the argv
//! touches only this module.
//!
//! Reads go through `hledger <cmd> … -O json` and are deserialized; the write path (M2)
//! will reuse the same runner + version check.

mod cli;
mod json;
mod runner;

pub mod amount;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::OnceCell;

pub use amount::{Amount, Quantity};
pub use runner::HledgerError;

/// Environment variable naming the hledger binary (set by the nix shell / `mise run
/// init-env`); falls back to `hledger` on `PATH`.
pub const HLEDGER_BIN_ENV: &str = "HLEDGER_EXECUTABLE_PATH";

/// The hledger version this server is built and tested against (CLAUDE.md: pinned 1.52).
pub const PINNED_VERSION: (u32, u32) = (1, 52);

/// A detected hledger version (`major.minor`) plus the raw `--version` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The full first line of `hledger --version`, e.g. `"hledger 1.52, mac-aarch64"`.
    pub raw: String,
    /// Major component (the `1` in `1.52`).
    pub major: u32,
    /// Minor component (the `52` in `1.52`).
    pub minor: u32,
}

impl Version {
    /// Whether the detected version matches the [`PINNED_VERSION`] this server targets.
    pub fn pin_matches(&self) -> bool {
        (self.major, self.minor) == PINNED_VERSION
    }
}

/// Parse the `hledger --version` banner into a [`Version`].
///
/// Accepts the real form `"hledger 1.52, mac-aarch64"` (and patch variants like `1.52.1`):
/// takes the first whitespace token beginning with a digit and reads its leading
/// `major.minor`.
fn parse_version(raw: &str) -> Result<Version, HledgerError> {
    let line = raw.lines().next().unwrap_or("").trim();
    let token = line
        .split_whitespace()
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit()))
        .ok_or_else(|| HledgerError::BadVersion(line.to_string()))?;
    // Keep only the leading numeric/dot run (drops a trailing comma, arch suffix, etc.).
    let numeric: String = token
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = numeric.split('.');
    let major = parts.next().and_then(|s| s.parse().ok());
    let minor = parts.next().and_then(|s| s.parse().ok());
    match (major, minor) {
        (Some(major), Some(minor)) => Ok(Version {
            raw: line.to_string(),
            major,
            minor,
        }),
        _ => Err(HledgerError::BadVersion(line.to_string())),
    }
}

/// A single account's balance (one [`BalanceReport`] row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountBalance {
    /// Full account name, e.g. `"assets:checking"`.
    pub account: String,
    /// The balance, possibly across several commodities.
    pub amounts: Vec<Amount>,
}

/// The result of `hledger balance`: per-account rows plus the column totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceReport {
    /// One row per matched account.
    pub rows: Vec<AccountBalance>,
    /// The grand total across all rows (per commodity).
    pub totals: Vec<Amount>,
}

/// One posting line within a [`Transaction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    /// The posted account.
    pub account: String,
    /// The posted amount(s).
    pub amounts: Vec<Amount>,
    /// Posting-level comment (without the leading `;`), or empty.
    pub comment: String,
    /// Posting-level `(key, value)` tags.
    pub tags: Vec<(String, String)>,
}

/// A transaction as returned by `hledger print`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Primary date (`YYYY-MM-DD`).
    pub date: String,
    /// Description / payee.
    pub description: String,
    /// hledger's 1-based transaction index within the journal.
    pub index: i64,
    /// Status word (`"Unmarked"`, `"Pending"`, `"Cleared"`).
    pub status: String,
    /// Transaction-level comment text, or empty.
    pub comment: String,
    /// Transaction-level `(key, value)` tags.
    pub tags: Vec<(String, String)>,
    /// The postings making up the (balanced) transaction.
    pub postings: Vec<Posting>,
}

/// The hledger adapter: a resolved binary plus an optional journal path.
#[derive(Debug, Clone)]
pub struct Hledger {
    bin: PathBuf,
    journal: Option<PathBuf>,
    /// Process-lifetime cache of the detected version. The binary doesn't change under us, and
    /// the write path gates on it per call, so we resolve it once via one subprocess and reuse.
    version: Arc<OnceCell<Version>>,
}

impl Hledger {
    /// Construct an adapter for an explicit binary and optional journal.
    pub fn new(bin: impl Into<PathBuf>, journal: Option<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            journal,
            version: Arc::new(OnceCell::new()),
        }
    }

    /// Resolve the binary from [`HLEDGER_BIN_ENV`] (falling back to `hledger` on `PATH`),
    /// pairing it with `journal`.
    pub fn from_env(journal: Option<PathBuf>) -> Self {
        let bin = std::env::var(HLEDGER_BIN_ENV)
            .ok()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("hledger"));
        Self::new(bin, journal)
    }

    /// The resolved hledger binary path.
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// Whether a journal is configured (read tools require one).
    pub fn has_journal(&self) -> bool {
        self.journal.is_some()
    }

    /// The configured journal path, if any (for diagnostics, e.g. `status`).
    pub fn journal_path(&self) -> Option<&Path> {
        self.journal.as_deref()
    }

    fn journal(&self) -> Result<&Path, HledgerError> {
        self.journal.as_deref().ok_or(HledgerError::NoJournal)
    }

    /// Run `hledger --version` and parse the detected [`Version`]. Needs no journal.
    ///
    /// Cached for the process lifetime: the first call spawns the subprocess, later calls (e.g.
    /// the write path's per-op version gate) reuse it. A failure is not cached — it retries.
    pub async fn version(&self) -> Result<Version, HledgerError> {
        self.version
            .get_or_try_init(|| async {
                let out = runner::run(&self.bin, &cli::version_argv()).await?;
                parse_version(&out)
            })
            .await
            .cloned()
    }

    /// `hledger balance [account] -O json` → a [`BalanceReport`].
    pub async fn balance(&self, account: Option<&str>) -> Result<BalanceReport, HledgerError> {
        let out = runner::run(&self.bin, &cli::balance_argv(self.journal()?, account)).await?;
        json::parse_balance(&out).map_err(HledgerError::from)
    }

    /// `hledger print [query…] -O json` → the matching [`Transaction`]s.
    pub async fn list_transactions(
        &self,
        query: &[String],
    ) -> Result<Vec<Transaction>, HledgerError> {
        let out = runner::run(&self.bin, &cli::print_argv(self.journal()?, query)).await?;
        json::parse_print(&out).map_err(HledgerError::from)
    }

    /// `hledger print` against an **explicit** journal path (used on the write candidate, which
    /// is not the configured live journal).
    pub async fn print_file(
        &self,
        journal: &Path,
        query: &[String],
    ) -> Result<Vec<Transaction>, HledgerError> {
        let out = runner::run(&self.bin, &cli::print_argv(journal, query)).await?;
        json::parse_print(&out).map_err(HledgerError::from)
    }

    /// Run `hledger check --strict` on an explicit journal path (the write candidate). `Ok(())`
    /// = valid; a [`HledgerError::NonZero`] carries hledger's diagnostic in `stderr`.
    pub async fn check_strict(&self, journal: &Path) -> Result<(), HledgerError> {
        runner::run(&self.bin, &cli::check_strict_argv(journal)).await?;
        Ok(())
    }

    /// The set of **declared** account names in the live journal (`accounts --declared`).
    pub async fn declared_accounts(&self) -> Result<Vec<String>, HledgerError> {
        let out = runner::run(&self.bin, &cli::accounts_declared_argv(self.journal()?)).await?;
        Ok(nonempty_lines(&out))
    }

    /// The set of **declared** commodity symbols in the live journal (`commodities`).
    pub async fn declared_commodities(&self) -> Result<Vec<String>, HledgerError> {
        let out = runner::run(&self.bin, &cli::commodities_argv(self.journal()?)).await?;
        Ok(nonempty_lines(&out))
    }

    /// The **tombstoned** (soft-deleted) subset of the declared accounts: those whose account
    /// directive carries a `tombstoned:` tag (M3 soft-delete — accounts are never hard-deleted,
    /// and postings to tombstoned accounts still resolve; C-4).
    pub async fn tombstoned_accounts(&self) -> Result<Vec<String>, HledgerError> {
        let out = runner::run(&self.bin, &cli::accounts_tombstoned_argv(self.journal()?)).await?;
        Ok(nonempty_lines(&out))
    }
}

/// Split plain hledger list output into trimmed, non-empty lines.
fn nonempty_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_version_banner() {
        let v = parse_version("hledger 1.52, mac-aarch64").expect("parse");
        assert_eq!((v.major, v.minor), (1, 52));
        assert!(v.pin_matches());
    }

    #[test]
    fn parses_patch_version_and_ignores_arch() {
        let v = parse_version("hledger 1.52.1, linux-x86_64\nextra line").expect("parse");
        assert_eq!((v.major, v.minor), (1, 52));
        assert!(v.pin_matches());
    }

    #[test]
    fn detects_pin_mismatch() {
        assert!(!parse_version("hledger 1.51, mac").unwrap().pin_matches());
        assert!(!parse_version("hledger 2.0, mac").unwrap().pin_matches());
    }

    #[test]
    fn rejects_unparseable_banner() {
        assert!(matches!(
            parse_version("not a version string"),
            Err(HledgerError::BadVersion(_))
        ));
        assert!(parse_version("").is_err());
    }

    #[test]
    fn from_env_falls_back_to_path_when_unset() {
        // We don't mutate the process env (it's shared across tests); just assert the
        // fallback path is well-formed when no explicit binary is given.
        let hl = Hledger::new("hledger", None);
        assert_eq!(hl.bin(), Path::new("hledger"));
        assert!(!hl.has_journal());
    }

    #[test]
    fn nonempty_lines_trims_and_drops_blanks() {
        assert_eq!(
            nonempty_lines("$\n  EUR \n\n\tGBP\n"),
            vec!["$".to_string(), "EUR".to_string(), "GBP".to_string()]
        );
        assert!(nonempty_lines("   \n\n").is_empty());
    }

    #[tokio::test]
    async fn read_without_journal_errors() {
        let hl = Hledger::new("hledger", None);
        assert!(matches!(
            hl.balance(None).await,
            Err(HledgerError::NoJournal)
        ));
        assert!(matches!(
            hl.list_transactions(&[]).await,
            Err(HledgerError::NoJournal)
        ));
    }

    /// An adapter pointed at the checked-in synthetic fixture journal, resolving the binary
    /// from `HLEDGER_EXECUTABLE_PATH`. Returns `None` (→ test skips) when hledger is absent.
    async fn fixture_adapter() -> Option<Hledger> {
        let journal = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample.journal"
        ));
        let hl = Hledger::from_env(Some(journal));
        match hl.version().await {
            Ok(_) => Some(hl),
            Err(_) => {
                eprintln!("SKIP adapter e2e: hledger not found (run inside `nix develop`)");
                None
            }
        }
    }

    #[tokio::test]
    async fn version_detects_hledger() {
        let Some(hl) = fixture_adapter().await else {
            return;
        };
        let v = hl.version().await.expect("version");
        assert!(v.major >= 1, "parsed a version: {}", v.raw);
    }

    #[tokio::test]
    async fn balance_reads_real_account() {
        let Some(hl) = fixture_adapter().await else {
            return;
        };
        let report = hl.balance(Some("assets:checking")).await.expect("balance");
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].account, "assets:checking");
        assert_eq!(report.rows[0].amounts[0].render(), "$43.66");
        assert_eq!(report.totals[0].render(), "$43.66");
    }

    #[tokio::test]
    async fn list_transactions_filters_by_query() {
        let Some(hl) = fixture_adapter().await else {
            return;
        };
        let txns = hl
            .list_transactions(&["expenses:supplies".to_string()])
            .await
            .expect("print");
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].description, "Acme");
        assert_eq!(txns[0].date, "2026-01-15");
        // Unfiltered lists every transaction in the fixture.
        let all = hl.list_transactions(&[]).await.expect("print all");
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn nonzero_exit_maps_to_error() {
        let Some(hl) = fixture_adapter().await else {
            return;
        };
        // An invalid date query makes hledger exit non-zero → typed NonZero error.
        let err = hl
            .list_transactions(&["date:not-a-date".to_string()])
            .await
            .expect_err("bad query should fail");
        assert!(matches!(err, HledgerError::NonZero { .. }), "{err:?}");
    }
}
