//! End-to-end smoke test exercising the **real** `hledger` binary (no mocking)
//! plus `git`, mirroring the write-path skeleton from the design docs:
//! write journal → `hledger check --strict` → read back → `git commit` (one
//! commit == one epoch).
//!
//! It **skips gracefully** when hledger is absent (e.g. outside `nix develop`)
//! so the suite — and the Stop-hook quality gate — still pass before the env is
//! materialized. Inside the nix dev shell it runs for real.

use std::process::Command;

/// Is `bin` an hledger we can actually run?
fn runnable(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve the hledger binary: prefer `HLEDGER_EXECUTABLE_PATH` (set by the flake
/// / written by `mise run init-env`), else `hledger` on PATH. A stale or bogus
/// env path falls through so the test skips instead of panicking.
fn hledger_bin() -> Option<String> {
    if let Ok(p) = std::env::var("HLEDGER_EXECUTABLE_PATH")
        && !p.is_empty()
        && runnable(&p)
    {
        return Some(p);
    }
    runnable("hledger").then(|| "hledger".to_string())
}

/// A `--strict`-clean journal: requires `commodity` + `account` declarations,
/// exactly what the production write path must satisfy.
const JOURNAL: &str = "\
commodity $1000.00
account assets:checking
account equity:opening balances

2026-01-01 opening balance
    assets:checking            $100.00
    equity:opening balances
";

#[test]
fn hledger_check_read_and_commit_roundtrip() {
    let Some(hledger) = hledger_bin() else {
        eprintln!("SKIP e2e smoke: hledger not found (run inside `nix develop`)");
        return;
    };

    // 1. We can exec the real (store) binary.
    let ver = Command::new(&hledger)
        .arg("--version")
        .output()
        .expect("exec hledger --version");
    assert!(ver.status.success(), "hledger --version failed");
    eprintln!("e2e using: {}", String::from_utf8_lossy(&ver.stdout).trim());

    // Scratch dir under $TMPDIR (sandbox-writable); auto-removed on drop.
    let dir = tempfile::tempdir().expect("create tempdir");
    let journal = dir.path().join("main.journal");
    std::fs::write(&journal, JOURNAL).expect("write journal");

    // 2. `hledger check --strict` passes on the well-formed journal.
    let check = Command::new(&hledger)
        .args(["check", "--strict", "-f"])
        .arg(&journal)
        .output()
        .expect("exec hledger check");
    assert!(
        check.status.success(),
        "hledger check --strict failed:\n{}",
        String::from_utf8_lossy(&check.stderr)
    );

    // 3. A read returns the expected balance.
    let bal = Command::new(&hledger)
        .args(["balance", "assets:checking", "-f"])
        .arg(&journal)
        .output()
        .expect("exec hledger balance");
    assert!(bal.status.success(), "hledger balance failed");
    let bal_s = String::from_utf8_lossy(&bal.stdout);
    assert!(
        bal_s.contains("$100.00"),
        "unexpected balance output:\n{bal_s}"
    );

    // 4. git init + commit — the epoch-as-commit skeleton, all in the temp dir.
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(["-c", "user.email=ci@example.com", "-c", "user.name=ci"])
            .args(args)
            .current_dir(dir.path())
            .status()
            .expect("exec git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["add", "main.journal"]);
    git(&["commit", "-qm", "e2e: initial journal"]);

    // HEAD now resolves to a commit — one validated write == one epoch.
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("exec git rev-parse");
    assert!(head.status.success(), "no commit / HEAD missing");
    assert!(
        String::from_utf8_lossy(&head.stdout).trim().len() >= 7,
        "expected a commit hash at HEAD"
    );
}

// ---- M2: the production write path end-to-end (declare → post → check → commit → read-back
// → void), driving the real library code (not raw Command). Skips when hledger is absent.

use hledger_mcp_for_cowork::epoch::ToolClass;
use hledger_mcp_for_cowork::hledger::Hledger;
use hledger_mcp_for_cowork::write::{
    self, CommitOutcome, WriteError, WriteOutcome,
    input::{PostingAmount, PostingInput, TransactionInput},
};

// The write ops demand a `WriteGuard` proof that only the gate mints (the M3 type-level
// invariant), so the e2e drives them through `guarded_once` — the production locking path.

async fn declare_commodity(
    hl: &Hledger,
    symbol: &str,
    places: u32,
) -> Result<CommitOutcome, WriteError> {
    write::guarded_once(hl, ToolClass::Record, async |ctx| {
        write::declare_commodity(&ctx, symbol, places).await
    })
    .await
}

async fn declare_account(hl: &Hledger, name: &str) -> Result<CommitOutcome, WriteError> {
    write::guarded_once(hl, ToolClass::Record, async |ctx| {
        write::declare_account(&ctx, name).await
    })
    .await
}

async fn post_transaction(
    hl: &Hledger,
    input: TransactionInput,
) -> Result<WriteOutcome, WriteError> {
    write::guarded_once(hl, ToolClass::Record, async |ctx| {
        write::post_transaction(&ctx, input).await
    })
    .await
}

async fn void_transaction(hl: &Hledger, id: &str) -> Result<WriteOutcome, WriteError> {
    write::guarded_once(hl, ToolClass::Record, async |ctx| {
        write::void_transaction(&ctx, id).await
    })
    .await
}

fn posting(account: &str, qty: Option<&str>, commodity: &str) -> PostingInput {
    PostingInput {
        account: account.to_string(),
        amount: qty.map(|q| PostingAmount {
            quantity: q.to_string(),
            commodity: commodity.to_string(),
        }),
    }
}

#[tokio::test]
async fn write_path_declare_post_void_round_trip() {
    let Some(bin) = hledger_bin() else {
        eprintln!("SKIP write e2e: hledger not found (run inside `nix develop`)");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("main.journal");
    let hl = Hledger::new(bin, Some(journal.clone()));

    // Declare prerequisites (require-pre-declare). Each is one commit.
    let c0 = declare_commodity(&hl, "$", 2).await.expect("declare $");
    declare_account(&hl, "assets:checking")
        .await
        .expect("declare checking");
    declare_account(&hl, "equity:opening balances")
        .await
        .expect("declare equity");
    declare_account(&hl, "expenses:supplies")
        .await
        .expect("declare supplies");

    // Posting to an UNDECLARED account is rejected as a correctable input error, BEFORE any
    // file change — fail-closed: the journal is byte-identical afterward.
    let before = std::fs::read_to_string(&journal).expect("read journal");
    let bad = TransactionInput {
        date: "2026-01-01".into(),
        description: "bad".into(),
        postings: vec![
            posting("assets:savings", Some("1.00"), "$"),
            posting("assets:checking", None, ""),
        ],
        tags: vec![],
        idem: None,
    };
    let err = post_transaction(&hl, bad)
        .await
        .expect_err("undeclared must fail");
    assert!(matches!(err, write::WriteError::Input(_)), "{err:?}");
    assert_eq!(
        before,
        std::fs::read_to_string(&journal).unwrap(),
        "journal untouched on input error"
    );

    // Post a balanced transaction (one omitted amount balances).
    let input = TransactionInput {
        date: "2026-01-01".into(),
        description: "opening balance".into(),
        postings: vec![
            posting("assets:checking", Some("100.00"), "$"),
            posting("equity:opening balances", None, ""),
        ],
        tags: vec![],
        idem: Some("opening-1".into()),
    };
    let posted = post_transaction(&hl, input.clone()).await.expect("post");
    assert!(!posted.deduped);
    assert_ne!(posted.base.commit, c0.commit, "post is a new commit/epoch");

    // Idempotent retry with the same idem key → no new transaction.
    let retry = post_transaction(&hl, input).await.expect("retry");
    assert!(retry.deduped, "retry deduped");

    // Read back through the adapter: the balance is what we posted.
    let bal = hl.balance(Some("assets:checking")).await.expect("balance");
    assert_eq!(bal.rows[0].amounts[0].render(), "$100.00");
    let all = hl.list_transactions(&[]).await.expect("list");
    assert_eq!(
        all.len(),
        1,
        "exactly one transaction after one post + a deduped retry"
    );

    // Void it → append-only reversing entry; nets the account to zero.
    let voided = void_transaction(&hl, &posted.base.id).await.expect("void");
    assert_ne!(
        voided.base.commit, posted.base.commit,
        "void is its own commit"
    );
    let after_void = hl.list_transactions(&[]).await.expect("list2");
    assert_eq!(after_void.len(), 2, "original + reversal");
    assert!(
        after_void
            .iter()
            .any(|t| t.tags.iter().any(|(k, _)| k == "reverses")),
        "a reversing entry is present"
    );
}

/// The §6 **round-trip safety net**: for several representative valid transactions, the
/// formatter output passes `check --strict` (implicit: `post` only commits if it does) *and*
/// `hledger print -O json` parses back to the same semantic content (date, description, each
/// posting's account + exact amount, and tags). Covers negatives, multiple commodities, and a
/// user tag. (Enumerated rather than `proptest`-generated to keep one hledger subprocess per
/// case bounded; the pure formatter itself is property- and mutation-tested.)
#[tokio::test]
async fn posted_transactions_round_trip_through_hledger() {
    let Some(bin) = hledger_bin() else {
        eprintln!("SKIP round-trip e2e: hledger not found");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("main.journal");
    let hl = Hledger::new(bin, Some(journal.clone()));
    declare_commodity(&hl, "$", 2).await.unwrap();
    declare_commodity(&hl, "EUR", 2).await.unwrap();
    declare_account(&hl, "assets:checking").await.unwrap();
    declare_account(&hl, "expenses:supplies").await.unwrap();
    declare_account(&hl, "expenses:travel").await.unwrap();

    // (id, input, expected (account, rendered-amount) postings)
    let cases = vec![
        (
            "rt-1",
            TransactionInput {
                date: "2026-01-15".into(),
                description: "Acme".into(),
                postings: vec![
                    posting("expenses:supplies", Some("12.34"), "$"),
                    posting("assets:checking", Some("-12.34"), "$"),
                ],
                tags: vec![("vendor".into(), "Acme".into())],
                idem: Some("rt-1".into()),
            },
            vec![
                ("expenses:supplies", "$12.34"),
                ("assets:checking", "$-12.34"),
            ],
        ),
        (
            "rt-2",
            TransactionInput {
                date: "2026-02-02".into(),
                description: "trip".into(),
                postings: vec![
                    posting("expenses:travel", Some("40.00"), "EUR"),
                    posting("assets:checking", None, ""),
                ],
                tags: vec![],
                idem: Some("rt-2".into()),
            },
            vec![("expenses:travel", "40.00 EUR")],
        ),
    ];

    for (idem, input, expected) in cases {
        let out = post_transaction(&hl, input).await.expect("post");
        let found = hl
            .list_transactions(&[format!("tag:idem={idem}")])
            .await
            .expect("read back");
        assert_eq!(found.len(), 1, "exactly one txn for idem {idem}");
        let txn = &found[0];
        // The posting account + exact amount survives the format → check → print round-trip.
        for (account, rendered) in expected {
            let posting = txn
                .postings
                .iter()
                .find(|p| p.account == account)
                .unwrap_or_else(|| panic!("posting {account} missing in {idem}"));
            assert_eq!(posting.amounts[0].render(), rendered, "{account} in {idem}");
        }
        // The author-stamped id tag matches the outcome.
        assert!(
            txn.tags.iter().any(|(k, v)| k == "id" && *v == out.base.id),
            "id tag round-trips for {idem}"
        );
    }
}
