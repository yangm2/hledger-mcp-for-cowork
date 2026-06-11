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
        let mut child = Command::new(env!("CARGO_BIN_EXE_hledger-mcp-for-cowork"))
            .args(args)
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

    // A supported version is echoed verbatim, and only the tools capability shows up.
    assert_eq!(result["protocolVersion"], json!("2025-11-25"));
    assert!(
        result["capabilities"]["tools"].is_object(),
        "tools capability declared"
    );
    assert!(
        result["capabilities"]["resources"].is_null(),
        "resources NOT declared in M0"
    );
    assert!(
        result["instructions"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "server_instructions present"
    );

    // tools/list advertises all tools (M0–M4).
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
            "close_account".to_string(),
            "declare_account".to_string(),
            "declare_commodity".to_string(),
            "echo".to_string(),
            "fund_project".to_string(),
            "get_account_balance".to_string(),
            "get_ap_aging".to_string(),
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
