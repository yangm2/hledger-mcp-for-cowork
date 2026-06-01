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

    // tools/list advertises the M0 connectivity tools plus the M1 read tools.
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
            "echo".to_string(),
            "get_account_balance".to_string(),
            "list_transactions".to_string(),
            "status".to_string(),
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
    assert!(
        status_text.contains("hledger: 1.52 (pinned)"),
        "status reports the pinned hledger version: {status_text}"
    );
    assert!(
        status_text.contains("binary:") && status_text.contains("sample.journal"),
        "status reports the resolved binary and journal: {status_text}"
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
        "params": { "name": "list_transactions", "arguments": { "query": "expenses:supplies" } }
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
