//! The M0 walking-skeleton MCP server: a stdio `rmcp` handler advertising two
//! synthetic, backend-free tools (`status`, `echo`) — enough to prove Claude Cowork
//! can discover and invoke tools this server registers.
//!
//! Capabilities declared: **`tools` only** (no `resources`/`prompts` yet — M5). The
//! `initialize` override emits the handshake wire-log and negotiates the protocol
//! version via [`crate::protocol`].

use std::time::Instant;

use rmcp::handler::server::common::schema_for_type;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, InitializeRequestParams, InitializeResult, JsonObject,
    ProtocolVersion, ServerInfo,
};
use rmcp::service::{Peer, RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::hledger::amount::render_amounts;
use crate::hledger::{BalanceReport, Hledger, HledgerError, PINNED_VERSION, Transaction};

const SERVER_NAME: &str = "hledger-mcp";

/// Short `server_instructions` returned in `initialize`. M5 will replace this with a
/// pointer to the `ledger://session-context` resource; for M0 it is a static blurb.
///
/// Compiled in from a markdown file via `include_str!` (CLAUDE.md *Conventions*): static
/// prose lives as a real `.md` file (diffable/reviewable) but is baked into the binary at
/// compile time — no runtime file, single self-contained binary.
const INSTRUCTIONS: &str = include_str!("resources/instructions.md");

/// Arguments for the `echo` tool. Used for the **advertised input schema**; the
/// handler itself reads arguments leniently so a bad call returns an `isError`
/// result the model can self-correct from, rather than a JSON-RPC protocol error.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct EchoArgs {
    /// The message to echo back unchanged.
    pub message: String,
}

/// Arguments for `get_account_balance`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct AccountBalanceArgs {
    /// The account to report, e.g. `assets:checking`. Matches as an hledger account query
    /// (a prefix matches sub-accounts).
    pub account: String,
}

/// Arguments for `list_transactions`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListTransactionsArgs {
    /// Optional hledger query terms, **one term per array element** — e.g.
    /// `["assets:checking", "date:2026"]`. Each element is passed to hledger verbatim, so a
    /// multi-word value stays one term: `["desc:Acme Corp"]` queries that description, it is
    /// not split. Omit (or pass `[]`) to list every transaction.
    #[serde(default)]
    pub query: Option<Vec<String>>,
}

/// The MCP server handler.
#[derive(Clone)]
pub struct HledgerMcp {
    started: Instant,
    hledger: Hledger,
}

impl Default for HledgerMcp {
    fn default() -> Self {
        Self::new(Hledger::from_env(None))
    }
}

#[tool_router]
impl HledgerMcp {
    /// Construct a fresh server (uptime clock started) over the given hledger adapter.
    pub fn new(hledger: Hledger) -> Self {
        Self {
            started: Instant::now(),
            hledger,
        }
    }

    /// Report server health: name, version, the session's **negotiated** protocol version,
    /// and uptime. The negotiated version is read from the peer (the client's `initialize`),
    /// reduced to what actually reached the wire (`protocol::effective_version`) — not the
    /// server's newest. Falls back to our newest only if called with no peer (shouldn't
    /// happen: `initialize` precedes any tool call).
    #[tool(
        description = "Report server status: name, version, the session's negotiated protocol \
                         version, the detected hledger version (and whether it matches the \
                         pinned 1.52), and uptime in seconds. Takes no arguments."
    )]
    async fn status(&self, peer: Peer<RoleServer>) -> Result<CallToolResult, McpError> {
        let negotiated = peer
            .peer_info()
            .map(|info| crate::protocol::effective_version(&info.protocol_version))
            .unwrap_or_else(crate::protocol::latest_supported);
        let backend = self.backend_block().await;
        Ok(CallToolResult::success(vec![Content::text(
            self.status_text(&negotiated, &backend),
        )]))
    }

    /// Render the hledger backend block for `status`: the version verdict (parsed `major.minor`
    /// + pin match, plus the raw `--version` banner), the resolved binary path, and the journal
    /// in use. This is the operator's diagnostic for "which hledger / which ledger" — surfaced
    /// here (and logged at startup) so a wrong binary or journal is immediately visible. The
    /// M1 policy is warn-and-continue for reads, hard-gate before M2 writes.
    async fn backend_block(&self) -> String {
        let version = match self.hledger.version().await {
            Ok(v) if v.pin_matches() => {
                format!("hledger: {}.{} (pinned) — {:?}", v.major, v.minor, v.raw)
            }
            Ok(v) => format!(
                "hledger: {}.{} (MISMATCH — expected {}.{}) — {:?}",
                v.major, v.minor, PINNED_VERSION.0, PINNED_VERSION.1, v.raw
            ),
            Err(err) => format!("hledger: unavailable ({err})"),
        };
        let binary = format!("binary: {}", self.hledger.bin().display());
        let journal = match self.hledger.journal_path() {
            Some(path) => format!("journal: {}", path.display()),
            None => "journal: (none configured — set --journal or LEDGER_FILE)".to_string(),
        };
        format!("{version}\n{binary}\n{journal}")
    }

    /// Pure render of the `status` body (separated from peer/subprocess access so it is
    /// unit-testable without a live `Peer` or a real hledger). `backend` is the multi-line
    /// block from [`Self::backend_block`].
    fn status_text(&self, negotiated: &ProtocolVersion, backend: &str) -> String {
        format!(
            "{SERVER_NAME} {}\nprotocol: {negotiated}\n{backend}\nuptime: {}s",
            env!("CARGO_PKG_VERSION"),
            self.started.elapsed().as_secs(),
        )
    }

    /// Report an account's balance via `hledger balance <account> -O json`.
    #[tool(
        description = "Get the balance of an account from the ledger. Requires a string field \
                      `account` (e.g. `assets:checking`); a parent account sums its \
                      sub-accounts. Returns each matched account and the total.",
        input_schema = schema_for_type::<AccountBalanceArgs>()
    )]
    async fn get_account_balance(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: AccountBalanceArgs = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        match self.hledger.balance(Some(&args.account)).await {
            Ok(report) => Ok(CallToolResult::success(vec![Content::text(
                render_balance(&report),
            )])),
            Err(err) => Ok(adapter_error(&err)),
        }
    }

    /// List transactions matching an optional hledger query via `hledger print -O json`.
    #[tool(
        description = "List transactions from the ledger, optionally filtered by an hledger \
                      query (field `query`: an array of terms, one per element, e.g. \
                      [\"assets:checking\", \"date:2026\"]). Omit `query` to list all. \
                      Returns date, description, and postings for each match.",
        input_schema = schema_for_type::<ListTransactionsArgs>()
    )]
    async fn list_transactions(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: ListTransactionsArgs = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        let terms = args.query.unwrap_or_default();
        match self.hledger.list_transactions(&terms).await {
            Ok(txns) => Ok(CallToolResult::success(vec![Content::text(
                render_transactions(&txns),
            )])),
            Err(err) => Ok(adapter_error(&err)),
        }
    }

    /// Echo a message back unchanged — a minimal tool-invocation connectivity check.
    ///
    /// Extracts arguments via [`crate::tools::parse_args`] so a bad call yields a
    /// self-correctable `isError` result (with serde's accurate missing-vs-wrong-type
    /// message) rather than a JSON-RPC `invalid_params`. The derived [`EchoArgs`] schema is
    /// the single source of truth for both the advertised `input_schema` and validation.
    #[tool(
        description = "Echo a message back unchanged. A minimal connectivity / \
                      tool-invocation check. Requires a string field `message`.",
        input_schema = schema_for_type::<EchoArgs>()
    )]
    async fn echo(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: EchoArgs = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        Ok(CallToolResult::success(vec![Content::text(args.message)]))
    }
}

/// Map an adapter [`HledgerError`] to a tool-level `isError` result (so the model sees the
/// failure as a tool outcome it can react to, consistent with the `parse_args` convention) —
/// not a JSON-RPC protocol error. The error text is hledger's own diagnostic / our typed
/// message; it never includes journal contents.
fn adapter_error(err: &HledgerError) -> CallToolResult {
    CallToolResult::error(vec![Content::text(format!("hledger error: {err}"))])
}

/// Render a [`BalanceReport`] as a compact text table: one `account  amount` line per row,
/// then a `total  amount` line.
fn render_balance(report: &BalanceReport) -> String {
    let mut lines: Vec<String> = report
        .rows
        .iter()
        .map(|row| format!("{}  {}", row.account, render_amounts(&row.amounts)))
        .collect();
    if report.rows.is_empty() {
        lines.push("(no matching accounts)".to_string());
    }
    lines.push(format!("total  {}", render_amounts(&report.totals)));
    lines.join("\n")
}

/// Render a list of [`Transaction`]s as text: a `date description` header per transaction
/// followed by indented `account  amount` posting lines.
fn render_transactions(txns: &[Transaction]) -> String {
    if txns.is_empty() {
        return "(no matching transactions)".to_string();
    }
    let mut lines = Vec::new();
    for txn in txns {
        lines.push(format!("{} {}", txn.date, txn.description));
        for posting in &txn.postings {
            lines.push(format!(
                "    {}  {}",
                posting.account,
                render_amounts(&posting.amounts)
            ));
        }
    }
    lines.join("\n")
}

#[tool_handler]
impl ServerHandler for HledgerMcp {
    fn get_info(&self) -> ServerInfo {
        // Only the `tools` capability is enabled (M0). `InitializeResult` is
        // `#[non_exhaustive]`, so build it via its constructor + builder methods.
        let capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build();
        ServerInfo::new(capabilities)
            .with_server_info(rmcp::model::Implementation::new(
                SERVER_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    /// Handle `initialize`: emit the diagnostic handshake wire-log (the signal that
    /// distinguishes "client never connected" from "version mismatch" from "tools
    /// never listed" — see `docs/development/mcp-protocol-versions.md`), then respond
    /// with the negotiated protocol version.
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        let requested = request.protocol_version.clone();
        // What we *return*; rmcp then reconciles to `effective` on the wire (see protocol.rs).
        let preferred = crate::protocol::negotiate(&requested);
        // What the client will *actually* receive — log this so the diagnostic matches reality.
        let effective = crate::protocol::effective_version(&requested);
        let roots = request.capabilities.roots.is_some();

        tracing::info!(
            client.name = %request.client_info.name,
            client.version = %request.client_info.version,
            protocol.requested = %requested,
            protocol.negotiated = %effective,
            roots,
            "initialize",
        );

        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }

        Ok(self.get_info().with_protocol_version(preferred))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hledger::{AccountBalance, Amount, Posting, Quantity};

    fn args(json: serde_json::Value) -> Parameters<JsonObject> {
        Parameters(json.as_object().expect("test args are an object").clone())
    }

    /// A server whose adapter has no journal (and a bogus binary) — fine for the tools that
    /// don't touch hledger (`echo`) and for exercising the `NoJournal` error path.
    fn test_server() -> HledgerMcp {
        HledgerMcp::new(Hledger::new("hledger", None))
    }

    #[tokio::test]
    async fn echo_returns_message_on_success() {
        let server = test_server();
        let result = server
            .echo(args(serde_json::json!({ "message": "hello" })))
            .await
            .expect("echo dispatch");
        assert_eq!(result.is_error, Some(false));
        let text = result.content[0].as_text().expect("text content");
        assert_eq!(text.text, "hello");
    }

    #[tokio::test]
    async fn echo_missing_message_is_iserror_not_protocol_error() {
        let server = test_server();
        // Dispatch succeeds (no JSON-RPC error); the *result* is flagged isError so
        // the model can self-correct.
        let result = server
            .echo(args(serde_json::json!({ "msg": "typo'd key" })))
            .await
            .expect("echo dispatch must not raise a protocol error on bad args");
        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].as_text().expect("text content").text;
        assert!(
            text.contains("missing"),
            "missing-field error says missing: {text}"
        );
    }

    #[tokio::test]
    async fn echo_wrong_type_is_iserror_and_says_type_not_missing() {
        let server = test_server();
        let result = server
            .echo(args(serde_json::json!({ "message": 42 })))
            .await
            .expect("echo dispatch");
        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].as_text().expect("text content").text;
        // A present-but-wrong-typed field must NOT be reported as "missing" (serde gives
        // "invalid type: integer …, expected a string" via the shared parse_args helper).
        assert!(
            text.contains("invalid type"),
            "type error names the type: {text}"
        );
        assert!(
            !text.contains("missing"),
            "type error must not say missing: {text}"
        );
    }

    #[test]
    fn status_text_reports_name_version_negotiated_protocol_and_backend() {
        // `status` itself extracts the version from the live `Peer` and the backend block from
        // a subprocess; the render is tested here against explicit values to prove it reports
        // the *session's* protocol (not the newest) and embeds the backend block verbatim.
        let server = test_server();
        let backend = "hledger: 1.52 (pinned) — \"hledger 1.52, mac-aarch64\"\n\
                       binary: /store/bin/hledger\n\
                       journal: /tmp/x.journal";
        let text = server.status_text(&ProtocolVersion::V_2024_11_05, backend);
        assert!(
            text.contains(SERVER_NAME),
            "status names the server: {text}"
        );
        assert!(
            text.contains(env!("CARGO_PKG_VERSION")),
            "status reports the version: {text}"
        );
        assert!(
            text.contains("2024-11-05"),
            "status reports the negotiated protocol, not the newest: {text}"
        );
        assert!(
            text.contains("binary: /store/bin/hledger") && text.contains("journal: /tmp/x.journal"),
            "status embeds the backend block (binary + journal): {text}"
        );
    }

    #[test]
    fn get_info_declares_tools_only_and_instructions() {
        let info = test_server().get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability declared"
        );
        assert!(
            info.capabilities.resources.is_none(),
            "resources NOT declared in M0"
        );
        assert!(info.instructions.is_some(), "server_instructions present");
    }

    #[test]
    fn render_balance_lists_rows_and_total() {
        let report = BalanceReport {
            rows: vec![AccountBalance {
                account: "assets:checking".into(),
                amounts: vec![Amount {
                    commodity: "$".into(),
                    quantity: Quantity::new(8766, 2),
                    commodity_left: true,
                    spaced: false,
                }],
            }],
            totals: vec![Amount {
                commodity: "$".into(),
                quantity: Quantity::new(8766, 2),
                commodity_left: true,
                spaced: false,
            }],
        };
        let text = render_balance(&report);
        assert!(text.contains("assets:checking  $87.66"), "{text}");
        assert!(text.contains("total  $87.66"), "{text}");
    }

    #[test]
    fn render_balance_handles_no_rows() {
        let empty = BalanceReport {
            rows: vec![],
            totals: vec![],
        };
        let text = render_balance(&empty);
        assert!(text.contains("no matching accounts"), "{text}");
        assert!(text.contains("total  0"), "{text}");
    }

    #[test]
    fn render_transactions_shows_header_and_postings_or_empty() {
        assert!(render_transactions(&[]).contains("no matching transactions"));
        let txns = vec![Transaction {
            date: "2026-01-15".into(),
            description: "Acme".into(),
            index: 1,
            status: "Unmarked".into(),
            comment: String::new(),
            tags: vec![],
            postings: vec![Posting {
                account: "expenses:supplies".into(),
                amounts: vec![Amount {
                    commodity: "$".into(),
                    quantity: Quantity::new(1234, 2),
                    commodity_left: true,
                    spaced: false,
                }],
                comment: String::new(),
                tags: vec![],
            }],
        }];
        let text = render_transactions(&txns);
        assert!(text.contains("2026-01-15 Acme"), "{text}");
        assert!(text.contains("    expenses:supplies  $12.34"), "{text}");
    }

    #[test]
    fn adapter_error_is_flagged_iserror() {
        let result = adapter_error(&HledgerError::NoJournal);
        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].as_text().expect("text").text;
        assert!(text.contains("no journal"), "{text}");
    }

    #[tokio::test]
    async fn get_account_balance_without_journal_is_iserror() {
        let server = test_server();
        let result = server
            .get_account_balance(args(serde_json::json!({ "account": "assets:checking" })))
            .await
            .expect("dispatch");
        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].as_text().expect("text").text;
        assert!(text.contains("no journal"), "{text}");
    }

    #[tokio::test]
    async fn get_account_balance_missing_arg_is_iserror() {
        let server = test_server();
        let result = server
            .get_account_balance(args(serde_json::json!({})))
            .await
            .expect("dispatch");
        assert_eq!(result.is_error, Some(true));
        let text = &result.content[0].as_text().expect("text").text;
        assert!(text.contains("missing"), "{text}");
    }

    /// A server backed by the real fixture journal + the env-resolved hledger. `None` when
    /// hledger is unavailable (→ caller skips).
    async fn fixture_server() -> Option<HledgerMcp> {
        let journal = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample.journal"
        ));
        let hledger = Hledger::from_env(Some(journal));
        match hledger.version().await {
            Ok(_) => Some(HledgerMcp::new(hledger)),
            Err(_) => {
                eprintln!("SKIP server e2e: hledger not found (run inside `nix develop`)");
                None
            }
        }
    }

    #[tokio::test]
    async fn get_account_balance_success_against_fixture() {
        let Some(server) = fixture_server().await else {
            return;
        };
        let result = server
            .get_account_balance(args(serde_json::json!({ "account": "assets:checking" })))
            .await
            .expect("dispatch");
        assert_eq!(result.is_error, Some(false));
        let text = &result.content[0].as_text().expect("text").text;
        assert!(text.contains("assets:checking  $43.66"), "{text}");
        assert!(text.contains("total  $43.66"), "{text}");
    }

    #[tokio::test]
    async fn list_transactions_success_against_fixture() {
        let Some(server) = fixture_server().await else {
            return;
        };
        // With a query (array of terms).
        let filtered = server
            .list_transactions(args(serde_json::json!({ "query": ["expenses:supplies"] })))
            .await
            .expect("dispatch");
        assert_eq!(filtered.is_error, Some(false));
        let text = &filtered.content[0].as_text().expect("text").text;
        assert!(text.contains("2026-01-15 Acme"), "{text}");
        assert!(text.contains("    expenses:supplies  $12.34"), "{text}");
        // Without a query (None) lists everything.
        let all = server
            .list_transactions(args(serde_json::json!({})))
            .await
            .expect("dispatch");
        assert_eq!(all.is_error, Some(false));
    }

    #[tokio::test]
    async fn backend_block_reports_version_binary_and_journal_when_available() {
        let Some(server) = fixture_server().await else {
            return;
        };
        let block = server.backend_block().await;
        // Version verdict carries the raw --version banner; binary + journal paths are shown.
        assert!(block.contains("hledger: 1.52 (pinned)"), "{block}");
        assert!(
            block.contains("hledger 1.52"),
            "raw banner present: {block}"
        );
        assert!(block.contains("binary:"), "{block}");
        assert!(
            block.contains("journal:") && block.contains("sample.journal"),
            "journal path shown: {block}"
        );
    }

    #[tokio::test]
    async fn backend_block_reports_unavailable_and_no_journal_for_bogus_binary() {
        let server = HledgerMcp::new(Hledger::new("/nonexistent/hledger", None));
        let block = server.backend_block().await;
        assert!(block.contains("unavailable"), "{block}");
        assert!(block.contains("binary: /nonexistent/hledger"), "{block}");
        assert!(block.contains("none configured"), "{block}");
    }
}
