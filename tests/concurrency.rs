//! The M3 concurrency suite: **C-1…C-6** from `docs/development/concurrency-model.md`,
//! plus the carried M2 deferrals (crash-reconcile invalid→restore e2e; a dedicated C-3
//! monotonicity test) and the read-ordering discipline.
//!
//! These drive the **production entry points** in-process: [`write::guarded_write`] with a
//! declared [`ToolClass`] is exactly what every MCP write tool calls (and what the first
//! decide-classified domain tool will call in M4/M5 — no MCP decide tool exists yet, so C-1's
//! end-to-end variant lands there). Each "connection" below is a separate server-shaped pair of
//! (in-process write mutex, last-seen slot) over the same journal — the in-process analogue of
//! the real deployment, where every MCP host spawns its own server process on one journal.
//!
//! All tests skip gracefully when hledger is absent (mirroring `tests/smoke.rs`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hledger_mcp_for_cowork::epoch::{Epoch, ToolClass};
use hledger_mcp_for_cowork::flags;
use hledger_mcp_for_cowork::git::GitRepo;
use hledger_mcp_for_cowork::hledger::Hledger;
use hledger_mcp_for_cowork::write::{
    self, WriteError,
    input::{PostingAmount, PostingInput, TransactionInput},
};

/// Resolve a runnable hledger, else `None` (tests skip).
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

/// One "connection": its own in-process write mutex and last-seen slot, sharing the journal.
/// This is the shape [`crate::server::HledgerMcp`] holds per instance.
struct Conn {
    hledger: Hledger,
    journal: PathBuf,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    last_seen: Arc<tokio::sync::Mutex<Option<Epoch>>>,
}

impl Conn {
    fn new(bin: &str, journal: &Path) -> Self {
        Self {
            hledger: Hledger::new(bin, Some(journal.to_path_buf())),
            journal: journal.to_path_buf(),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            last_seen: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// A read, as the server performs it: sample HEAD **before** the hledger read, bump
    /// last-seen to that pre-sample after the read succeeds.
    async fn read(&self) {
        let epoch = write::current_epoch(&self.journal).expect("sample epoch");
        self.hledger
            .list_transactions(&[])
            .await
            .expect("read journal");
        *self.last_seen.lock().await = Some(epoch);
    }

    /// A guarded write of the given class posting `input` — the production dispatch path.
    async fn guarded_post(
        &self,
        class: ToolClass,
        input: TransactionInput,
    ) -> Result<write::WriteOutcome, WriteError> {
        write::guarded_write(
            &self.hledger,
            &self.write_lock,
            &self.last_seen,
            class,
            || write::post_transaction(&self.hledger, input),
        )
        .await
    }
}

fn txn(description: &str, to: &str, amount: &str, idem: Option<&str>) -> TransactionInput {
    TransactionInput {
        date: "2026-01-01".into(),
        description: description.into(),
        postings: vec![
            PostingInput {
                account: to.into(),
                amount: Some(PostingAmount {
                    quantity: amount.into(),
                    commodity: "$".into(),
                }),
            },
            PostingInput {
                account: "equity:opening".into(),
                amount: None,
            },
        ],
        tags: vec![],
        idem: idem.map(str::to_string),
    }
}

/// Bootstrap a journal with the standard test declarations, returning (tempdir, journal path).
/// Goes through the production write path (each declare = one commit).
async fn setup(bin: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("main.journal");
    let hl = Hledger::new(bin, Some(journal.clone()));
    write::declare_commodity(&hl, "$", 2).await.expect("$");
    for account in ["assets:checking", "expenses:misc", "equity:opening"] {
        write::declare_account(&hl, account).await.expect(account);
    }
    (dir, journal)
}

/// **C-1** — a decide call with `last-seen < HEAD` is rejected `STALE`; a fresh read then
/// retry succeeds. (In-process through `guarded_write` + `ToolClass::Decide`: the production
/// code path; the e2e variant arrives with the first decide-classified MCP tool, M4/M5.)
#[tokio::test]
async fn c1_stale_decide_rejected_then_read_retry_succeeds() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let a = Conn::new(&bin, &journal);
    let b = Conn::new(&bin, &journal);

    // A grounds itself at the current epoch.
    a.read().await;

    // B (another connection/process) advances the epoch with a record write.
    b.guarded_post(
        ToolClass::Record,
        txn("b-post", "expenses:misc", "1.00", None),
    )
    .await
    .expect("record write at any epoch");

    // A's decide is now built on a stale read → rejected STALE, journal untouched by it.
    let err = a
        .guarded_post(
            ToolClass::Decide,
            txn("a-decide", "expenses:misc", "2.00", None),
        )
        .await
        .expect_err("stale decide must be rejected");
    let WriteError::StaleEpoch(stale) = &err else {
        panic!("expected StaleEpoch, got {err:?}");
    };
    assert!(stale.to_string().contains("Re-read"), "carries the hint");
    let a_conn = Conn::new(&bin, &journal);
    let all = { a_conn.hledger.list_transactions(&[]).await.expect("list") };
    assert_eq!(all.len(), 1, "the rejected decide posted nothing");

    // Re-read → retry succeeds (C-5's mechanism).
    a.read().await;
    a.guarded_post(
        ToolClass::Decide,
        txn("a-decide", "expenses:misc", "2.00", None),
    )
    .await
    .expect("fresh decide succeeds");
}

/// **C-1 (partition half)** — record calls are *never* rejected STALE, even when behind.
#[tokio::test]
async fn c1_record_never_stale() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let a = Conn::new(&bin, &journal);
    let b = Conn::new(&bin, &journal);

    a.read().await;
    b.guarded_post(ToolClass::Record, txn("b1", "expenses:misc", "1.00", None))
        .await
        .expect("b posts");
    // A is behind, but a record write is epoch-free.
    a.guarded_post(ToolClass::Record, txn("a1", "expenses:misc", "2.00", None))
        .await
        .expect("record at a stale epoch still succeeds");
    // And a connection that never read at all can record too.
    let c = Conn::new(&bin, &journal);
    c.guarded_post(ToolClass::Record, txn("c1", "expenses:misc", "3.00", None))
        .await
        .expect("record without any prior read succeeds");
}

/// **C-2** — a post retried with a duplicate `idem:` key yields exactly one transaction,
/// re-asserted under the partition (through `guarded_write`).
#[tokio::test]
async fn c2_idempotent_posts_under_the_partition() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let a = Conn::new(&bin, &journal);

    let first = a
        .guarded_post(
            ToolClass::Record,
            txn("x", "expenses:misc", "1.00", Some("once")),
        )
        .await
        .expect("first");
    assert!(!first.deduped);
    let retry = a
        .guarded_post(
            ToolClass::Record,
            txn("x", "expenses:misc", "1.00", Some("once")),
        )
        .await
        .expect("retry");
    assert!(retry.deduped, "duplicate idem deduplicates");
    assert_eq!(retry.id, first.id, "dedup returns the original's id");

    let all = a.hledger.list_transactions(&[]).await.expect("list");
    assert_eq!(all.len(), 1, "exactly one transaction recorded");
}

/// **C-3 (dedicated)** — the epoch is monotonic: every validated write advances `HEAD` to a
/// **new** commit whose parent is the previous `HEAD` (a strictly linear, growing chain), and
/// reads never move it. (M2 demonstrated this incidentally; this is the carried dedicated test.)
#[tokio::test]
async fn c3_epoch_monotonic_writes_advance_reads_dont() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let a = Conn::new(&bin, &journal);

    let mut epochs = vec![
        write::current_epoch(&journal)
            .expect("epoch")
            .oid()
            .expect("born after setup")
            .to_string(),
    ];
    for i in 0..3 {
        let outcome = a
            .guarded_post(
                ToolClass::Record,
                txn(&format!("w{i}"), "expenses:misc", "1.00", None),
            )
            .await
            .expect("write");
        epochs.push(outcome.commit.clone());
    }

    // Strictly advancing: all distinct, and each commit's parent is its predecessor.
    let unique: std::collections::HashSet<&String> = epochs.iter().collect();
    assert_eq!(unique.len(), epochs.len(), "every write = a fresh epoch");
    let repo = GitRepo::open(journal.parent().unwrap())
        .expect("open")
        .expect("repo");
    assert_eq!(
        repo.head_oid().expect("head").as_deref(),
        Some(epochs.last().unwrap().as_str()),
        "HEAD is the last write's commit"
    );

    // Reads do not move the epoch.
    a.read().await;
    a.hledger.balance(None).await.expect("balance read");
    assert_eq!(
        write::current_epoch(&journal).expect("epoch").oid(),
        Some(epochs.last().unwrap().as_str()),
        "reads never move HEAD"
    );
}

/// **C-4** — an account is soft-deleted (tombstoned), never hard-deleted: it stays declared,
/// it is queryable as tombstoned, and a posting referencing it still resolves.
#[tokio::test]
async fn c4_posting_to_tombstoned_account_resolves() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let a = Conn::new(&bin, &journal);

    // History on the account, then tombstone it (through the guarded production path).
    a.guarded_post(
        ToolClass::Record,
        txn("before", "expenses:misc", "1.00", None),
    )
    .await
    .expect("pre-tombstone post");
    write::guarded_write(
        &a.hledger,
        &a.write_lock,
        &a.last_seen,
        ToolClass::Record,
        || write::tombstone_account(&a.hledger, "expenses:misc"),
    )
    .await
    .expect("tombstone");

    let tombstoned = a.hledger.tombstoned_accounts().await.expect("tombstoned");
    assert_eq!(tombstoned, vec!["expenses:misc".to_string()]);
    let declared = a.hledger.declared_accounts().await.expect("declared");
    assert!(
        declared.contains(&"expenses:misc".to_string()),
        "tombstoned account is still declared (soft delete)"
    );

    // A new posting to the tombstoned account still resolves (passes check --strict).
    a.guarded_post(
        ToolClass::Record,
        txn("after", "expenses:misc", "2.00", None),
    )
    .await
    .expect("posting to a tombstoned account resolves (C-4)");

    // Tombstoning is idempotent (a repeat is a no-op at the current epoch, not an error).
    let head_before = write::current_epoch(&journal).expect("epoch");
    let again = write::tombstone_account(&a.hledger, "expenses:misc")
        .await
        .expect("idempotent re-tombstone");
    assert_eq!(Some(again.as_str()), head_before.oid(), "no new commit");

    // Tombstoning an undeclared account is a correctable input error.
    let err = write::tombstone_account(&a.hledger, "no:such")
        .await
        .expect_err("undeclared");
    assert!(matches!(err, WriteError::Input(_)), "{err:?}");
}

/// **C-5** — progress/liveness: nothing is held, so a stale connection *always* succeeds after
/// re-reading, no matter how many times it lost the race.
#[tokio::test]
async fn c5_stale_connection_always_progresses_after_reread() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let a = Conn::new(&bin, &journal);
    let b = Conn::new(&bin, &journal);

    for round in 0..3 {
        a.read().await;
        // B races a write in every round — A's grounded belief is repeatedly invalidated.
        b.guarded_post(
            ToolClass::Record,
            txn(&format!("race{round}"), "expenses:misc", "1.00", None),
        )
        .await
        .expect("b races");
        let stale = a
            .guarded_post(
                ToolClass::Decide,
                txn(&format!("a{round}"), "expenses:misc", "2.00", None),
            )
            .await
            .expect_err("stale again");
        assert!(matches!(stale, WriteError::StaleEpoch(_)));
        // Re-read → retry: must succeed (nothing held → no deadlock, no starvation by design).
        a.read().await;
        a.guarded_post(
            ToolClass::Decide,
            txn(&format!("a{round}"), "expenses:misc", "2.00", None),
        )
        .await
        .expect("after a re-read the decide always lands");
    }
}

/// **C-6** — a soft-invariant violation (an overdrawing post) **succeeds** and is surfaced as
/// a flag in report output, never a rejection.
#[tokio::test]
async fn c6_overdraft_succeeds_and_surfaces_as_flag() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let a = Conn::new(&bin, &journal);

    // Overdraw assets:checking from a zero balance — the record write must NOT be rejected.
    a.guarded_post(
        ToolClass::Record,
        txn("overdraw", "assets:checking", "-50.00", None),
    )
    .await
    .expect("a soft-invariant violation is never a write rejection (C-6)");

    // …but the report surfaces it.
    let report = a
        .hledger
        .balance(Some("assets:checking"))
        .await
        .expect("balance");
    let flags = flags::overdraft_flags(&report);
    assert_eq!(flags.len(), 1, "overdraft flagged");
    assert_eq!(flags[0].kind, "overdraft");
    assert_eq!(flags[0].account, "assets:checking");
}

/// Carried M2 deferral — crash reconcile, the **invalid → restore** branch, end to end: a
/// crash (or hand edit) leaves the working-tree journal *invalid* and uncommitted; startup
/// reconciliation must restore it to `HEAD` (which stays check-valid) and not commit.
#[tokio::test]
async fn crash_reconcile_restores_invalid_uncommitted_journal() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let hl = Hledger::new(&bin, Some(journal.clone()));

    let committed = std::fs::read_to_string(&journal).expect("read journal");
    let head_before = write::current_epoch(&journal).expect("epoch");

    // Simulate the crash: the journal on disk is invalid (undeclared account, --strict fails)
    // and uncommitted.
    let mut bad = committed.clone();
    bad.push_str("\n2026-01-01 garbage\n    no:such:account  $1.00\n    equity:opening\n");
    std::fs::write(&journal, &bad).expect("write bad journal");

    let outcome = write::reconcile(&hl).await.expect("reconcile runs");
    assert_eq!(outcome, None, "an invalid tree is never committed");
    assert_eq!(
        std::fs::read_to_string(&journal).expect("read"),
        committed,
        "journal restored to HEAD byte-for-byte"
    );
    assert_eq!(
        write::current_epoch(&journal).expect("epoch"),
        head_before,
        "HEAD unchanged — reconcile never advances the epoch for invalid content"
    );
}

/// A record write must NOT mask interleaved commits (the bug the M3 code review caught,
/// `FullyInformedDecisions` in the spec): A reads, B commits, A posts a record — A's own
/// write succeeding teaches it nothing about B's commit, so A's next decide must still be
/// rejected `STALE` until A actually re-reads.
#[tokio::test]
async fn record_write_does_not_mask_interleaved_commits() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let a = Conn::new(&bin, &journal);
    let b = Conn::new(&bin, &journal);

    a.read().await;
    b.guarded_post(ToolClass::Record, txn("b", "expenses:misc", "1.00", None))
        .await
        .expect("b commits while a is idle");
    // A's record write succeeds (records are epoch-free)…
    a.guarded_post(
        ToolClass::Record,
        txn("a-rec", "expenses:misc", "2.00", None),
    )
    .await
    .expect("a's record lands at any epoch");
    // …but it must not have refreshed A's view: the decide is still built on a belief that
    // never saw B's commit.
    let err = a
        .guarded_post(
            ToolClass::Decide,
            txn("a-dec", "expenses:misc", "3.00", None),
        )
        .await
        .expect_err("a's own record write must not mask b's commit");
    assert!(matches!(err, WriteError::StaleEpoch(_)), "{err:?}");

    // After a real read the decide lands; and a write on top of a *fresh* view does keep the
    // view fresh (no spurious STALE for single-connection flows).
    a.read().await;
    a.guarded_post(
        ToolClass::Record,
        txn("a-rec2", "expenses:misc", "4.00", None),
    )
    .await
    .expect("fresh record");
    a.guarded_post(
        ToolClass::Decide,
        txn("a-dec2", "expenses:misc", "5.00", None),
    )
    .await
    .expect("a decide right after a fresh-view write needs no extra read");
}

/// The read-ordering discipline: last-seen must be the HEAD sampled **before** the hledger
/// read. If a write lands between the sample and the bump (a mid-read write), the connection
/// must look stale — never fresh — to the CAS. (The conservative direction; see
/// `concurrency-model.md` "ordering disciplines".)
#[tokio::test]
async fn read_bump_uses_pre_read_sample_so_midread_writes_keep_us_stale() {
    let Some(bin) = hledger_bin() else { return };
    let (_dir, journal) = setup(&bin).await;
    let a = Conn::new(&bin, &journal);
    let b = Conn::new(&bin, &journal);

    // A samples (as its read begins)…
    let pre = write::current_epoch(&journal).expect("sample");
    a.hledger.list_transactions(&[]).await.expect("read");
    // …B commits while A's read is in flight…
    b.guarded_post(
        ToolClass::Record,
        txn("midread", "expenses:misc", "1.00", None),
    )
    .await
    .expect("mid-read write");
    // …and A bumps with the PRE-read sample (what the server does).
    *a.last_seen.lock().await = Some(pre);

    // A's decide must see STALE: its data may predate B's write.
    let err = a
        .guarded_post(ToolClass::Decide, txn("a", "expenses:misc", "2.00", None))
        .await
        .expect_err("mid-read write leaves the reader stale");
    assert!(matches!(err, WriteError::StaleEpoch(_)), "{err:?}");
}
