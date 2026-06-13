//! End-to-end stdio integration test (M0): spawn the real built binary and drive a
//! full MCP lifecycle over newline-delimited JSON-RPC —
//! `initialize → notifications/initialized → tools/list → tools/call` — asserting the
//! advertised tool set, protocol-version negotiation, and an `echo` round-trip.
//!
//! This is the automated half of the headline M0 proof (the manual half is a real
//! Claude Cowork session invoking `echo`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

/// A spawned server plus pipes to drive it.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    fn spawn() -> Self {
        Self::spawn_args(&[])
    }

    /// Spawn the server binary with extra CLI args (e.g. `--journal <path>`), inheriting the
    /// process env (so `HLEDGER_EXECUTABLE_PATH` from `.env.local` reaches the child).
    fn spawn_args(args: &[&str]) -> Self {
        Self::spawn_with_env(args, &[])
    }

    /// [`Self::spawn_args`] with env overrides (e.g. a bogus `HLEDGER_EXECUTABLE_PATH` to
    /// prove a path never spawns hledger).
    fn spawn_with_env(args: &[&str], envs: &[(&str, &str)]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hledger-mcp-for-cowork"))
            .args(args)
            .envs(envs.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn server binary");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Send a JSON-RPC message (one line).
    fn send(&mut self, msg: &Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("write to server");
        self.stdin.flush().expect("flush");
    }

    /// Read the next JSON-RPC message line from the server.
    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).expect("read from server");
        assert!(n > 0, "server closed stdout unexpectedly");
        serde_json::from_str(line.trim()).expect("server emitted valid JSON-RPC")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run the handshake, returning the `initialize` result object.
fn initialize(server: &mut Server, requested_version: &str) -> Value {
    server.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": requested_version,
            "capabilities": {},
            "clientInfo": { "name": "itest", "version": "0.0.0" }
        }
    }));
    let resp = server.recv();
    assert_eq!(resp["id"], json!(1), "initialize response id");
    server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    resp["result"].clone()
}

#[test]
fn full_lifecycle_lists_tools_and_echoes() {
    let mut server = Server::spawn();
    let result = initialize(&mut server, "2025-11-25");

    // A supported version is echoed verbatim; tools + resources capabilities show up (M5).
    assert_eq!(result["protocolVersion"], json!("2025-11-25"));
    assert!(
        result["capabilities"]["tools"].is_object(),
        "tools capability declared"
    );
    assert!(
        result["capabilities"]["resources"].is_object(),
        "resources capability declared (M5)"
    );
    assert!(
        result["instructions"]
            .as_str()
            .is_some_and(|s| s.contains("ledger://session-context")),
        "server_instructions points at session-context"
    );

    // tools/list advertises all tools under the default `full` profile (M0–M5).
    server.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let listed = server.recv();
    let mut names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name").to_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "budget_list".to_string(),
            "budget_set".to_string(),
            "close_account".to_string(),
            "declare_account".to_string(),
            "declare_commodity".to_string(),
            "echo".to_string(),
            "eco_approve".to_string(),
            "eco_create".to_string(),
            "eco_void".to_string(),
            "fund_project".to_string(),
            "get_account_balance".to_string(),
            "get_ap_aging".to_string(),
            "get_budget_vs_actual".to_string(),
            "get_project_summary".to_string(),
            "list_transactions".to_string(),
            "pay_invoice".to_string(),
            "post_interest".to_string(),
            "post_transaction".to_string(),
            "receive_invoice".to_string(),
            "status".to_string(),
            "update_transaction".to_string(),
            "vendor_add".to_string(),
            "vendor_list".to_string(),
            "void_transaction".to_string(),
        ]
    );

    // tools/call echo round-trips the message.
    server.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "echo", "arguments": { "message": "ping" } }
    }));
    let called = server.recv();
    let result = &called["result"];
    assert_eq!(result["isError"], json!(false));
    assert_eq!(result["content"][0]["text"], json!("ping"));

    // status reports the session's negotiated protocol version (here 2025-11-25), proving it
    // reads the live peer rather than hardcoding the server's newest.
    server.send(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": { "name": "status", "arguments": {} }
    }));
    let status = server.recv();
    assert_eq!(status["result"]["isError"], json!(false));
    let body = status["result"]["content"][0]["text"]
        .as_str()
        .expect("status text");
    assert!(
        body.contains("protocol: 2025-11-25"),
        "status reports the negotiated version: {body}"
    );
}

#[test]
fn unknown_newer_protocol_version_is_capped_not_echoed() {
    let mut server = Server::spawn();
    // A future RC (lexically > our newest) the server has not validated must be capped to the
    // newest supported, never blind-echoed.
    let result = initialize(&mut server, "2026-07-28");
    assert_eq!(result["protocolVersion"], json!("2025-11-25"));
}

#[test]
fn unknown_older_protocol_version_is_returned_as_requested() {
    let mut server = Server::spawn();
    // Documents the real wire behavior: rmcp reconciles via min(client, our_response), so a
    // version lexically BELOW our newest is returned as the client requested it — our
    // negotiate() cap does not reach the wire here (see src/protocol.rs effective_version).
    // This is the below-range case the cap test above cannot cover.
    let result = initialize(&mut server, "2024-06-01");
    assert_eq!(result["protocolVersion"], json!("2024-06-01"));
}

/// Can we actually run hledger? Mirrors the smoke test's resolution so this e2e skips
/// gracefully (rather than failing) when hledger is absent — e.g. outside `nix develop`.
fn hledger_available() -> bool {
    let runnable = |bin: &str| {
        Command::new(bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    match std::env::var("HLEDGER_EXECUTABLE_PATH") {
        Ok(p) if !p.is_empty() && runnable(&p) => true,
        _ => runnable("hledger"),
    }
}

/// Drive the real M1 read tools over the wire against the checked-in synthetic fixture
/// journal — the automated proof that `get_account_balance` / `list_transactions` work
/// end-to-end through the adapter and are invocable exactly as a Cowork client would. Skips
/// when hledger is unavailable.
#[test]
fn read_tools_work_end_to_end_against_fixture_journal() {
    if !hledger_available() {
        eprintln!("SKIP read e2e: hledger not found (run inside `nix develop`)");
        return;
    }
    let journal = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample.journal");
    let mut server = Server::spawn_args(&["--journal", journal]);
    initialize(&mut server, "2025-11-25");

    // status reports the detected hledger version + pin match, the resolved binary, and the
    // journal in use (the operator's "which hledger / which ledger" diagnostic).
    server.send(&json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": { "name": "status", "arguments": {} }
    }));
    let status = server.recv();
    let status_text = status["result"]["content"][0]["text"]
        .as_str()
        .expect("status text");
    // Assert status surfaced the backend (version line + binary + journal) without pinning to
    // a specific hledger version — a dev box may have a non-1.52 hledger on PATH, and this test
    // should exercise the wiring, not the pin (the pin is asserted in the version unit tests).
    assert!(
        status_text.contains("hledger: ")
            && status_text.contains("binary:")
            && status_text.contains("sample.journal"),
        "status reports the backend version, binary, and journal: {status_text}"
    );

    // get_account_balance returns the real computed balance ($100 − $12.34 − $44 = $43.66).
    server.send(&json!({
        "jsonrpc": "2.0", "id": 11, "method": "tools/call",
        "params": { "name": "get_account_balance", "arguments": { "account": "assets:checking" } }
    }));
    let bal = server.recv();
    assert_eq!(bal["result"]["isError"], json!(false));
    let bal_text = bal["result"]["content"][0]["text"]
        .as_str()
        .expect("balance text");
    assert!(
        bal_text.contains("assets:checking") && bal_text.contains("$43.66"),
        "balance output: {bal_text}"
    );

    // list_transactions with a query returns the matching transaction's header + postings.
    server.send(&json!({
        "jsonrpc": "2.0", "id": 12, "method": "tools/call",
        "params": { "name": "list_transactions", "arguments": { "query": ["expenses:supplies"] } }
    }));
    let txns = server.recv();
    assert_eq!(txns["result"]["isError"], json!(false));
    let txns_text = txns["result"]["content"][0]["text"]
        .as_str()
        .expect("transactions text");
    assert!(
        txns_text.contains("2026-01-15 Acme") && txns_text.contains("expenses:supplies"),
        "transactions output: {txns_text}"
    );
}

/// **M3 cross-process contention (flock):** two *separate server processes* on the same
/// journal — the real stdio multi-client shape (Desktop / Cowork / Claude Code each spawn
/// their own server) — write concurrently. The advisory file lock beside the journal must
/// serialize them: every write lands, every commit is distinct (one write = one epoch), and
/// the final journal is `check --strict`-valid. Requests are pipelined to both processes
/// *before* any response is read, so the writes genuinely contend.
#[test]
fn concurrent_writers_from_two_processes_serialize() {
    if !hledger_available() {
        eprintln!("SKIP contention e2e: hledger not found (run inside `nix develop`)");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("main.journal");
    let journal_arg = journal.display().to_string();

    let mut a = Server::spawn_args(&["--journal", &journal_arg]);
    let mut b = Server::spawn_args(&["--journal", &journal_arg]);
    initialize(&mut a, "2025-11-25");
    initialize(&mut b, "2025-11-25");

    // Bootstrap declarations through server A (serial, awaited calls).
    let mut id = 100;
    let mut call_ok = |server: &mut Server, name: &str, args: Value| -> String {
        id += 1;
        server.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }));
        let resp = server.recv();
        assert_eq!(
            resp["result"]["isError"],
            json!(false),
            "{name} failed: {resp}"
        );
        resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text")
            .to_owned()
    };
    call_ok(&mut a, "declare_commodity", json!({ "commodity": "$" }));
    call_ok(
        &mut a,
        "declare_account",
        json!({ "account": "expenses:misc" }),
    );
    call_ok(
        &mut a,
        "declare_account",
        json!({ "account": "equity:opening" }),
    );

    // Contention: pipeline N posts into BOTH processes before reading any response.
    const N: usize = 4;
    let post = |tag: &str, i: usize, req_id: usize| {
        json!({
            "jsonrpc": "2.0", "id": req_id, "method": "tools/call",
            "params": { "name": "post_transaction", "arguments": {
                "date": "2026-01-01",
                "description": format!("contend-{tag}-{i}"),
                "postings": [
                    { "account": "expenses:misc",
                      "amount": { "quantity": "1.00", "commodity": "$" } },
                    { "account": "equity:opening" }
                ]
            }}
        })
    };
    for i in 0..N {
        a.send(&post("a", i, 200 + i));
        b.send(&post("b", i, 300 + i));
    }

    // Collect all 2N responses; every one must have succeeded with its own commit.
    let mut commits = std::collections::HashSet::new();
    for server in [&mut a, &mut b] {
        for _ in 0..N {
            let resp = server.recv();
            assert_eq!(
                resp["result"]["isError"],
                json!(false),
                "contended post failed: {resp}"
            );
            let text = resp["result"]["content"][0]["text"].as_str().expect("text");
            let commit = text
                .rsplit("(commit ")
                .next()
                .and_then(|s| s.strip_suffix(')'))
                .expect("commit oid in response")
                .to_owned();
            assert!(commits.insert(commit), "duplicate epoch across processes");
        }
    }
    assert_eq!(commits.len(), 2 * N, "one fresh epoch per write");

    // Every write landed (no lost update), and the final journal is check-valid.
    let listed = call_ok(&mut a, "list_transactions", json!({}));
    for tag in ["a", "b"] {
        for i in 0..N {
            assert!(
                listed.contains(&format!("contend-{tag}-{i}")),
                "missing contend-{tag}-{i} in:\n{listed}"
            );
        }
    }
    let hledger = std::env::var("HLEDGER_EXECUTABLE_PATH").unwrap_or("hledger".into());
    let check = Command::new(hledger)
        .args(["check", "--strict", "-f", &journal_arg])
        .output()
        .expect("run hledger check");
    assert!(
        check.status.success(),
        "final journal must be check --strict valid: {}",
        String::from_utf8_lossy(&check.stderr)
    );
}

/// **M4 domain tools e2e:** drive all eight M4 tools over the wire against a fresh
/// tempdir journal, checking response content (not just `isError: false`). Skips when
/// hledger is absent.
#[test]
fn m4_domain_tools_work_end_to_end() {
    if !hledger_available() {
        eprintln!("SKIP M4 e2e: hledger not found (run inside `nix develop`)");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("main.journal");
    let journal_arg = journal.display().to_string();
    let mut server = Server::spawn_args(&["--journal", &journal_arg]);
    initialize(&mut server, "2025-11-25");

    let mut id = 700;
    let mut call = |server: &mut Server, name: &str, args: Value| -> String {
        id += 1;
        server.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }));
        let resp = server.recv();
        assert_eq!(
            resp["result"]["isError"],
            json!(false),
            "{name} must not be isError: {resp}"
        );
        resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text content")
            .to_owned()
    };

    // Bootstrap declarations
    call(
        &mut server,
        "declare_commodity",
        json!({ "commodity": "$" }),
    );
    call(
        &mut server,
        "declare_account",
        json!({ "account": "assets:checking" }),
    );
    call(
        &mut server,
        "declare_account",
        json!({ "account": "equity:owner capital" }),
    );
    call(
        &mut server,
        "declare_account",
        json!({ "account": "income:interest" }),
    );

    // vendor_add — declares AP + expense accounts
    let va = call(
        &mut server,
        "vendor_add",
        json!({ "vendor": "Acme", "vendor_type": "trade", "trade": "plumbing" }),
    );
    assert!(va.contains("Acme"), "vendor_add response: {va}");

    // vendor_list — Acme's AP account is now declared
    let vl = call(&mut server, "vendor_list", json!({}));
    assert!(
        vl.contains("liabilities:ap:vendor:Acme"),
        "vendor_list response: {vl}"
    );

    // fund_project
    let fp = call(
        &mut server,
        "fund_project",
        json!({ "date": "2020-01-01", "amount": "50000.00", "commodity": "$" }),
    );
    assert!(fp.contains("commit"), "fund_project response: {fp}");

    // receive_invoice — use an old date so the aging flag fires
    let ri = call(
        &mut server,
        "receive_invoice",
        json!({
            "date": "2020-01-15",
            "vendor": "Acme",
            "expense_account": "expenses:construction:plumbing",
            "amount": "8000.00",
            "commodity": "$",
            "invoice_ref": "INV-001"
        }),
    );
    assert!(ri.contains("commit"), "receive_invoice response: {ri}");

    // get_ap_aging — Acme balance is outstanding and old enough to be overdue
    let aging = call(&mut server, "get_ap_aging", json!({}));
    assert!(aging.contains("AP aging"), "get_ap_aging header: {aging}");
    assert!(aging.contains("Acme"), "get_ap_aging has vendor: {aging}");
    // The invoice is from 2020 — definitely Over90Days; the ap-aging soft-invariant flag
    // must appear as a separate "flag ap-aging: …" footer line (distinct from the age
    // label in the entry row, which also says "overdue").
    assert!(
        aging.contains("flag ap-aging"),
        "get_ap_aging must surface ap-aging flag footer: {aging}"
    );

    // pay_invoice — clears Acme's AP balance
    let pi = call(
        &mut server,
        "pay_invoice",
        json!({
            "date": "2020-02-01",
            "vendor": "Acme",
            "amount": "8000.00",
            "commodity": "$"
        }),
    );
    assert!(pi.contains("commit"), "pay_invoice response: {pi}");

    // Paying in full clears the AP balance — aging must now report nothing outstanding.
    let cleared = call(&mut server, "get_ap_aging", json!({}));
    assert!(
        cleared.contains("(no outstanding payables)"),
        "aging after full payment: {cleared}"
    );

    // post_interest
    let int = call(
        &mut server,
        "post_interest",
        json!({ "date": "2020-03-01", "amount": "10.00", "commodity": "$" }),
    );
    assert!(int.contains("commit"), "post_interest response: {int}");

    // Checking reflects the whole chain: $50000 fund − $8000 payment + $10 interest.
    let bal = call(
        &mut server,
        "get_account_balance",
        json!({ "account": "assets:checking" }),
    );
    assert!(bal.contains("$42010.00"), "checking balance: {bal}");

    // get_project_summary — balance sheet + income statement
    let summary = call(&mut server, "get_project_summary", json!({}));
    assert!(
        summary.contains("Balance Sheet"),
        "summary has balance sheet: {summary}"
    );
    assert!(
        summary.contains("Income Statement"),
        "summary has income statement: {summary}"
    );
}

#[test]
fn bad_tool_args_return_iserror_not_protocol_error() {
    let mut server = Server::spawn();
    initialize(&mut server, "2025-11-25");

    server.send(&json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "echo", "arguments": { "wrong": "key" } }
    }));
    let resp = server.recv();
    // No JSON-RPC error object; the *result* carries isError so the model self-corrects.
    assert!(
        resp.get("error").is_none(),
        "must not be a JSON-RPC protocol error: {resp}"
    );
    assert_eq!(resp["result"]["isError"], json!(true));
}

// ---- M5 e2e --------------------------------------------------------------------------

/// Call a tool, returning `(is_error, text)` — for flows that expect failures (STALE,
/// already-approved).
fn call_any(server: &mut Server, id: u64, name: &str, args: Value) -> (bool, String) {
    server.send(&json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    }));
    let resp = server.recv();
    let result = &resp["result"];
    (
        result["isError"] == json!(true),
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

/// Call a tool, asserting success, returning the text body.
fn call_ok(server: &mut Server, id: u64, name: &str, args: Value) -> String {
    let (is_error, text) = call_any(server, id, name, args);
    assert!(!is_error, "{name} must succeed: {text}");
    text
}

/// Read a resource over the wire, returning the result value (panics on JSON-RPC error).
fn read_resource(server: &mut Server, id: u64, uri: &str) -> Value {
    server.send(&json!({
        "jsonrpc": "2.0", "id": id, "method": "resources/read",
        "params": { "uri": uri }
    }));
    let resp = server.recv();
    assert!(resp.get("error").is_none(), "resources/read {uri}: {resp}");
    resp["result"].clone()
}

/// The MC-8 cold-start claim, asserted: with a **bogus hledger binary**, the whole discovery
/// path — initialize, tools/list (tiered descriptions), resources/list, every static
/// resource read — works; only the dynamic `ledger://vendors` resource fails (it is the
/// documented exception that reads the ledger).
#[test]
fn discovery_path_serves_tools_and_resources_without_hledger() {
    let mut server =
        Server::spawn_with_env(&[], &[("HLEDGER_EXECUTABLE_PATH", "/nonexistent/hledger")]);
    let result = initialize(&mut server, "2025-11-25");
    assert!(result["capabilities"]["resources"].is_object());

    // tools/list: full catalog, Tier-2 descriptions reduced to one line.
    server.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let tools = server.recv()["result"]["tools"].clone();
    let tools = tools.as_array().expect("tools array");
    assert_eq!(tools.len(), 24, "full catalog advertised");
    let desc = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == json!(name))
            .and_then(|t| t["description"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let vendor_add = desc("vendor_add");
    assert!(
        vendor_add.contains("ledger://vendor-guide") && !vendor_add.contains('\n'),
        "tier-2 is a one-liner pointing at its guide: {vendor_add}"
    );
    assert!(
        desc("post_transaction").len() > vendor_add.len(),
        "tier-1 keeps the full description"
    );

    // resources/list: 6 static guides + the dynamic vendors resource.
    server.send(&json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }));
    let resources = server.recv()["result"]["resources"].clone();
    let uris: Vec<String> = resources
        .as_array()
        .expect("resources array")
        .iter()
        .map(|r| r["uri"].as_str().expect("uri").to_owned())
        .collect();
    assert_eq!(uris.len(), 7, "{uris:?}");
    assert!(uris.contains(&"ledger://session-context".to_string()));
    assert!(uris.contains(&"ledger://vendors".to_string()));

    // Every static resource serves its markdown — still no hledger anywhere.
    for (i, uri) in uris.iter().filter(|u| *u != "ledger://vendors").enumerate() {
        let result = read_resource(&mut server, 10 + i as u64, uri);
        let text = result["contents"][0]["text"].as_str().expect("text body");
        assert!(text.len() > 200, "{uri} has substantive content");
    }

    // The dynamic vendors resource is the exception: it DOES hit hledger, so with the bogus
    // binary it errors — proving the rest of discovery never touched the backend.
    server.send(&json!({
        "jsonrpc": "2.0", "id": 30, "method": "resources/read",
        "params": { "uri": "ledger://vendors" }
    }));
    let resp = server.recv();
    assert!(resp.get("error").is_some(), "vendors needs hledger: {resp}");

    // Unknown URI → the resource-not-found error code (-32002).
    server.send(&json!({
        "jsonrpc": "2.0", "id": 31, "method": "resources/read",
        "params": { "uri": "ledger://nope" }
    }));
    let resp = server.recv();
    assert_eq!(resp["error"]["code"], json!(-32002), "{resp}");
}

/// MC-10 over the wire: `--profile operational` advertises only Tier 1, `status` reports the
/// profile, and a tool **not** advertised still dispatches.
#[test]
fn profile_filters_advertising_but_not_dispatch() {
    if !hledger_available() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("main.journal");
    let journal_arg = journal.display().to_string();
    let mut server = Server::spawn_args(&["--journal", &journal_arg, "--profile", "operational"]);
    initialize(&mut server, "2025-11-25");

    server.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let mut names: Vec<String> = server.recv()["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "fund_project",
            "get_account_balance",
            "get_ap_aging",
            "get_project_summary",
            "list_transactions",
            "pay_invoice",
            "post_interest",
            "post_transaction",
            "receive_invoice",
            "status",
            "update_transaction",
            "void_transaction",
        ],
        "operational advertises exactly Tier 1"
    );

    let status = call_ok(&mut server, 3, "status", json!({}));
    assert!(
        status.contains("profile: operational"),
        "status reports the profile: {status}"
    );

    // The MC-10 invariant: declare_commodity is NOT advertised, but still dispatches.
    let out = call_ok(
        &mut server,
        4,
        "declare_commodity",
        json!({ "commodity": "$" }),
    );
    assert!(out.contains("declared commodity"), "{out}");
}

/// The budget + ECO round-trip (M5 headline): set a goal, overspend → over-budget flag;
/// replace the goal (not accumulate!); create → approve → re-approve fails → void a change
/// order, with balances proving each step.
#[test]
fn budget_and_eco_lifecycle_work_end_to_end() {
    if !hledger_available() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("main.journal");
    let journal_arg = journal.display().to_string();
    let mut server = Server::spawn_args(&["--journal", &journal_arg]);
    initialize(&mut server, "2025-11-25");
    let mut id = 100;
    let mut call = |server: &mut Server, name: &str, args: Value| -> String {
        id += 1;
        call_ok(server, id, name, args)
    };

    // Bootstrap: commodity, core accounts, the GC vendor (for the CO), a budgeted trade.
    call(
        &mut server,
        "declare_commodity",
        json!({ "commodity": "$" }),
    );
    for account in [
        "assets:checking",
        "equity:owner capital",
        "expenses:construction:plumbing",
    ] {
        call(
            &mut server,
            "declare_account",
            json!({ "account": account }),
        );
    }
    call(
        &mut server,
        "vendor_add",
        json!({ "vendor": "GC", "vendor_type": "trade", "trade": "general" }),
    );
    call(
        &mut server,
        "fund_project",
        json!({ "date": "2020-01-01", "amount": "50000.00", "commodity": "$" }),
    );

    // Budget: set, list, overspend → flag.
    let set = call(
        &mut server,
        "budget_set",
        json!({
            "account": "expenses:construction:plumbing",
            "period": "monthly", "amount": "500.00", "commodity": "$"
        }),
    );
    assert!(
        set.contains("budget set") && set.contains("commit"),
        "{set}"
    );
    let rules = call(&mut server, "budget_list", json!({}));
    assert!(
        rules.contains("expenses:construction:plumbing  monthly = 500.00 $"),
        "{rules}"
    );
    call(
        &mut server,
        "post_transaction",
        json!({
            "date": "2020-01-15", "description": "pipes",
            "postings": [
                { "account": "expenses:construction:plumbing",
                  "amount": { "quantity": "800.00", "commodity": "$" } },
                { "account": "assets:checking" }
            ]
        }),
    );
    let report = call(&mut server, "get_budget_vs_actual", json!({}));
    assert!(
        report.contains("flag over-budget: expenses:construction:plumbing"),
        "over-budget surfaces as a flag: {report}"
    );

    // budget_set again REPLACES the goal (the design point — appends would accumulate).
    call(
        &mut server,
        "budget_set",
        json!({
            "account": "expenses:construction:plumbing",
            "period": "monthly", "amount": "1000.00", "commodity": "$"
        }),
    );
    let report = call(&mut server, "get_budget_vs_actual", json!({}));
    assert!(
        report.contains("budget $1000.00") && !report.contains("flag over-budget"),
        "replaced (not accumulated) goal clears the flag: {report}"
    );

    // ECO lifecycle: create (pending) → approve (decide) → re-approve fails → void.
    let created = call(
        &mut server,
        "eco_create",
        json!({
            "eco": "7", "trade": "electrical", "vendor": "GC",
            "description": "add outlets", "date": "2020-02-01",
            "amount": "1500.00", "commodity": "$"
        }),
    );
    assert!(created.contains("pending"), "{created}");
    let pending = call(
        &mut server,
        "get_account_balance",
        json!({ "account": "expenses:change orders:pending" }),
    );
    assert!(pending.contains("1500.00"), "pending exposure: {pending}");

    // Budget the CO account (eco_create auto-declared it) so the approval's budget
    // footer reports a named row — the "approve because within budget" grounding.
    call(
        &mut server,
        "budget_set",
        json!({
            "account": "expenses:change orders:electrical",
            "period": "monthly", "amount": "2000.00", "commodity": "$"
        }),
    );
    let approved = call(
        &mut server,
        "eco_approve",
        json!({ "eco": "7", "date": "2020-02-05" }),
    );
    assert!(
        approved.contains("approved")
            && approved.contains("expenses:change orders:electrical  actual $1500.00 | budget $"),
        "approval reports the budget standing: {approved}"
    );
    let tracked = call(
        &mut server,
        "get_account_balance",
        json!({ "account": "expenses:change orders:electrical" }),
    );
    assert!(tracked.contains("1500.00"), "{tracked}");

    let (is_error, text) = call_any(
        &mut server,
        999,
        "eco_approve",
        json!({ "eco": "7", "date": "2020-02-06" }),
    );
    assert!(is_error && text.contains("already approved"), "{text}");

    let voided = call(
        &mut server,
        "eco_void",
        json!({ "eco": "7", "date": "2020-03-01" }),
    );
    assert!(
        voided.contains("2 reversing"),
        "create + approval: {voided}"
    );
    let after = call(
        &mut server,
        "get_account_balance",
        json!({ "account": "expenses:change orders" }),
    );
    assert!(after.contains("total  $0.00"), "CO subtree zeroed: {after}");
}

/// `eco_approve` is a **decide** call: a write from another process between this
/// connection's read and its approve makes the approve STALE; a re-read unblocks it (the
/// C-1 epoch-CAS contract, end-to-end over the MCP wire).
#[test]
fn eco_approve_is_epoch_checked_across_processes() {
    if !hledger_available() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("main.journal");
    let journal_arg = journal.display().to_string();
    let mut a = Server::spawn_args(&["--journal", &journal_arg]);
    initialize(&mut a, "2025-11-25");

    call_ok(&mut a, 2, "declare_commodity", json!({ "commodity": "$" }));
    call_ok(
        &mut a,
        3,
        "vendor_add",
        json!({ "vendor": "GC", "vendor_type": "trade", "trade": "general" }),
    );
    call_ok(
        &mut a,
        4,
        "eco_create",
        json!({
            "eco": "9", "trade": "hvac", "vendor": "GC",
            "description": "bigger unit", "date": "2020-02-01",
            "amount": "2000.00", "commodity": "$"
        }),
    );
    // A reads → its last-seen epoch is current.
    call_ok(
        &mut a,
        5,
        "get_account_balance",
        json!({ "account": "expenses:change orders:pending" }),
    );

    // B (another process) moves HEAD.
    {
        let mut b = Server::spawn_args(&["--journal", &journal_arg]);
        initialize(&mut b, "2025-11-25");
        call_ok(
            &mut b,
            2,
            "declare_account",
            json!({ "account": "expenses:misc" }),
        );
    }

    // A's approve is now a decision on a stale belief → STALE, not silently applied.
    let (is_error, text) = call_any(
        &mut a,
        6,
        "eco_approve",
        json!({ "eco": "9", "date": "2020-02-05" }),
    );
    assert!(is_error && text.contains("STALE"), "{text}");

    // Re-read → retry succeeds.
    call_ok(
        &mut a,
        7,
        "get_account_balance",
        json!({ "account": "expenses:change orders:pending" }),
    );
    let approved = call_ok(
        &mut a,
        8,
        "eco_approve",
        json!({ "eco": "9", "date": "2020-02-05" }),
    );
    assert!(approved.contains("approved"), "{approved}");
}

/// The M4 deferrals, closed: a permit posts prepaid via `post_transaction` with **no AP**
/// account anywhere, and a multi-line GC pass-through invoice splits across two trade
/// accounts with the GC as the sole AP vendor (chart-of-accounts.md rules).
#[test]
fn permits_and_gc_passthrough_post_via_the_generic_tool() {
    if !hledger_available() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("main.journal");
    let journal_arg = journal.display().to_string();
    let mut server = Server::spawn_args(&["--journal", &journal_arg]);
    initialize(&mut server, "2025-11-25");
    let mut id = 200;
    let mut call = |server: &mut Server, name: &str, args: Value| -> String {
        id += 1;
        call_ok(server, id, name, args)
    };

    call(
        &mut server,
        "declare_commodity",
        json!({ "commodity": "$" }),
    );
    for account in [
        "assets:checking",
        "expenses:permits and fees",
        "expenses:construction:plumbing",
        "expenses:construction:electrical",
    ] {
        call(
            &mut server,
            "declare_account",
            json!({ "account": account }),
        );
    }

    // Permit: prepaid, no AP — the jurisdiction is never a vendor.
    call(
        &mut server,
        "post_transaction",
        json!({
            "date": "2020-01-10", "description": "building permit",
            "postings": [
                { "account": "expenses:permits and fees",
                  "amount": { "quantity": "120.00", "commodity": "$" } },
                { "account": "assets:checking" }
            ]
        }),
    );
    let permits = call(
        &mut server,
        "get_account_balance",
        json!({ "account": "expenses:permits and fees" }),
    );
    assert!(permits.contains("120.00"), "{permits}");
    let aging = call(&mut server, "get_ap_aging", json!({}));
    assert!(
        aging.contains("(no outstanding payables)"),
        "permit must touch no AP: {aging}"
    );

    // Multi-line GC pass-through: one transaction, two trade splits, GC is the AP vendor.
    call(
        &mut server,
        "vendor_add",
        json!({ "vendor": "GC", "vendor_type": "trade", "trade": "general" }),
    );
    call(
        &mut server,
        "post_transaction",
        json!({
            "date": "2020-01-20", "description": "GC invoice",
            "tags": [["invoice", "INV-042"], ["vendor", "GC"]],
            "postings": [
                { "account": "expenses:construction:plumbing",
                  "amount": { "quantity": "3000.00", "commodity": "$" } },
                { "account": "expenses:construction:electrical",
                  "amount": { "quantity": "2000.00", "commodity": "$" } },
                { "account": "liabilities:ap:vendor:GC" }
            ]
        }),
    );
    let aging = call(&mut server, "get_ap_aging", json!({}));
    assert!(
        aging.contains("liabilities:ap:vendor:GC") && aging.contains("5000.00"),
        "GC carries the pass-through AP: {aging}"
    );

    // And the normal pay clears it.
    call(
        &mut server,
        "pay_invoice",
        json!({ "date": "2020-02-01", "vendor": "GC", "amount": "5000.00", "commodity": "$" }),
    );
    let aging = call(&mut server, "get_ap_aging", json!({}));
    assert!(aging.contains("(no outstanding payables)"), "{aging}");
}
