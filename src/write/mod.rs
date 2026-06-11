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
//! - **git2, not a subprocess** (see [`crate::git`]). The two-layer serialization — the
//!   in-process [`WriterLock`] plus the cross-process flock — and the per-connection epoch view
//!   live in [`ConnectionView`]; every write op runs under [`ConnectionView::guarded`], which
//!   holds both locks across the whole dedup→append→commit sequence.

pub mod format;
pub mod input;
pub mod validate;

use std::collections::HashSet;
use std::ops::AsyncFnOnce;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use crate::epoch::{CommitOid, Epoch, Stale, ToolClass};
use crate::git::{GitError, GitRepo};
use crate::hledger::{Hledger, HledgerError, Transaction};

use format::{EntryPosting, render_entry};
use input::TransactionInput;

/// The hledger version the write path hard-gates on (CLAUDE.md: pinned 1.52).
use crate::hledger::PINNED_VERSION;

/// The identity+epoch half of any write outcome: what was written and which commit it landed in.
/// Returned by declare/tombstone ops. Transaction ops return [`WriteOutcome`], which composes
/// this with dedup tracking.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    /// Stable identifier: the transaction's `id:` UUID, or the account/commodity name.
    pub id: String,
    /// The new (or current, for a dedup) `HEAD` commit oid.
    pub commit: CommitOid,
}

/// Outcome of a successful transaction write (`post`, `void`, `update`). Composes
/// [`CommitOutcome`] with idempotency tracking.
#[derive(Debug, Clone)]
pub struct WriteOutcome {
    pub base: CommitOutcome,
    /// `true` when an idempotent retry matched an existing transaction (no new commit).
    pub deduped: bool,
}

impl From<CommitOutcome> for WriteOutcome {
    fn from(base: CommitOutcome) -> Self {
        WriteOutcome {
            base,
            deduped: false,
        }
    }
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
    /// A **decide** call built on a stale read (epoch CAS, M3). Always recoverable: re-read,
    /// then retry — nothing is held, so progress is guaranteed (C-5).
    #[error("{0}")]
    StaleEpoch(Stale),
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

/// Name of the cross-process write lockfile, beside the journal. Lock state lives in the kernel
/// (advisory `flock`), not the file contents — the file itself is just an anchor and is never
/// removed (deleting a lockfile is racy).
const LOCK_FILE: &str = ".hledger-mcp.lock";

/// Acquire the **cross-process** write lock for the journal's directory (advisory exclusive
/// file lock via std's `File::lock`, blocking until free). stdio multi-client =
/// multi-*process* — Desktop, Cowork, and Claude Code each spawn their own server on the same
/// journal — so the in-process mutex alone cannot serialize writers. Released when the returned
/// handle drops (file close releases the lock).
///
/// The blocking acquisition runs on the blocking pool so a contended lock never stalls the
/// async runtime.
async fn acquire_write_flock(dir: &Path) -> Result<std::fs::File, WriteError> {
    let path = dir.join(LOCK_FILE);
    tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let file = std::fs::File::create(&path)?;
        // std file locking (stable since 1.89): advisory, exclusive, released on drop/close.
        file.lock()?;
        Ok(file)
    })
    .await
    .map_err(|e| WriteError::Internal(format!("flock task: {e}")))?
    .map_err(|e| WriteError::Internal(format!("acquire write lock: {e}")))
}

/// The current epoch: `HEAD` of the journal's repo (`Epoch(None)` when the repo or its first
/// commit doesn't exist yet). Sampled fresh on every call — the epoch changes with every write,
/// so it must never be cached process-wide (unlike the hledger *version*, which is
/// process-constant).
pub fn current_epoch(journal: &Path) -> Result<Epoch, WriteError> {
    match GitRepo::open(&repo_dir(journal))? {
        Some(repo) => Ok(Epoch::new(repo.head_oid()?)),
        None => Ok(Epoch::new(None)),
    }
}

/// Proof that the write locks are held and the epoch CAS ran. Private unit field — only
/// [`ConnectionView::guarded`] / [`guarded_once`] can mint one, making "every write goes through
/// the gate" a compile-time invariant rather than a convention.
struct WriteGuard(());

/// The execution context passed into every write operation: the hledger adapter (which carries
/// the journal path) plus the [`WriteGuard`] that proves the locks are held. The only way to
/// obtain one is through [`ConnectionView::guarded`] or [`guarded_once`]; the private fields
/// make outside construction a compile error.
///
/// Replaces the `(_proof: &WriteGuard, hledger: &Hledger)` two-arg pattern: write ops take
/// `ctx: &WriteContext<'_>` and access `ctx.hledger` and `ctx.journal()` directly.
pub struct WriteContext<'hledger> {
    pub(crate) hledger: &'hledger Hledger,
    _guard: WriteGuard,
}

impl WriteContext<'_> {
    /// The journal path. [`gate`] already verified it is `Some`; the `expect` is unreachable.
    pub(crate) fn journal(&self) -> &Path {
        self.hledger
            .journal_path()
            .expect("gate verified journal is configured")
    }
}

/// The **process-wide** half of the single-writer invariant: the in-process serializing write
/// mutex for one journal (the cross-process half is the flock inside
/// [`ConnectionView::guarded`]). Cheap to clone — clones share the same underlying mutex, so
/// every [`ConnectionView`] built from the same `WriterLock` contends on one lock. One per
/// process/journal: over stdio that is one per server; a future multi-connection transport
/// (HTTP, M6) clones the one `WriterLock` into each connection's view.
#[derive(Clone, Default)]
pub struct WriterLock(Arc<tokio::sync::Mutex<()>>);

/// One connection's view of the ledger: its **last-seen epoch** (this connection's entry in
/// the CAS directory) paired at construction with a shared handle to the process-wide
/// [`WriterLock`]. The pairing is the point — the two halves have different cardinalities
/// (per-process vs per-connection), and nesting the shared handle inside the per-connection
/// struct makes it impossible to combine one connection's lock with another's view.
///
/// Both M3 ordering disciplines live here, as the only entry points that touch `last_seen`:
/// [`Self::guarded`] (the write path) and [`Self::grounded_read`] (the read path).
pub struct ConnectionView {
    writer: WriterLock,
    /// The `HEAD` this connection last **read** (`None` = never read). Decide calls are
    /// CAS-checked against it *inside* the write locks; reads bump it (pre-read sample,
    /// success only); a write bumps it only when it was already current ([`Self::guarded`]).
    last_seen: tokio::sync::Mutex<Option<Epoch>>,
}

impl Default for ConnectionView {
    /// A standalone view: fresh [`WriterLock`], never-read last-seen. Right for stdio (one
    /// connection per process) and for one-shot callers; a multi-connection transport instead
    /// shares one `WriterLock` across views via [`Self::new`].
    fn default() -> Self {
        Self::new(WriterLock::default())
    }
}

impl ConnectionView {
    /// A view for one connection over the process-wide `writer` lock (clone the same
    /// [`WriterLock`] into every connection's view).
    pub fn new(writer: WriterLock) -> Self {
        Self {
            writer,
            last_seen: tokio::sync::Mutex::new(None),
        }
    }

    /// This connection's last-seen epoch (for `status`/diagnostics).
    pub async fn last_seen(&self) -> Option<Epoch> {
        self.last_seen.lock().await.clone()
    }

    /// The single write entry point (M3): every mutating tool call goes through here, declaring
    /// its [`ToolClass`]. Runs `op` with **both** write locks held — the in-process
    /// [`WriterLock`] (tasks within this process) and the cross-process flock (other server
    /// processes on the same journal) — and applies the epoch CAS for [`ToolClass::Decide`]
    /// calls **inside** those locks, so there is no check-to-commit gap (a TOCTOU would break
    /// `NoLostDecision`; see `proofs/tla/Ledger.tla`). `op` receives a [`WriteContext`] that
    /// carries the resolved journal path, the hledger adapter, and the proof that the gate ran.
    ///
    /// On success the connection's `last_seen` is bumped to the new `HEAD` **only when the
    /// write landed on top of what the connection had already seen** (`last_seen == HEAD`
    /// before the op). A writer learns nothing about *other* connections' commits from its own
    /// append succeeding — an unconditional bump would mask any write that interleaved since
    /// this connection's last read, letting a later decide pass the CAS on a belief that never
    /// saw it. (`FullyInformedDecisions` in `proofs/tla/Ledger.tla` is the formal version; the
    /// `bump` spec-mutation demonstrates the unconditional variant is unsound.)
    pub async fn guarded<T, F>(
        &self,
        hledger: &Hledger,
        class: ToolClass,
        op: F,
    ) -> Result<T, WriteError>
    where
        F: AsyncFnOnce(WriteContext<'_>) -> Result<T, WriteError>,
    {
        let journal = gate(hledger).await?;
        ensure_journal_exists(&journal)?;
        let dir = repo_dir(&journal);

        let _writer_guard = self.writer.0.lock().await;
        let _flock = acquire_write_flock(&dir).await?;

        let head_before = current_epoch(&journal)?;
        if class == ToolClass::Decide {
            let seen = self.last_seen.lock().await.clone();
            crate::epoch::check(class, seen.as_ref(), &head_before)
                .map_err(WriteError::StaleEpoch)?;
        }

        let ctx = WriteContext {
            hledger,
            _guard: WriteGuard(()),
        };
        let out = op(ctx).await?;

        // Conditional bump, still inside the locks: only a write on top of an up-to-date view
        // keeps the view up to date. (For a decide the guard above guarantees this branch.)
        // A failed re-sample must NOT fail the already-committed write: clear the view instead
        // (conservative — the connection re-reads before its next decide).
        let mut seen = self.last_seen.lock().await;
        if *seen == Some(head_before) {
            match current_epoch(&journal) {
                Ok(now) => *seen = Some(now),
                Err(err) => {
                    tracing::warn!(%err, "post-write epoch sample failed; clearing last-seen");
                    *seen = None;
                }
            }
        }
        Ok(out)
    }

    /// Run a ledger read with the grounding discipline built in: sample `HEAD` **before**
    /// invoking hledger, and bump this connection's last-seen to that pre-read sample only on
    /// success. Owning the ordering here makes it impossible for a (future) read tool to get
    /// wrong — bumping to a *post*-read `HEAD` could record an epoch newer than the data
    /// actually seen (the unsafe direction for the CAS); sampling before is conservative
    /// (worst case a spurious `STALE` re-read). Every ledger-reading tool goes through this.
    pub async fn grounded_read<T, E, F, Fut>(&self, hledger: &Hledger, op: F) -> Result<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let epoch = hledger
            .journal_path()
            .and_then(|journal| current_epoch(journal).ok());
        let out = op().await;
        if out.is_ok()
            && let Some(epoch) = epoch
        {
            *self.last_seen.lock().await = Some(epoch);
        }
        out
    }
}

/// One-shot [`ConnectionView::guarded`] on a fresh [`ConnectionView`] — for tests and
/// single-shot callers that don't hold a server's view. Still fully serialized: the
/// cross-process flock contends even between two views in one process, and a fresh
/// (never-read) last-seen makes any [`ToolClass::Decide`] call conservatively `STALE`.
pub async fn guarded_once<T, F>(hledger: &Hledger, class: ToolClass, op: F) -> Result<T, WriteError>
where
    F: AsyncFnOnce(WriteContext<'_>) -> Result<T, WriteError>,
{
    ConnectionView::default().guarded(hledger, class, op).await
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
    ctx: &WriteContext<'_>,
    addition: &str,
    commit_message: &str,
) -> Result<CommitOid, WriteError> {
    let journal = ctx.journal();
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
    let tmp = dir.join(format!("{CANDIDATE_PREFIX}{}.journal", Uuid::new_v4()));
    std::fs::write(&tmp, &candidate).map_err(WriteError::io("write candidate"))?;

    if let Err(err) = ctx.hledger.check_strict(&tmp).await {
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

/// Backslash-escape regex metacharacters so a literal string matches only itself inside an
/// hledger query. hledger's `tag:NAME=VALUE` value is an **unanchored regex**, so an unescaped
/// value would match by substring (`idem=txn-1` also matches `txn-10`) or error on a stray
/// metachar (`a(b`). Used with `^…$` anchoring by [`find_by_exact_tag`].
fn regex_escape(s: &str) -> String {
    const META: &[char] = &[
        '.', '^', '$', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\',
    ];
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if META.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Find transactions whose tag `name` equals `value` **exactly**. Anchors + escapes the hledger
/// query (whose tag-value match is an unanchored regex) and then re-checks exact equality in
/// Rust — belt-and-suspenders against any remaining regex surprise. `name` is always a literal
/// system tag (`id`/`idem`), so only the value needs escaping.
async fn find_by_exact_tag(
    hledger: &Hledger,
    name: &str,
    value: &str,
) -> Result<Vec<Transaction>, HledgerError> {
    let query = format!("tag:{name}=^{}$", regex_escape(value));
    let txns = hledger.list_transactions(&[query]).await?;
    Ok(txns
        .into_iter()
        .filter(|t| tag_value(t, name).as_deref() == Some(value))
        .collect())
}

/// Read the journal's declared account + commodity sets (require-pre-declare inputs to
/// [`validate::validate`]). A read failure is internal (the journal exists by this point).
async fn declared_sets(
    hledger: &Hledger,
) -> Result<(HashSet<String>, HashSet<String>), WriteError> {
    let accounts = hledger
        .declared_accounts()
        .await
        .map_err(|e| WriteError::Internal(format!("read declared accounts: {e}")))?
        .into_iter()
        .collect();
    let commodities = hledger
        .declared_commodities()
        .await
        .map_err(|e| WriteError::Internal(format!("read declared commodities: {e}")))?
        .into_iter()
        .collect();
    Ok((accounts, commodities))
}

/// `post_transaction`: validate → format (stamping `id:`/`idem:`) → candidate → check → swap →
/// commit. Idempotent on the `idem` key (dedup runs here, under the locks the [`WriteContext`]
/// proves are held).
pub async fn post_transaction(
    ctx: &WriteContext<'_>,
    input: TransactionInput,
) -> Result<WriteOutcome, WriteError> {
    let journal = ctx.journal();
    ensure_journal_exists(journal)?;

    let idem = input
        .idem
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // Idempotency: a prior write with this *exact* idem tag means "already done".
    let existing = find_by_exact_tag(ctx.hledger, "idem", &idem)
        .await
        .map_err(|e| WriteError::Internal(format!("idempotency query: {e}")))?;
    if let Some(prior) = existing.first() {
        let repo = GitRepo::open_or_init(&repo_dir(journal))?;
        let commit = repo.head_oid()?.unwrap_or_default();
        return Ok(WriteOutcome {
            base: CommitOutcome {
                id: tag_value(prior, "id").unwrap_or_default(),
                commit,
            },
            deduped: true,
        });
    }

    let (accounts, commodities) = declared_sets(ctx.hledger).await?;
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

    let commit = append_and_commit(ctx, &text, &format!("post id:{id}")).await?;
    Ok(WriteOutcome {
        base: CommitOutcome { id, commit },
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
    ctx: &WriteContext<'_>,
    target_id: &str,
) -> Result<WriteOutcome, WriteError> {
    let journal = ctx.journal();
    ensure_journal_exists(journal)?;

    let matches = find_by_exact_tag(ctx.hledger, "id", target_id)
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

    let commit =
        append_and_commit(ctx, &text, &format!("void reverses:{target_id} id:{id}")).await?;
    Ok(WriteOutcome {
        base: CommitOutcome { id, commit },
        deduped: false,
    })
}

/// `update_transaction`: void the target, then post a replacement — **two** transactions (the
/// append-only audit trail), not an in-place edit. Returns the new post's outcome.
///
/// The replacement is **validated before the void commits**, so a bad replacement (undeclared
/// account, unbalanced, malformed date) aborts with nothing changed — never leaving the target
/// voided-with-no-replacement. (A post-format `check` failure would still be an internal bug,
/// but validation rules out every *correctable* input error up front.)
pub async fn update_transaction(
    ctx: &WriteContext<'_>,
    target_id: &str,
    replacement: TransactionInput,
) -> Result<WriteOutcome, WriteError> {
    let (accounts, commodities) = declared_sets(ctx.hledger).await?;
    validate::validate(&replacement, &accounts, &commodities).map_err(WriteError::Input)?;

    void_transaction(ctx, target_id).await?;
    post_transaction(ctx, replacement).await
}

/// Strip newlines / `;` from text destined for a description (defensive — target text could
/// have been hand-edited into the journal).
fn sanitize(text: &str) -> String {
    text.replace(['\n', ';'], " ")
}

/// `declare_account`: append an `account <name>` directive (the require-pre-declare prerequisite
/// of posting). Idempotent at the journal level (a duplicate directive is harmless).
pub async fn declare_account(
    ctx: &WriteContext<'_>,
    name: &str,
) -> Result<CommitOutcome, WriteError> {
    let name = name.trim();
    if name.is_empty() || name.contains(['\n', ';']) || name.starts_with(':') || name.ends_with(':')
    {
        return Err(WriteError::Input(format!("invalid account name: '{name}'")));
    }
    let commit = append_and_commit(ctx, &format!("account {name}\n"), "declare account").await?;
    Ok(CommitOutcome {
        id: name.to_string(),
        commit,
    })
}

/// `declare_commodity`: append a `commodity` directive defining the symbol's display style with
/// `places` decimals. Symbols starting with a non-alphanumeric char (e.g. `$`) render on the
/// left; alphabetic codes (e.g. `USD`) on the right — matching hledger conventions.
pub async fn declare_commodity(
    ctx: &WriteContext<'_>,
    symbol: &str,
    places: u32,
) -> Result<CommitOutcome, WriteError> {
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
    let commit = append_and_commit(ctx, &directive, "declare commodity").await?;
    Ok(CommitOutcome {
        id: symbol.to_string(),
        commit,
    })
}

/// `tombstone_account`: **soft-delete** an account by appending a duplicate `account` directive
/// carrying a `tombstoned:` tag (verified on 1.52: a later duplicate directive's tag registers,
/// and `accounts --declared tag:^tombstoned$` filters on it). The account stays declared, so
/// existing references and even new postings still resolve (C-4) — nothing is ever hard-deleted.
/// Idempotent: re-tombstoning an already-tombstoned account is a no-op returning current `HEAD`.
pub async fn tombstone_account(
    ctx: &WriteContext<'_>,
    name: &str,
) -> Result<CommitOutcome, WriteError> {
    let journal = ctx.journal();
    ensure_journal_exists(journal)?;
    let name = name.trim();
    let declared: HashSet<String> = ctx
        .hledger
        .declared_accounts()
        .await
        .map_err(|e| WriteError::Internal(format!("read declared accounts: {e}")))?
        .into_iter()
        .collect();
    if !declared.contains(name) {
        return Err(WriteError::Input(format!(
            "cannot tombstone undeclared account '{name}'"
        )));
    }
    let tombstoned = ctx
        .hledger
        .tombstoned_accounts()
        .await
        .map_err(|e| WriteError::Internal(format!("read tombstoned accounts: {e}")))?;
    if tombstoned.iter().any(|a| a == name) {
        // Already tombstoned — idempotent no-op at the current epoch. HEAD must be born
        // here: a tombstone requires a prior commit, so an unborn repo is impossible.
        let epoch = current_epoch(journal)?;
        let commit = CommitOid::new(
            epoch
                .oid()
                .expect("idempotent tombstone implies a prior commit")
                .to_string(),
        );
        return Ok(CommitOutcome {
            id: name.to_string(),
            commit,
        });
    }
    let commit = append_and_commit(
        ctx,
        &format!("account {name}  ; tombstoned:\n"),
        &format!("tombstone account {name}"),
    )
    .await?;
    Ok(CommitOutcome {
        id: name.to_string(),
        commit,
    })
}

/// A one-line git/write-readiness summary for `status` (read-only — never inits a repo).
/// Dirty means **the journal itself** is uncommitted — the narrowest check that matches the
/// invariant (the M2 lesson): unrelated untracked files (the write lockfile, editor litter)
/// must not report the ledger as dirty.
pub fn git_status_line(journal: &Path) -> String {
    if !journal.exists() {
        return "git: (no journal yet — first write bootstraps it)".to_string();
    }
    match GitRepo::open(&repo_dir(journal)) {
        Ok(Some(repo)) => {
            let dirty = journal_relpath(journal)
                .ok()
                .and_then(|rel| repo.is_path_dirty(&rel).ok())
                .unwrap_or(false);
            let state = if dirty { "dirty" } else { "clean" };
            match repo.head_oid() {
                Ok(Some(oid)) => format!("git: {} ({state})", oid.short()),
                Ok(None) => format!("git: (no commits yet, {state})"),
                Err(err) => format!("git: error ({err})"),
            }
        }
        Ok(None) => "git: (journal directory is not a git repo)".to_string(),
        Err(err) => format!("git: error ({err})"),
    }
}

/// Prefix of the same-directory candidate temp files [`append_and_commit`] writes.
const CANDIDATE_PREFIX: &str = ".hledger-mcp-candidate-";

/// Remove abandoned candidate temp files (left by a crash between writing the candidate and the
/// atomic rename). Best-effort: failures are ignored. Safe at startup — no writer is running yet.
fn sweep_candidate_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(CANDIDATE_PREFIX) && name.ends_with(".journal") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Startup crash reconciliation: if the working tree **journal** is uncommitted (a crash between
/// the atomic swap and the commit), **commit it if `check --strict` passes, else restore to
/// HEAD** — so `HEAD` is always a `check`-valid journal. Returns the new commit oid if it
/// committed. Scoped to the journal path so an unrelated untracked file (e.g. a swept-too-late
/// candidate temp) never triggers a spurious empty reconcile commit.
pub async fn reconcile(hledger: &Hledger) -> Result<Option<CommitOid>, WriteError> {
    let Some(journal) = hledger.journal_path() else {
        return Ok(None);
    };
    if !journal.exists() {
        return Ok(None);
    }
    let dir = repo_dir(journal);
    // Cross-process lock: another server process may be mid-write while this one starts up; a
    // reconcile racing that write would see (and sweep/commit) its in-flight state.
    let _flock = acquire_write_flock(&dir).await?;
    sweep_candidate_temps(&dir);
    let repo = GitRepo::open_or_init(&dir)?;
    if !repo.is_path_dirty(&journal_relpath(journal)?)? {
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

    /// Build a [`WriteContext`] without going through the gate — unit tests only. The private
    /// fields are reachable from this child module; integration tests use `guarded_once`.
    fn ctx(hledger: &Hledger) -> WriteContext<'_> {
        WriteContext {
            hledger,
            _guard: WriteGuard(()),
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
    fn regex_escape_escapes_metacharacters() {
        assert_eq!(regex_escape("txn-1"), "txn-1"); // hyphen is not a metachar
        assert_eq!(regex_escape("a.b"), "a\\.b");
        assert_eq!(regex_escape("a(b)c"), "a\\(b\\)c");
        assert_eq!(regex_escape("x+y*z?"), "x\\+y\\*z\\?");
        // A UUID is left intact (no metachars).
        let uuid = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";
        assert_eq!(regex_escape(uuid), uuid);
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
        declare_commodity(&ctx(&hl), "$", 2).await.unwrap();
        declare_account(&ctx(&hl), "assets:checking").await.unwrap();
        let before = std::fs::read_to_string(&journal).unwrap();

        // Deliberately malformed (unbalanced single posting) — simulates a formatter bug. The
        // pipeline must fail closed: internal error, live journal untouched, no commit.
        let bad = "2026-01-01 bad\n    assets:checking  100.00 $\n";
        let err = append_and_commit(&ctx(&hl), bad, "should not commit")
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
        declare_commodity(&ctx(&hl), "$", 2).await.unwrap();
        let committed = GitRepo::open(dir.path())
            .unwrap()
            .unwrap()
            .head_oid()
            .unwrap();

        // Simulate a crash after the atomic swap but before the commit: a valid, uncommitted edit.
        // Dirtiness is checked journal-scoped (`is_path_dirty`): the write lockfile beside the
        // journal is untracked by design and must not register.
        let mut text = std::fs::read_to_string(&journal).unwrap();
        text.push_str("account assets:checking\n");
        std::fs::write(&journal, &text).unwrap();
        let relpath = Path::new("main.journal");
        assert!(
            GitRepo::open(dir.path())
                .unwrap()
                .unwrap()
                .is_path_dirty(relpath)
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
                .is_path_dirty(relpath)
                .unwrap(),
            "journal clean after reconcile"
        );
    }

    #[tokio::test]
    async fn dedup_distinguishes_regex_substring_idem_keys() {
        // Regression for the unanchored-regex dedup bug: idem `txn-1` is a regex substring of
        // `txn-10`. Both posts must be recorded as distinct transactions — the second must NOT
        // be silently deduped away.
        let Some(bin) = hledger_bin() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("main.journal");
        let hl = Hledger::new(bin, Some(journal.clone()));
        declare_commodity(&ctx(&hl), "$", 2).await.unwrap();
        declare_account(&ctx(&hl), "assets:checking").await.unwrap();
        declare_account(&ctx(&hl), "equity:opening").await.unwrap();

        let entry = |idem: &str| TransactionInput {
            date: "2026-01-01".into(),
            description: "x".into(),
            postings: vec![
                ("assets:checking".to_string(), Some(("10.00", "$"))),
                ("equity:opening".to_string(), None),
            ]
            .into_iter()
            .map(|(account, amt)| input::PostingInput {
                account,
                amount: amt.map(|(q, c)| input::PostingAmount {
                    quantity: q.to_string(),
                    commodity: c.to_string(),
                }),
            })
            .collect(),
            tags: vec![],
            idem: Some(idem.to_string()),
        };

        let first = post_transaction(&ctx(&hl), entry("txn-1")).await.unwrap();
        assert!(!first.deduped);
        let second = post_transaction(&ctx(&hl), entry("txn-10")).await.unwrap();
        assert!(!second.deduped, "txn-10 must not dedup against txn-1");

        // And an exact retry of txn-1 *does* dedup.
        let retry = post_transaction(&ctx(&hl), entry("txn-1")).await.unwrap();
        assert!(retry.deduped, "exact idem retry deduplicates");

        let all = hl.list_transactions(&[]).await.unwrap();
        assert_eq!(all.len(), 2, "two distinct posts recorded");
    }

    #[tokio::test]
    async fn update_with_invalid_replacement_does_not_void() {
        // Atomicity regression: a bad replacement must abort *before* the void commits, leaving
        // the original intact (not voided-with-no-replacement).
        let Some(bin) = hledger_bin() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("main.journal");
        let hl = Hledger::new(bin, Some(journal.clone()));
        declare_commodity(&ctx(&hl), "$", 2).await.unwrap();
        declare_account(&ctx(&hl), "assets:checking").await.unwrap();
        declare_account(&ctx(&hl), "equity:opening").await.unwrap();

        let mk = |postings: Vec<(&str, Option<&str>)>| TransactionInput {
            date: "2026-01-01".into(),
            description: "x".into(),
            postings: postings
                .into_iter()
                .map(|(account, amt)| input::PostingInput {
                    account: account.to_string(),
                    amount: amt.map(|q| input::PostingAmount {
                        quantity: q.to_string(),
                        commodity: "$".to_string(),
                    }),
                })
                .collect(),
            tags: vec![],
            idem: None,
        };

        let posted = post_transaction(
            &ctx(&hl),
            mk(vec![
                ("assets:checking", Some("10.00")),
                ("equity:opening", None),
            ]),
        )
        .await
        .unwrap();

        // Replacement references an UNDECLARED account → correctable input error.
        let bad = mk(vec![
            ("assets:savings", Some("10.00")),
            ("equity:opening", None),
        ]);
        let err = update_transaction(&ctx(&hl), &posted.base.id, bad)
            .await
            .expect_err("invalid replacement must fail");
        assert!(matches!(err, WriteError::Input(_)), "{err:?}");

        // The original is still the only transaction — nothing was voided.
        let all = hl.list_transactions(&[]).await.unwrap();
        assert_eq!(all.len(), 1, "no reversal posted on a failed update");
        assert!(
            !all[0].tags.iter().any(|(k, _)| k == "reverses"),
            "original not voided"
        );
    }

    #[tokio::test]
    async fn reconcile_ignores_stray_temp_and_sweeps_it() {
        // A leftover candidate temp must neither trigger a spurious reconcile commit nor linger.
        let Some(bin) = hledger_bin() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("main.journal");
        let hl = Hledger::new(bin, Some(journal.clone()));
        declare_commodity(&ctx(&hl), "$", 2).await.unwrap();
        let head_before = GitRepo::open(dir.path())
            .unwrap()
            .unwrap()
            .head_oid()
            .unwrap();

        // Drop an abandoned candidate temp (simulating a crash mid-check). The journal itself is
        // committed and clean.
        let stray = dir
            .path()
            .join(format!("{CANDIDATE_PREFIX}abandoned.journal"));
        std::fs::write(&stray, "garbage\n").unwrap();

        let committed = reconcile(&hl).await.unwrap();
        assert_eq!(committed, None, "no reconcile commit for a clean journal");
        assert_eq!(
            GitRepo::open(dir.path())
                .unwrap()
                .unwrap()
                .head_oid()
                .unwrap(),
            head_before,
            "HEAD did not advance"
        );
        assert!(!stray.exists(), "stray candidate temp was swept");
    }
}
