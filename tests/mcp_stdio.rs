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
        let mut child = Command::new(env!("CARGO_BIN_EXE_hledger-mcp-for-cowork"))
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

    // tools/list advertises exactly `status` + `echo`.
    server.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let listed = server.recv();
    let mut names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name").to_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["echo".to_string(), "status".to_string()]);

    // tools/call echo round-trips the message.
    server.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "echo", "arguments": { "message": "ping" } }
    }));
    let called = server.recv();
    let result = &called["result"];
    assert_eq!(result["isError"], json!(false));
    assert_eq!(result["content"][0]["text"], json!("ping"));
}

#[test]
fn unknown_protocol_version_is_capped_not_echoed() {
    let mut server = Server::spawn();
    // A future RC the server has not validated must be capped to the newest supported,
    // never blind-echoed.
    let result = initialize(&mut server, "2026-07-28");
    assert_eq!(result["protocolVersion"], json!("2025-11-25"));
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
