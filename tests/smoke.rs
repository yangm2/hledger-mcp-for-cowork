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
