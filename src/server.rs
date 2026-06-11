//! The stdio `rmcp` MCP server: read tools (M1), write tools (M2), and the epoch-CAS
//! concurrency layer (M3 — per-connection [`write::ConnectionView`], record-vs-decide via
//! [`write::ConnectionView::guarded`], soft-invariant flags).
//!
//! Capabilities declared: **`tools` only** (no `resources`/`prompts` yet — M5). The
//! `initialize` override emits the handshake wire-log and negotiates the protocol
//! version via [`crate::protocol`].

use std::sync::Arc;
use std::time::Instant;

use chrono::NaiveDate;

use rmcp::handler::server::common::schema_for_type;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, InitializeRequestParams, InitializeResult, JsonObject,
    ProtocolVersion, ServerInfo,
};
use rmcp::service::{Peer, RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::epoch::{Epoch, ToolClass};
use crate::hledger::amount::render_amounts;
use crate::hledger::{BalanceReport, Hledger, HledgerError, PINNED_VERSION, Transaction};
use std::ops::AsyncFnOnce;

use crate::write::{
    self, CommitOutcome, ConnectionView, WriteContext, WriteError, WriteOutcome,
    input::TransactionInput,
};

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

/// Arguments for `declare_account`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct DeclareAccountArgs {
    /// The account name to declare, e.g. `assets:checking` (colon-separated).
    pub account: String,
}

/// Arguments for `declare_commodity`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct DeclareCommodityArgs {
    /// The commodity symbol to declare, e.g. `$` or `EUR`.
    pub commodity: String,
    /// Decimal places for the display style (default 2).
    #[serde(default)]
    pub decimal_places: Option<u32>,
}

/// Arguments for `void_transaction`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct VoidTransactionArgs {
    /// The `id:` tag of the transaction to void (from a prior `post_transaction`).
    pub id: String,
}

/// Arguments for `update_transaction` (= void the target + post a replacement).
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct UpdateTransactionArgs {
    /// The `id:` tag of the transaction to replace.
    pub id: String,
    /// The replacement transaction (posted fresh; the original is reversed, not edited).
    pub transaction: TransactionInput,
}

/// Arguments for `close_account` (soft-delete / tombstone).
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct CloseAccountArgs {
    /// The declared account to close (tombstone), e.g. `liabilities:ap:oldvendor`.
    pub account: String,
}

/// Arguments for `fund_project`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct FundProjectArgs {
    /// Date of the funding (YYYY-MM-DD).
    #[schemars(with = "String")]
    pub date: chrono::NaiveDate,
    /// Amount deposited, e.g. `"50000.00"`.
    pub amount: String,
    /// Commodity symbol, e.g. `"$"`.
    #[schemars(with = "String")]
    pub commodity: crate::hledger::amount::Commodity,
    /// Optional idempotency key — reuse on retry to avoid a duplicate.
    #[serde(default)]
    pub idem: Option<String>,
}

/// Arguments for `receive_invoice`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct ReceiveInvoiceArgs {
    /// Invoice date (YYYY-MM-DD).
    #[schemars(with = "String")]
    pub date: chrono::NaiveDate,
    /// Vendor name, e.g. `"Acme"`.
    pub vendor: String,
    /// Expense account, e.g. `"expenses:construction:plumbing"` or
    /// `"expenses:professional - Bob Engineer"`. Use `vendor_add` to declare it first.
    pub expense_account: String,
    /// Invoice amount, e.g. `"8000.00"`.
    pub amount: String,
    /// Commodity symbol, e.g. `"$"`.
    #[schemars(with = "String")]
    pub commodity: crate::hledger::amount::Commodity,
    /// Vendor-assigned invoice reference, e.g. `"INV-001"`.
    pub invoice_ref: String,
    /// Optional idempotency key.
    #[serde(default)]
    pub idem: Option<String>,
}

/// Arguments for `pay_invoice`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct PayInvoiceArgs {
    /// Payment date (YYYY-MM-DD).
    #[schemars(with = "String")]
    pub date: chrono::NaiveDate,
    /// Vendor name matching the AP account, e.g. `"Acme"`.
    pub vendor: String,
    /// Amount paid, e.g. `"8000.00"`.
    pub amount: String,
    /// Commodity symbol, e.g. `"$"`.
    #[schemars(with = "String")]
    pub commodity: crate::hledger::amount::Commodity,
    /// Optional idempotency key.
    #[serde(default)]
    pub idem: Option<String>,
}

/// Arguments for `post_interest`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct PostInterestArgs {
    /// Date interest was earned (YYYY-MM-DD).
    #[schemars(with = "String")]
    pub date: chrono::NaiveDate,
    /// Interest amount, e.g. `"125.00"`.
    pub amount: String,
    /// Commodity symbol, e.g. `"$"`.
    #[schemars(with = "String")]
    pub commodity: crate::hledger::amount::Commodity,
    /// Optional idempotency key.
    #[serde(default)]
    pub idem: Option<String>,
}

/// The two vendor kinds, deciding which expense account `vendor_add` declares.
#[derive(Debug, Clone, Copy, serde::Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum VendorType {
    /// Shared trade expense account (`expenses:construction:{trade}`).
    Trade,
    /// Dedicated per-vendor account (`expenses:professional - {vendor}`).
    Professional,
}

/// Arguments for `vendor_add`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct VendorAddArgs {
    /// Vendor name, e.g. `"Acme"` or `"Bob Engineer"`.
    pub vendor: String,
    /// `"trade"` for a shared trade expense account (`expenses:construction:{trade}`), or
    /// `"professional"` for a dedicated per-vendor account (`expenses:professional - {vendor}`).
    pub vendor_type: VendorType,
    /// The trade/sub-trade name (e.g. `"plumbing"`). Required when `vendor_type` is `"trade"`.
    #[serde(default)]
    pub trade: Option<String>,
}

/// Arguments for `get_ap_aging`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct GetApAgingArgs {
    /// Reference date for age computation (YYYY-MM-DD). Defaults to today.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub as_of: Option<NaiveDate>,
}

/// The MCP server handler.
#[derive(Clone)]
pub struct HledgerMcp {
    started: Instant,
    hledger: Hledger,
    /// This connection's [`ConnectionView`] (M3): the per-connection last-seen epoch paired
    /// with the process-wide writer lock. Over stdio there is exactly one connection per server
    /// process, so one standalone view (the multi-connection directory — one `WriterLock`, one
    /// view per connection — arrives with HTTP, M6). All reads go through
    /// [`ConnectionView::grounded_read`], all writes through [`ConnectionView::guarded`]; both
    /// ordering disciplines live on the view, not here.
    view: Arc<ConnectionView>,
    /// When `Some`, writes are refused (with the reason) — set when startup reconciliation
    /// failed, so the working tree may hold unreconciled content a write would silently absorb
    /// into its commit. Self-healing: the next write attempt retries `reconcile` and clears
    /// this on success.
    write_block: Arc<tokio::sync::Mutex<Option<String>>>,
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
            view: Arc::new(ConnectionView::default()),
            write_block: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Block writes with `reason` until a reconcile retry succeeds (builder, for startup:
    /// `main` sets this when crash reconciliation fails — see `write_block`).
    pub fn with_write_block(self, reason: Option<String>) -> Self {
        Self {
            write_block: Arc::new(tokio::sync::Mutex::new(reason)),
            ..self
        }
    }

    /// Sample the current epoch (journal repo `HEAD`) — `None` when no journal is configured or
    /// the repo can't be read.
    fn sample_epoch(&self) -> Option<Epoch> {
        let journal = self.hledger.journal_path()?;
        write::current_epoch(journal).ok()
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
        let detected = self.hledger.version().await; // one subprocess; reused below
        let pinned = matches!(&detected, Ok(v) if v.pin_matches());
        let version = match &detected {
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
        let (journal, git) = match self.hledger.journal_path() {
            Some(path) => (
                format!("journal: {}", path.display()),
                crate::write::git_status_line(path),
            ),
            None => (
                "journal: (none configured — set --journal or LEDGER_FILE)".to_string(),
                "git: (no journal configured)".to_string(),
            ),
        };
        // The epoch story (M3): current HEAD vs what this connection last read.
        let epoch = match self.sample_epoch() {
            Some(head) => {
                let seen = self.view.last_seen().await;
                let connection = match &seen {
                    Some(seen) if *seen == head => format!("last-seen {} (fresh)", seen.short()),
                    Some(seen) => format!("last-seen {} (STALE — re-read)", seen.short()),
                    None => "no read yet this connection".to_string(),
                };
                format!("epoch: {} — {connection}", head.short())
            }
            None => "epoch: (no journal configured)".to_string(),
        };
        // Writes need the pinned hledger, a journal, and a reconciled tree.
        let writes = if detected.is_err() {
            "writes: blocked (hledger unavailable)".to_string()
        } else if !pinned {
            "writes: BLOCKED (hledger not pinned 1.52)".to_string()
        } else if let Some(reason) = self.write_block.lock().await.as_deref() {
            format!("writes: BLOCKED ({reason}; a write attempt retries the reconcile)")
        } else if self.hledger.has_journal() {
            "writes: enabled".to_string()
        } else {
            "writes: blocked (no journal configured)".to_string()
        };
        format!("{version}\n{binary}\n{journal}\n{git}\n{epoch}\n{writes}")
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

    /// Report an account's balance via `hledger balance <account> -O json`. **Read** — bumps
    /// this connection's last-seen epoch; soft-invariant flags (overdraft) are surfaced in the
    /// output, never enforced (C-6).
    #[tool(
        description = "Get the balance of an account from the ledger. Requires a string field \
                      `account` (e.g. `assets:checking`); a parent account sums its \
                      sub-accounts. Returns each matched account and the total, plus any \
                      soft-invariant flags (e.g. an overdrawn asset account).",
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
        let result = self
            .view
            .grounded_read(&self.hledger, || self.hledger.balance(Some(&args.account)))
            .await;
        match result {
            Ok(report) => {
                let mut text = render_balance(&report);
                let flags = crate::flags::overdraft_flags(&report);
                if !flags.is_empty() {
                    text.push('\n');
                    text.push_str(&crate::flags::render_flags(&flags));
                }
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(err) => Ok(adapter_error(&err)),
        }
    }

    /// List transactions matching an optional hledger query via `hledger print -O json`.
    /// **Read** — bumps this connection's last-seen epoch.
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
        let result = self
            .view
            .grounded_read(&self.hledger, || self.hledger.list_transactions(&terms))
            .await;
        match result {
            Ok(txns) => Ok(CallToolResult::success(vec![Content::text(
                render_transactions(&txns),
            )])),
            Err(err) => Ok(adapter_error(&err)),
        }
    }

    /// Run a write tool through this connection's [`ConnectionView::guarded`], rendering the
    /// outcome — the single dispatch point where a tool's [`ToolClass`] takes effect (and
    /// where every write-tool body collapses to one call). `op` receives a [`WriteContext`]
    /// that carries the resolved journal, the hledger adapter, and proof the gate ran.
    ///
    /// If writes are blocked (startup reconciliation failed), retries the reconcile first and
    /// self-heals on success; otherwise refuses with the reason — a write against an
    /// unreconciled tree would silently absorb foreign content into its commit.
    async fn guarded_tool<T, F>(
        &self,
        class: ToolClass,
        op: F,
        render: impl FnOnce(&T) -> String,
    ) -> Result<CallToolResult, McpError>
    where
        F: AsyncFnOnce(WriteContext<'_>) -> Result<T, WriteError>,
    {
        {
            let mut block = self.write_block.lock().await;
            if let Some(reason) = block.clone() {
                match write::reconcile(&self.hledger).await {
                    Ok(_) => {
                        tracing::warn!("reconcile retry succeeded; writes unblocked");
                        *block = None;
                    }
                    Err(err) => {
                        return Ok(write_error_result(WriteError::Refused(format!(
                            "writes blocked: {reason} (reconcile retry failed: {err})"
                        ))));
                    }
                }
            }
        }
        let result = self.view.guarded(&self.hledger, class, op).await;
        match result {
            Ok(out) => Ok(CallToolResult::success(vec![Content::text(render(&out))])),
            Err(err) => Ok(write_error_result(err)),
        }
    }

    /// Declare an account so transactions may post to it (require-pre-declare). **Record** —
    /// append-only directive, no epoch check.
    #[tool(
        description = "Declare an account so transactions can post to it. The ledger requires \
                      accounts to be declared before use. Requires a string field `account` \
                      (e.g. `assets:checking`).",
        input_schema = schema_for_type::<DeclareAccountArgs>()
    )]
    async fn declare_account(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: DeclareAccountArgs = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        let account = args.account.clone();
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| write::declare_account(&ctx, &account).await,
            |out: &CommitOutcome| {
                format!(
                    "declared account '{}' (commit {})",
                    out.id,
                    out.commit.short()
                )
            },
        )
        .await
    }

    /// Close (tombstone) an account — **soft-delete**: the account stays declared, history and
    /// even new postings to it still resolve; nothing is ever hard-deleted. **Record** —
    /// append-only directive, no epoch check.
    #[tool(
        description = "Close (tombstone) an account — a soft delete. The account stays declared \
                      and its history remains valid; it is marked closed rather than removed. \
                      Requires a string field `account`.",
        input_schema = schema_for_type::<CloseAccountArgs>()
    )]
    async fn close_account(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: CloseAccountArgs = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        let account = args.account.clone();
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| write::tombstone_account(&ctx, &account).await,
            |out: &CommitOutcome| {
                format!(
                    "closed (tombstoned) account '{}' (commit {})",
                    out.id,
                    out.commit.short()
                )
            },
        )
        .await
    }

    /// Declare a commodity so amounts may use it (require-pre-declare). **Record** —
    /// append-only directive, no epoch check.
    #[tool(
        description = "Declare a commodity so amounts can use it. Requires a string field \
                      `commodity` (e.g. `$` or `EUR`); optional `decimal_places` (default 2).",
        input_schema = schema_for_type::<DeclareCommodityArgs>()
    )]
    async fn declare_commodity(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: DeclareCommodityArgs = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        let places = args.decimal_places.unwrap_or(2);
        let commodity = args.commodity.clone();
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| write::declare_commodity(&ctx, &commodity, places).await,
            |out: &CommitOutcome| {
                format!(
                    "declared commodity '{}' ({} dp, commit {})",
                    out.id,
                    places,
                    out.commit.short()
                )
            },
        )
        .await
    }

    /// Post a balanced transaction (validate → check --strict → atomic write → git commit).
    /// **Record** — append-only with a transaction-local balance invariant and an idempotency
    /// key, safe at any epoch; never epoch-checked (C-2/C-6).
    #[tool(
        description = "Post a transaction to the ledger. Provide `date` (YYYY-MM-DD), \
                      `description`, and `postings` (>=2; at most one may omit `amount` to \
                      balance). Accounts and commodities must be declared first. Optional \
                      `idem` (idempotency key — reuse it on a retry to avoid a duplicate). \
                      One validated write = one git commit.",
        input_schema = schema_for_type::<TransactionInput>()
    )]
    async fn post_transaction(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let input: TransactionInput = match crate::tools::parse_args(raw) {
            Ok(input) => input,
            Err(err) => return Ok(err),
        };
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| write::post_transaction(&ctx, input).await,
            post_outcome_text,
        )
        .await
    }

    /// Void a transaction by posting a reversing entry (append-only correction). **Record** —
    /// corrections are reversal posts, not decisions; never epoch-checked.
    #[tool(
        description = "Void a transaction by posting a reversing entry that negates it (the \
                      original is never edited or removed — append-only). Requires the `id` tag \
                      of the transaction to void.",
        input_schema = schema_for_type::<VoidTransactionArgs>()
    )]
    async fn void_transaction(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: VoidTransactionArgs = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        let id = args.id.clone();
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| write::void_transaction(&ctx, &id).await,
            |outcome: &WriteOutcome| {
                format!(
                    "voided '{}' with reversing entry id:{} (commit {})",
                    args.id,
                    outcome.base.id,
                    outcome.base.commit.short()
                )
            },
        )
        .await
    }

    /// Replace a transaction: void the original and post a replacement (two entries, no edit).
    /// **Record** — a correction (void + re-post), not a decision; never epoch-checked. (The
    /// first *decide*-classified tool arrives with the M4/M5 domain surface, e.g.
    /// `eco_approve`; the CAS mechanism it will use is [`ConnectionView::guarded`].)
    #[tool(
        description = "Replace a transaction: void the original (reversing entry) and post a \
                      replacement. This is two appended transactions, not an in-place edit. \
                      Requires `id` (the target's id tag) and `transaction` (the replacement).",
        input_schema = schema_for_type::<UpdateTransactionArgs>()
    )]
    async fn update_transaction(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: UpdateTransactionArgs = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        let id = args.id.clone();
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| write::update_transaction(&ctx, &id, args.transaction).await,
            |outcome: &WriteOutcome| {
                format!(
                    "updated: voided '{}', posted replacement id:{} (commit {})",
                    args.id,
                    outcome.base.id,
                    outcome.base.commit.short()
                )
            },
        )
        .await
    }

    // ---- M4 domain tools (all Record) --------------------------------------------------

    /// Fund a construction project: deposit owner capital into `assets:checking`.
    #[tool(
        description = "Fund the project: record an owner capital deposit into checking. \
                      Requires `date` (YYYY-MM-DD), `amount` (e.g. \"50000.00\"), \
                      `commodity` (e.g. \"$\"). Optional `idem` for idempotent retry. \
                      One validated write = one git commit.",
        input_schema = schema_for_type::<FundProjectArgs>()
    )]
    async fn fund_project(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: FundProjectArgs = match crate::tools::parse_args(raw) {
            Ok(a) => a,
            Err(e) => return Ok(e),
        };
        let date = args.date;
        let amount = args.amount;
        let commodity = args.commodity;
        let idem = args.idem;
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| {
                let input = crate::domain::fund_project_input(date, amount, commodity, idem);
                write::post_transaction(&ctx, input).await
            },
            post_outcome_text,
        )
        .await
    }

    /// Receive a vendor invoice: debit the expense account, credit the vendor AP account.
    #[tool(
        description = "Record a received invoice from a vendor. Requires `date` (YYYY-MM-DD), \
                      `vendor` (name), `expense_account` (e.g. \
                      \"expenses:construction:plumbing\" or \"expenses:professional - Bob\"), \
                      `amount`, `commodity`, `invoice_ref` (vendor invoice number). Optional `idem`. \
                      Declare accounts with `vendor_add` first.",
        input_schema = schema_for_type::<ReceiveInvoiceArgs>()
    )]
    async fn receive_invoice(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: ReceiveInvoiceArgs = match crate::tools::parse_args(raw) {
            Ok(a) => a,
            Err(e) => return Ok(e),
        };
        let date = args.date;
        let vendor = args.vendor;
        let expense_account = args.expense_account;
        let amount = args.amount;
        let commodity = args.commodity;
        let invoice_ref = args.invoice_ref;
        let idem = args.idem;
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| {
                let input = crate::domain::receive_invoice_input(
                    date,
                    &vendor,
                    expense_account,
                    amount,
                    commodity,
                    invoice_ref,
                    idem,
                );
                write::post_transaction(&ctx, input).await
            },
            post_outcome_text,
        )
        .await
    }

    /// Pay a vendor invoice: debit the vendor AP account, credit `assets:checking`.
    #[tool(
        description = "Record a payment to a vendor — clears the AP liability for that vendor. \
                      Requires `date` (YYYY-MM-DD), `vendor` (name), `amount`, `commodity`. \
                      Optional `idem`.",
        input_schema = schema_for_type::<PayInvoiceArgs>()
    )]
    async fn pay_invoice(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: PayInvoiceArgs = match crate::tools::parse_args(raw) {
            Ok(a) => a,
            Err(e) => return Ok(e),
        };
        let date = args.date;
        let vendor = args.vendor;
        let amount = args.amount;
        let commodity = args.commodity;
        let idem = args.idem;
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| {
                let ap_account = crate::domain::vendor_ap_account(&vendor);
                let balance = ctx
                    .hledger
                    .balance_flat(Some(&ap_account))
                    .await
                    .map_err(|e| WriteError::Internal(format!("AP balance query: {e}")))?;
                let has_outstanding = balance
                    .rows
                    .iter()
                    .any(|row| row.amounts.iter().any(|a| a.quantity.mantissa < 0));
                if !has_outstanding {
                    return Err(WriteError::Input(format!(
                        "vendor '{vendor}' has no outstanding AP balance"
                    )));
                }
                let input =
                    crate::domain::pay_invoice_input(date, &vendor, amount, commodity, idem);
                write::post_transaction(&ctx, input).await
            },
            post_outcome_text,
        )
        .await
    }

    /// Post interest earned: debit `assets:checking`, credit `income:interest`.
    #[tool(
        description = "Record interest earned on the project checking account. Requires \
                      `date` (YYYY-MM-DD), `amount`, `commodity`. Optional `idem`.",
        input_schema = schema_for_type::<PostInterestArgs>()
    )]
    async fn post_interest(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: PostInterestArgs = match crate::tools::parse_args(raw) {
            Ok(a) => a,
            Err(e) => return Ok(e),
        };
        let date = args.date;
        let amount = args.amount;
        let commodity = args.commodity;
        let idem = args.idem;
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| {
                let input = crate::domain::post_interest_input(date, amount, commodity, idem);
                write::post_transaction(&ctx, input).await
            },
            post_outcome_text,
        )
        .await
    }

    /// Declare a vendor: register both its AP account and its expense account in one commit.
    #[tool(
        description = "Declare a vendor — registers the AP account \
                      (`liabilities:ap:vendor:{vendor}`) and the expense account in one commit. \
                      Requires `vendor` (name), `vendor_type` (\"trade\" or \"professional\"). \
                      For \"trade\", also supply `trade` (sub-trade name, e.g. \"plumbing\"); \
                      shared expense account `expenses:construction:{trade}`. \
                      For \"professional\", a dedicated `expenses:professional - {vendor}` is used.",
        input_schema = schema_for_type::<VendorAddArgs>()
    )]
    async fn vendor_add(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: VendorAddArgs = match crate::tools::parse_args(raw) {
            Ok(a) => a,
            Err(e) => return Ok(e),
        };
        let expense_account = match args.vendor_type {
            VendorType::Trade => {
                let trade = match args.trade {
                    Some(t) => t,
                    None => {
                        return Ok(CallToolResult::error(vec![Content::text(
                            "input error: `trade` is required when vendor_type is \"trade\"",
                        )]));
                    }
                };
                crate::domain::trade_expense_account(&trade)
            }
            VendorType::Professional => crate::domain::professional_expense_account(&args.vendor),
        };
        let ap_account = crate::domain::vendor_ap_account(&args.vendor);
        let vendor = args.vendor;
        self.guarded_tool(
            ToolClass::Record,
            async |ctx| write::vendor_add(&ctx, &vendor, &ap_account, &expense_account).await,
            |out: &CommitOutcome| {
                format!(
                    "declared vendor '{}': AP account + expense account (commit {})",
                    out.id,
                    out.commit.short()
                )
            },
        )
        .await
    }

    /// List declared vendor AP accounts (`liabilities:ap:vendor:*`). **Read**.
    #[tool(
        description = "List all declared vendor AP accounts (liabilities:ap:vendor:*). \
                      Returns vendor names and their AP account paths. Takes no arguments."
    )]
    async fn vendor_list(
        &self,
        Parameters(_raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let result = self
            .view
            .grounded_read(&self.hledger, || self.hledger.declared_accounts())
            .await;
        match result {
            Ok(accounts) => {
                let vendors: Vec<&str> = accounts
                    .iter()
                    .filter(|a| a.starts_with("liabilities:ap:vendor:"))
                    .map(String::as_str)
                    .collect();
                let text = if vendors.is_empty() {
                    "(no vendors declared — use vendor_add first)".to_string()
                } else {
                    vendors.join("\n")
                };
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(err) => Ok(adapter_error(&err)),
        }
    }

    /// AP aging report: outstanding payables bucketed by age. **Read** — soft-invariant flags
    /// (90+ days overdue) are surfaced alongside the report, never enforced (C-6).
    #[tool(
        description = "AP aging report: outstanding vendor payables bucketed as current (0-30), \
                      31-60, 61-90, or 90+ days. Optional `as_of` date (YYYY-MM-DD, defaults to \
                      today). Flags any 90+-day overdue balances as soft-invariant warnings.",
        input_schema = schema_for_type::<GetApAgingArgs>()
    )]
    async fn get_ap_aging(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args: GetApAgingArgs = match crate::tools::parse_args(raw) {
            Ok(a) => a,
            Err(e) => return Ok(e),
        };
        let as_of = args.as_of.unwrap_or_else(crate::domain::today);
        let hledger = &self.hledger;
        let ap_query = "liabilities:ap".to_string();
        let result = self
            .view
            .grounded_read(&self.hledger, || async move {
                let balance = hledger.balance_flat(Some("liabilities:ap")).await?;
                let txns = hledger.list_transactions(&[ap_query]).await?;
                Ok::<_, HledgerError>((balance, txns))
            })
            .await;
        match result {
            Ok((balance, txns)) => {
                let entries = crate::domain::compute_ap_aging(&balance, &txns, as_of);
                let mut text = crate::domain::render_ap_aging(&entries, as_of);
                let flags = crate::flags::ap_aging_flags(&entries);
                if !flags.is_empty() {
                    text.push('\n');
                    text.push_str(&crate::flags::render_flags(&flags));
                }
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(err) => Ok(adapter_error(&err)),
        }
    }

    /// Project summary: balance sheet + income statement. **Read**.
    #[tool(
        description = "Project financial summary: balance sheet (assets, liabilities, net) and \
                      income statement (revenues, expenses). Takes no arguments."
    )]
    async fn get_project_summary(
        &self,
        Parameters(_raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let hledger = &self.hledger;
        let result = self
            .view
            .grounded_read(&self.hledger, || async move {
                let bs = hledger.balancesheet().await?;
                let is = hledger.incomestatement().await?;
                Ok::<_, HledgerError>((bs, is))
            })
            .await;
        match result {
            Ok((bs, is)) => {
                let text = format!(
                    "{}\n\n{}",
                    crate::domain::render_composite(&bs),
                    crate::domain::render_composite(&is),
                );
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
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

/// Human-facing result text for a `post_transaction` outcome (handles the deduped case).
fn post_outcome_text(outcome: &WriteOutcome) -> String {
    if outcome.deduped {
        format!(
            "already posted (idempotent): transaction id:{} — no new commit (HEAD {})",
            outcome.base.id,
            outcome.base.commit.short()
        )
    } else {
        format!(
            "posted transaction id:{} (commit {})",
            outcome.base.id,
            outcome.base.commit.short()
        )
    }
}

/// Map a [`WriteError`] to a tool-level `isError` result. Internal (our-bug) errors are logged
/// loudly here too; the `Display` text already carries the `input:`/`refused:`/`internal:`
/// prefix the model can act on.
fn write_error_result(err: WriteError) -> CallToolResult {
    if matches!(err, WriteError::Internal(_)) {
        tracing::error!(%err, "write path returned an internal error");
    }
    CallToolResult::error(vec![Content::text(err.to_string())])
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

    /// Resolve a runnable hledger for write-path tests, else `None` (test skips).
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

    /// A write-blocked server self-heals: the first write attempt retries the reconcile
    /// (healthy journal → success), clears the block, and the write proceeds. `status`
    /// reports BLOCKED before and enabled after.
    #[tokio::test]
    async fn write_block_self_heals_on_successful_reconcile_retry() {
        let Some(bin) = hledger_bin() else { return };
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = dir.path().join("main.journal");
        let server = HledgerMcp::new(Hledger::new(bin, Some(journal)))
            .with_write_block(Some("startup reconciliation failed: synthetic".into()));

        assert!(
            server.backend_block().await.contains("writes: BLOCKED"),
            "status reports the block"
        );

        // The journal is healthy (reconcile retry will succeed) → the write self-heals.
        let result = server
            .declare_commodity(args(serde_json::json!({ "commodity": "$" })))
            .await
            .expect("dispatch");
        assert_eq!(result.is_error, Some(false), "{result:?}");
        assert!(server.write_block.lock().await.is_none(), "block cleared");
        assert!(
            server.backend_block().await.contains("writes: enabled"),
            "status reports enabled after the self-heal"
        );
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
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
            description: "Acme".into(),
            index: 1,
            status: crate::hledger::Status::Unmarked,
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

    #[test]
    fn post_outcome_text_distinguishes_deduped() {
        use crate::epoch::CommitOid;
        let fresh = WriteOutcome {
            base: CommitOutcome {
                id: "i1".into(),
                commit: CommitOid::new("deadbeefcafe0000".into()),
            },
            deduped: false,
        };
        assert!(post_outcome_text(&fresh).starts_with("posted transaction id:i1"));
        let dup = WriteOutcome {
            deduped: true,
            ..fresh
        };
        assert!(post_outcome_text(&dup).contains("already posted (idempotent)"));
    }

    #[test]
    fn write_error_result_is_iserror_with_prefix() {
        let r = write_error_result(WriteError::Input("bad".into()));
        assert_eq!(r.is_error, Some(true));
        assert!(
            r.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("input error: bad")
        );
        // Internal variant also flagged isError (and logged loudly).
        let r2 = write_error_result(WriteError::Internal("boom".into()));
        assert_eq!(r2.is_error, Some(true));
    }

    #[tokio::test]
    async fn write_tools_refuse_without_journal() {
        // No journal configured → the write path refuses before touching anything.
        let server = test_server();
        for result in [
            server
                .declare_account(args(serde_json::json!({ "account": "assets:checking" })))
                .await
                .expect("dispatch"),
            server
                .declare_commodity(args(serde_json::json!({ "commodity": "$" })))
                .await
                .expect("dispatch"),
            server
                .void_transaction(args(serde_json::json!({ "id": "abc" })))
                .await
                .expect("dispatch"),
            server
                .post_transaction(args(serde_json::json!({
                    "date": "2026-01-01",
                    "description": "x",
                    "postings": [
                        { "account": "a:b", "amount": { "quantity": "1.00", "commodity": "$" } },
                        { "account": "c:d" }
                    ]
                })))
                .await
                .expect("dispatch"),
        ] {
            assert_eq!(result.is_error, Some(true));
            let text = &result.content[0].as_text().expect("text").text;
            assert!(text.contains("refused"), "expected refusal: {text}");
        }
    }

    /// Build a server backed by a fresh tempdir journal (commodity + two accounts declared),
    /// returning it alongside the tempdir guard (dropped → journal deleted). Skips when
    /// hledger is absent.
    async fn write_server(bin: &str) -> (HledgerMcp, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = dir.path().join("main.journal");
        let server = HledgerMcp::new(Hledger::new(bin, Some(journal)));
        server
            .declare_commodity(args(serde_json::json!({ "commodity": "$" })))
            .await
            .expect("declare $");
        server
            .declare_account(args(serde_json::json!({ "account": "assets:checking" })))
            .await
            .expect("declare assets:checking");
        server
            .declare_account(args(serde_json::json!({ "account": "equity:opening" })))
            .await
            .expect("declare equity:opening");
        (server, dir)
    }

    #[tokio::test]
    async fn close_account_tombstones_declared_account() {
        let Some(bin) = hledger_bin() else { return };
        let (server, _dir) = write_server(&bin).await;
        let result = server
            .close_account(args(serde_json::json!({ "account": "assets:checking" })))
            .await
            .expect("dispatch");
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let text = &result.content[0].as_text().expect("text").text;
        assert!(
            text.contains("tombstoned") && text.contains("assets:checking"),
            "close_account must report the tombstoned account: {text}"
        );
        assert!(
            text.contains("commit "),
            "result carries a commit oid: {text}"
        );
    }

    #[tokio::test]
    async fn update_transaction_posts_replacement_and_voids_original() {
        let Some(bin) = hledger_bin() else { return };
        let (server, _dir) = write_server(&bin).await;
        // Post the original.
        let posted = server
            .post_transaction(args(serde_json::json!({
                "date": "2026-01-01",
                "description": "original",
                "postings": [
                    { "account": "assets:checking", "amount": { "quantity": "10.00", "commodity": "$" } },
                    { "account": "equity:opening" }
                ]
            })))
            .await
            .expect("post");
        assert_eq!(posted.is_error, Some(false), "{posted:?}");
        let posted_text = &posted.content[0].as_text().expect("text").text;
        // Extract the id from "posted transaction id:<id> …".
        let id = posted_text
            .split("id:")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .expect("id in post result");

        // Update: void + replacement.
        let result = server
            .update_transaction(args(serde_json::json!({
                "id": id,
                "transaction": {
                    "date": "2026-01-02",
                    "description": "replacement",
                    "postings": [
                        { "account": "assets:checking", "amount": { "quantity": "20.00", "commodity": "$" } },
                        { "account": "equity:opening" }
                    ]
                }
            })))
            .await
            .expect("update dispatch");
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let text = &result.content[0].as_text().expect("text").text;
        assert!(
            text.contains("updated:") && text.contains("voided"),
            "update_transaction must report void + replacement: {text}"
        );
    }

    #[tokio::test]
    async fn grounded_read_updates_last_seen_visible_in_backend_block() {
        let Some(server) = fixture_server().await else {
            return;
        };
        // Before any read the epoch line shows "no read yet".
        let before = server.backend_block().await;
        assert!(
            before.contains("no read yet this connection"),
            "pre-read: {before}"
        );
        // A grounded read (via get_account_balance) bumps last_seen.
        server
            .get_account_balance(args(serde_json::json!({ "account": "assets:checking" })))
            .await
            .expect("dispatch");
        let after = server.backend_block().await;
        assert!(
            after.contains("(fresh)"),
            "after a read the epoch must show fresh: {after}"
        );
        assert!(
            !after.contains("no read yet"),
            "after a read there must be a last-seen: {after}"
        );
    }

    #[tokio::test]
    async fn post_transaction_bad_args_is_iserror_before_dispatch() {
        let server = test_server();
        // Missing required fields → parse-level input error (not a protocol error).
        let result = server
            .post_transaction(args(serde_json::json!({ "description": "x" })))
            .await
            .expect("dispatch");
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("invalid arguments"),
            "{:?}",
            result.content[0]
        );
    }
}
