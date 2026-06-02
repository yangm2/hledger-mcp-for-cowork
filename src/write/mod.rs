//! The write path: the **only** way data enters the ledger. One validated write = one
//! `hledger check --strict`-clean candidate, atomically swapped in, = one git commit = one epoch.
//!
//! Discipline (CLAUDE.md *write-path discipline*, M2 doc):
//! - **Fail closed:** the live journal is replaced only *after* `check --strict` passes on a
//!   same-directory candidate; on any failure the live journal is byte-for-byte untouched and
//!   nothing commits.
//! - **Input vs internal:** all input is validated *before* formatting ([`validate`]), so a
//!   post-format `check` failure can only be our formatter bug — surfaced as a loud internal
//!   error with the `check` output attached (logged verbatim; logs are local, not the repo).
//! - **Require pre-declare:** accounts/commodities must already be declared (`declare_*`).
//! - **Append-only:** corrections are reversing entries (`void`); nothing is ever edited in place.
//! - **git2, not a subprocess** (see [`crate::git`]). The serializing write mutex lives in the
//!   server; the dedup→append→commit sequence here must run under it.

pub mod format;
pub mod input;
pub mod validate;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::git::{GitError, GitRepo};
use crate::hledger::{Hledger, HledgerError, Transaction};

use format::{EntryPosting, render_entry};
use input::TransactionInput;

/// The hledger version the write path hard-gates on (CLAUDE.md: pinned 1.52).
use crate::hledger::PINNED_VERSION;

/// Outcome of a successful write.
#[derive(Debug, Clone)]
pub struct WriteOutcome {
    /// The transaction's stable `id:` tag (a fresh UUID), or the account/commodity name for
    /// `declare_*`.
    pub id: String,
    /// The new `HEAD` commit oid — the epoch. (For a deduped post, the *current* HEAD.)
    pub commit: String,
    /// `true` when an idempotent retry matched an existing transaction (no new commit).
    pub deduped: bool,
}

/// Errors from the write path, partitioned by how the caller should treat them.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// Correctable bad input, caught *before* formatting — the model can fix and retry.
    #[error("input error: {0}")]
    Input(String),
    /// Refused for environment reasons (no journal configured, version-pin mismatch). Not a bug,
    /// not fixable by rephrasing the call.
    #[error("refused: {0}")]
    Refused(String),
    /// Our bug / unexpected failure (post-format `check` rejection, git/IO). Logged loudly.
    #[error("internal error: {0}")]
    Internal(String),
}

impl WriteError {
    fn io(context: &str) -> impl Fn(std::io::Error) -> WriteError + '_ {
        move |e| WriteError::Internal(format!("{context}: {e}"))
    }
}

impl From<GitError> for WriteError {
    fn from(e: GitError) -> Self {
        WriteError::Internal(format!("git: {e}"))
    }
}

/// The repo/working directory for `journal` (its parent, or `.` if it has none).
fn repo_dir(journal: &Path) -> PathBuf {
    match journal.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The journal's path relative to its repo dir (just the file name).
fn journal_relpath(journal: &Path) -> Result<PathBuf, WriteError> {
    journal
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(|| WriteError::Internal(format!("journal path has no file name: {journal:?}")))
}

/// Hard version gate + journal resolution, shared by every write op.
async fn gate(hledger: &Hledger) -> Result<PathBuf, WriteError> {
    let journal = hledger
        .journal_path()
        .ok_or_else(|| WriteError::Refused("no journal configured".to_string()))?
        .to_path_buf();
    let version = hledger
        .version()
        .await
        .map_err(|e| WriteError::Refused(format!("hledger unavailable: {e}")))?;
    if !version.pin_matches() {
        return Err(WriteError::Refused(format!(
            "refusing to write against hledger {}.{} (write path requires the pinned {}.{})",
            version.major, version.minor, PINNED_VERSION.0, PINNED_VERSION.1
        )));
    }
    Ok(journal)
}

/// Create the journal (and its directory) if missing, so reads/declared-set queries work and the
/// first write has something to append to. Does not commit — the first write's commit captures it.
fn ensure_journal_exists(journal: &Path) -> Result<(), WriteError> {
    if !journal.exists() {
        std::fs::create_dir_all(repo_dir(journal)).map_err(WriteError::io("create ledger dir"))?;
        std::fs::write(journal, "; hledger-mcp ledger\n")
            .map_err(WriteError::io("create journal"))?;
    }
    Ok(())
}

/// The core: append `addition` to the live journal, validate the candidate with `check --strict`
/// in a same-directory temp file, atomically swap it in, and commit. Fail-closed.
///
/// Assumes the version gate already passed and the input was already validated (so a `check`
/// failure here is an [`WriteError::Internal`] — our bug — with the diagnostic attached).
async fn append_and_commit(
    hledger: &Hledger,
    journal: &Path,
    addition: &str,
    commit_message: &str,
) -> Result<String, WriteError> {
    ensure_journal_exists(journal)?;
    let live = std::fs::read_to_string(journal).map_err(WriteError::io("read journal"))?;

    let mut candidate = live;
    if !candidate.is_empty() && !candidate.ends_with('\n') {
        candidate.push('\n');
    }
    candidate.push('\n');
    candidate.push_str(addition);

    // Same-directory temp so the rename is atomic (a cross-device rename fails).
    let dir = repo_dir(journal);
    let tmp = dir.join(format!(".hledger-mcp-candidate-{}.journal", Uuid::new_v4()));
    std::fs::write(&tmp, &candidate).map_err(WriteError::io("write candidate"))?;

    if let Err(err) = hledger.check_strict(&tmp).await {
        let _ = std::fs::remove_file(&tmp);
        let detail = match err {
            HledgerError::NonZero { stderr, .. } => stderr,
            other => other.to_string(),
        };
        // Loud: this is our formatter producing invalid journal text from validated input.
        tracing::error!(
            check = %detail,
            "internal error: hledger check --strict rejected journal text we generated"
        );
        return Err(WriteError::Internal(format!(
            "hledger check --strict rejected our generated journal (formatter bug):\n{detail}"
        )));
    }

    std::fs::rename(&tmp, journal).map_err(WriteError::io("atomic replace journal"))?;

    let repo = GitRepo::open_or_init(&dir)?;
    let oid = repo.commit_path(&journal_relpath(journal)?, commit_message)?;
    Ok(oid)
}

/// Read a transaction-level tag value.
fn tag_value(txn: &Transaction, key: &str) -> Option<String> {
    txn.tags
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

/// `post_transaction`: validate → format (stamping `id:`/`idem:`) → candidate → check → swap →
/// commit. Idempotent on the `idem` key (dedup runs here, under the caller's write mutex).
pub async fn post_transaction(
    hledger: &Hledger,
    input: TransactionInput,
) -> Result<WriteOutcome, WriteError> {
    let journal = gate(hledger).await?;
    ensure_journal_exists(&journal)?;

    let idem = input
        .idem
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // Idempotency: a prior write with this idem tag means "already done".
    let existing = hledger
        .list_transactions(&[format!("tag:idem={idem}")])
        .await
        .map_err(|e| WriteError::Internal(format!("idempotency query: {e}")))?;
    if let Some(prior) = existing.first() {
        let repo = GitRepo::open_or_init(&repo_dir(&journal))?;
        let commit = repo.head_oid()?.unwrap_or_default();
        return Ok(WriteOutcome {
            id: tag_value(prior, "id").unwrap_or_default(),
            commit,
            deduped: true,
        });
    }

    let accounts: HashSet<String> = hledger
        .declared_accounts()
        .await
        .map_err(|e| WriteError::Internal(format!("read declared accounts: {e}")))?
        .into_iter()
        .collect();
    let commodities: HashSet<String> = hledger
        .declared_commodities()
        .await
        .map_err(|e| WriteError::Internal(format!("read declared commodities: {e}")))?
        .into_iter()
        .collect();

    let validated =
        validate::validate(&input, &accounts, &commodities).map_err(WriteError::Input)?;

    let id = Uuid::new_v4().to_string();
    let mut tags = vec![("id".to_string(), id.clone()), ("idem".to_string(), idem)];
    tags.extend(validated.tags.iter().cloned());
    let text = render_entry(
        &validated.date,
        &validated.description,
        &validated.postings,
        &tags,
    );

    let commit = append_and_commit(hledger, &journal, &text, &format!("post id:{id}")).await?;
    Ok(WriteOutcome {
        id,
        commit,
        deduped: false,
    })
}

/// Negate a transaction's postings into reversing-entry posting lines.
fn reversal_postings(target: &Transaction) -> Vec<EntryPosting> {
    let mut postings = Vec::new();
    for posting in &target.postings {
        for amount in &posting.amounts {
            let negated =
                crate::hledger::Quantity::new(-amount.quantity.mantissa, amount.quantity.places);
            postings.push((
                posting.account.clone(),
                Some((negated, amount.commodity.clone())),
            ));
        }
    }
    postings
}

/// `void_transaction`: post a **reversing** entry (tagged `reverses:<id>`) that negates every
/// posting of the target. Append-only — the original line is never touched.
pub async fn void_transaction(
    hledger: &Hledger,
    target_id: &str,
) -> Result<WriteOutcome, WriteError> {
    let journal = gate(hledger).await?;
    ensure_journal_exists(&journal)?;

    let matches = hledger
        .list_transactions(&[format!("tag:id={target_id}")])
        .await
        .map_err(|e| WriteError::Internal(format!("lookup by id: {e}")))?;
    let target = matches
        .first()
        .ok_or_else(|| WriteError::Input(format!("no transaction with id '{target_id}'")))?;
    if tag_value(target, "reverses").is_some() {
        return Err(WriteError::Input(format!(
            "transaction '{target_id}' is itself a reversal; refusing to void a reversal"
        )));
    }

    let postings = reversal_postings(target);
    if postings.is_empty() {
        return Err(WriteError::Internal(
            "target transaction has no postings to reverse".to_string(),
        ));
    }

    let id = Uuid::new_v4().to_string();
    let idem = Uuid::new_v4().to_string();
    let description = sanitize(&format!("reversal of {}", target.description));
    let tags = vec![
        ("id".to_string(), id.clone()),
        ("idem".to_string(), idem),
        ("reverses".to_string(), target_id.to_string()),
    ];
    // Reversal carries the target's date (period-neutral void).
    let text = render_entry(&target.date, &description, &postings, &tags);

    let commit = append_and_commit(
        hledger,
        &journal,
        &text,
        &format!("void reverses:{target_id} id:{id}"),
    )
    .await?;
    Ok(WriteOutcome {
        id,
        commit,
        deduped: false,
    })
}

/// `update_transaction`: void the target, then post a replacement — **two** transactions (the
/// append-only audit trail), not an in-place edit. Returns the new post's outcome.
pub async fn update_transaction(
    hledger: &Hledger,
    target_id: &str,
    replacement: TransactionInput,
) -> Result<WriteOutcome, WriteError> {
    void_transaction(hledger, target_id).await?;
    post_transaction(hledger, replacement).await
}

/// Strip newlines / `;` from text destined for a description (defensive — target text could
/// have been hand-edited into the journal).
fn sanitize(text: &str) -> String {
    text.replace(['\n', ';'], " ")
}

/// `declare_account`: append an `account <name>` directive (the require-pre-declare prerequisite
/// of posting). Idempotent at the journal level (a duplicate directive is harmless).
pub async fn declare_account(hledger: &Hledger, name: &str) -> Result<String, WriteError> {
    let journal = gate(hledger).await?;
    let name = name.trim();
    if name.is_empty() || name.contains(['\n', ';']) || name.starts_with(':') || name.ends_with(':')
    {
        return Err(WriteError::Input(format!("invalid account name: '{name}'")));
    }
    append_and_commit(
        hledger,
        &journal,
        &format!("account {name}\n"),
        "declare account",
    )
    .await
}

/// `declare_commodity`: append a `commodity` directive defining the symbol's display style with
/// `places` decimals. Symbols starting with a non-alphanumeric char (e.g. `$`) render on the
/// left; alphabetic codes (e.g. `USD`) on the right — matching hledger conventions.
pub async fn declare_commodity(
    hledger: &Hledger,
    symbol: &str,
    places: u32,
) -> Result<String, WriteError> {
    let journal = gate(hledger).await?;
    let symbol = symbol.trim();
    if symbol.is_empty()
        || symbol.contains(['\n', ';', ' '])
        || symbol.contains(|c: char| c.is_ascii_digit())
    {
        return Err(WriteError::Input(format!(
            "invalid commodity symbol: '{symbol}'"
        )));
    }
    let sample_number = if places == 0 {
        "1000".to_string()
    } else {
        format!("1000.{}", "0".repeat(places as usize))
    };
    let left = !symbol.starts_with(|c: char| c.is_ascii_alphabetic());
    let directive = if left {
        format!("commodity {symbol}{sample_number}\n")
    } else {
        format!("commodity {sample_number} {symbol}\n")
    };
    append_and_commit(hledger, &journal, &directive, "declare commodity").await
}

/// A one-line git/write-readiness summary for `status` (read-only — never inits a repo).
pub fn git_status_line(journal: &Path) -> String {
    if !journal.exists() {
        return "git: (no journal yet — first write bootstraps it)".to_string();
    }
    match GitRepo::open(&repo_dir(journal)) {
        Ok(Some(repo)) => {
            let dirty = repo.is_dirty().unwrap_or(false);
            let state = if dirty { "dirty" } else { "clean" };
            match repo.head_oid() {
                Ok(Some(oid)) => format!("git: {} ({state})", &oid[..oid.len().min(12)]),
                Ok(None) => format!("git: (no commits yet, {state})"),
                Err(err) => format!("git: error ({err})"),
            }
        }
        Ok(None) => "git: (journal directory is not a git repo)".to_string(),
        Err(err) => format!("git: error ({err})"),
    }
}

/// Startup crash reconciliation: if the working tree journal is uncommitted (a crash between the
/// atomic swap and the commit), **commit it if `check --strict` passes, else restore to HEAD** —
/// so `HEAD` is always a `check`-valid journal. Returns the new commit oid if it committed.
pub async fn reconcile(hledger: &Hledger) -> Result<Option<String>, WriteError> {
    let Some(journal) = hledger.journal_path() else {
        return Ok(None);
    };
    if !journal.exists() {
        return Ok(None);
    }
    let repo = GitRepo::open_or_init(&repo_dir(journal))?;
    if !repo.is_dirty()? {
        return Ok(None);
    }
    match hledger.check_strict(journal).await {
        Ok(()) => {
            let oid =
                repo.commit_path(&journal_relpath(journal)?, "reconcile uncommitted journal")?;
            tracing::warn!(commit = %oid, "reconciled an uncommitted but valid journal at startup");
            Ok(Some(oid))
        }
        Err(_) => {
            repo.restore_to_head()?;
            tracing::warn!("restored an uncommitted, invalid journal to HEAD at startup");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hledger::{Amount, Posting, Quantity, Transaction};

    /// Resolve a runnable hledger for the e2e tests, else `None` (test skips).
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

    fn txn(tags: Vec<(String, String)>, postings: Vec<Posting>) -> Transaction {
        Transaction {
            date: "2026-01-01".into(),
            description: "x".into(),
            index: 1,
            status: "Unmarked".into(),
            comment: String::new(),
            tags,
            postings,
        }
    }

    fn posting(account: &str, mantissa: i128, commodity: &str) -> Posting {
        Posting {
            account: account.into(),
            amounts: vec![Amount {
                commodity: commodity.into(),
                quantity: Quantity::new(mantissa, 2),
                commodity_left: true,
                spaced: false,
            }],
            comment: String::new(),
            tags: vec![],
        }
    }

    #[test]
    fn repo_dir_and_relpath() {
        assert_eq!(
            repo_dir(Path::new("/a/b/main.journal")),
            PathBuf::from("/a/b")
        );
        assert_eq!(repo_dir(Path::new("main.journal")), PathBuf::from("."));
        assert_eq!(
            journal_relpath(Path::new("/a/b/main.journal")).unwrap(),
            PathBuf::from("main.journal")
        );
    }

    #[test]
    fn sanitize_strips_newlines_and_semicolons() {
        assert_eq!(sanitize("a;b\nc"), "a b c");
    }

    #[test]
    fn tag_value_reads_named_tag() {
        let t = txn(vec![("id".into(), "abc".into())], vec![]);
        assert_eq!(tag_value(&t, "id").as_deref(), Some("abc"));
        assert_eq!(tag_value(&t, "missing"), None);
    }

    #[test]
    fn reversal_negates_every_posting_amount() {
        let t = txn(
            vec![],
            vec![
                posting("expenses:x", 1234, "$"),
                posting("assets:c", -1234, "$"),
            ],
        );
        let rev = reversal_postings(&t);
        assert_eq!(rev.len(), 2);
        // first posting was +12.34 → reversal -12.34
        let (acct, amount) = &rev[0];
        assert_eq!(acct, "expenses:x");
        let (q, c) = amount.as_ref().unwrap();
        assert_eq!(q.render(), "-12.34");
        assert_eq!(c, "$");
        // second was -12.34 → +12.34
        assert_eq!(rev[1].1.as_ref().unwrap().0.render(), "12.34");
    }

    #[test]
    fn write_error_display_has_actionable_prefixes() {
        assert!(
            WriteError::Input("x".into())
                .to_string()
                .starts_with("input error:")
        );
        assert!(
            WriteError::Refused("x".into())
                .to_string()
                .starts_with("refused:")
        );
        assert!(
            WriteError::Internal("x".into())
                .to_string()
                .starts_with("internal error:")
        );
    }

    #[test]
    fn git_status_line_reports_no_journal_then_repo_state() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("main.journal");
        assert!(git_status_line(&journal).contains("no journal yet"));

        std::fs::write(&journal, "; j\n").unwrap();
        // Dir is not a repo yet.
        assert!(git_status_line(&journal).contains("not a git repo"));

        // Init + commit → reports a short oid and clean.
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        repo.commit_path(Path::new("main.journal"), "c").unwrap();
        let line = git_status_line(&journal);
        assert!(
            line.starts_with("git: ") && line.contains("clean"),
            "{line}"
        );
    }

    #[tokio::test]
    async fn append_and_commit_fails_closed_on_invalid_text() {
        let Some(bin) = hledger_bin() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("main.journal");
        let hl = Hledger::new(bin, Some(journal.clone()));
        // Bootstrap a committed, valid journal.
        declare_commodity(&hl, "$", 2).await.unwrap();
        declare_account(&hl, "assets:checking").await.unwrap();
        let before = std::fs::read_to_string(&journal).unwrap();

        // Deliberately malformed (unbalanced single posting) — simulates a formatter bug. The
        // pipeline must fail closed: internal error, live journal untouched, no commit.
        let bad = "2026-01-01 bad\n    assets:checking  100.00 $\n";
        let err = append_and_commit(&hl, &journal, bad, "should not commit")
            .await
            .expect_err("invalid candidate must be rejected");
        assert!(matches!(err, WriteError::Internal(_)), "{err:?}");
        assert_eq!(
            before,
            std::fs::read_to_string(&journal).unwrap(),
            "live journal must be byte-identical after a failed write"
        );
        // No stray candidate temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("candidate"))
            .collect();
        assert!(leftovers.is_empty(), "candidate temp cleaned up");
    }

    #[tokio::test]
    async fn reconcile_commits_valid_uncommitted_journal() {
        let Some(bin) = hledger_bin() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("main.journal");
        let hl = Hledger::new(bin, Some(journal.clone()));
        declare_commodity(&hl, "$", 2).await.unwrap();
        let committed = GitRepo::open(dir.path())
            .unwrap()
            .unwrap()
            .head_oid()
            .unwrap();

        // Simulate a crash after the atomic swap but before the commit: a valid, uncommitted edit.
        let mut text = std::fs::read_to_string(&journal).unwrap();
        text.push_str("account assets:checking\n");
        std::fs::write(&journal, &text).unwrap();
        assert!(
            GitRepo::open(dir.path())
                .unwrap()
                .unwrap()
                .is_dirty()
                .unwrap()
        );

        let oid = reconcile(&hl)
            .await
            .unwrap()
            .expect("reconcile commits valid tree");
        assert_ne!(Some(oid), committed, "reconcile advanced HEAD");
        assert!(
            !GitRepo::open(dir.path())
                .unwrap()
                .unwrap()
                .is_dirty()
                .unwrap(),
            "clean after reconcile"
        );
    }
}
