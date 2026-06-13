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
    pub decimal_places: Option<u8>,
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

/// The money-write argument block shared by every M4 domain tool, inlined into each tool's
/// arg struct via `#[serde(flatten)]` — on the wire (and in the advertised schema) the fields
/// stay flat. `date` deliberately stays **outside** this struct, per-tool: its doc comment
/// becomes the schema description the model reads, and "Invoice date" vs "Payment date" is
/// load-bearing guidance.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct MoneyArgs {
    /// Exact decimal amount as a string, e.g. `"8000.00"` (never a JSON number).
    pub amount: String,
    /// Commodity symbol, e.g. `"$"`.
    #[schemars(with = "String")]
    pub commodity: crate::hledger::amount::Commodity,
    /// Optional idempotency key — reuse on retry to avoid a duplicate.
    #[serde(default)]
    pub idem: Option<String>,
}

/// Arguments for `fund_project`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct FundProjectArgs {
    /// Date of the funding (YYYY-MM-DD).
    #[schemars(with = "String")]
    pub date: chrono::NaiveDate,
    #[serde(flatten)]
    pub money: MoneyArgs,
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
    /// Vendor-assigned invoice reference, e.g. `"INV-001"`.
    pub invoice_ref: String,
    #[serde(flatten)]
    pub money: MoneyArgs,
}

/// Arguments for `pay_invoice`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct PayInvoiceArgs {
    /// Payment date (YYYY-MM-DD).
    #[schemars(with = "String")]
    pub date: chrono::NaiveDate,
    /// Vendor name matching the AP account, e.g. `"Acme"`.
    pub vendor: String,
    #[serde(flatten)]
    pub money: MoneyArgs,
}

/// Arguments for `post_interest`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct PostInterestArgs {
    /// Date interest was earned (YYYY-MM-DD).
    #[schemars(with = "String")]
    pub date: chrono::NaiveDate,
    #[serde(flatten)]
    pub money: MoneyArgs,
}

/// Arguments for `vendor_add`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct VendorAddArgs {
    /// Vendor name, e.g. `"Acme"` or `"Bob Engineer"`.
    pub vendor: String,
    /// `"trade"` for a shared trade expense account (`expenses:construction:{trade}`), or
    /// `"professional"` for a dedicated per-vendor account (`expenses:professional - {vendor}`).
    pub vendor_type: crate::domain::VendorType,
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

/// Arguments for `budget_set`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct BudgetSetArgs {
    /// The (declared) account the goal applies to, e.g. `expenses:construction:plumbing`.
    pub account: String,
    /// Goal period: `daily` | `weekly` | `monthly` | `quarterly` | `yearly`.
    pub period: write::budget::BudgetPeriod,
    /// Exact decimal goal amount as a string, e.g. `"500.00"`.
    pub amount: String,
    /// Commodity symbol, e.g. `"$"`.
    #[schemars(with = "String")]
    pub commodity: crate::hledger::amount::Commodity,
}

/// Arguments for `get_budget_vs_actual`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct BudgetVsActualArgs {
    /// Optional account query scoping the report (e.g. `expenses:construction`); omit for all.
    #[serde(default)]
    pub account: Option<String>,
}

/// Arguments for `eco_create`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct EcoCreateArgs {
    /// Your change-order number/id, e.g. `"7"` or `"ECO-7"` — the `eco:` tag value.
    pub eco: String,
    /// The trade the CO belongs to, e.g. `"electrical"` (`pending` is reserved).
    pub trade: String,
    /// The vendor billing the CO (usually the GC) — must be declared via `vendor_add`.
    pub vendor: String,
    /// What the change order covers.
    pub description: String,
    /// CO date (YYYY-MM-DD).
    #[schemars(with = "String")]
    pub date: chrono::NaiveDate,
    #[serde(flatten)]
    pub money: MoneyArgs,
}

/// Arguments for `eco_approve` / `eco_void`.
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct EcoRefArgs {
    /// The change-order id used at `eco_create` (the `eco:` tag value).
    pub eco: String,
    /// Event date (YYYY-MM-DD).
    #[schemars(with = "String")]
    pub date: chrono::NaiveDate,
}

/// The MCP server handler.
#[derive(Clone)]
pub struct HledgerMcp {
    started: Instant,
    hledger: Hledger,
    /// The advertising profile (`--profile`, MC-10): filters `tools/list` only — dispatch
    /// always runs against the full catalog.
    profile: crate::catalog::Profile,
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
            profile: crate::catalog::Profile::default(),
            view: Arc::new(ConnectionView::default()),
            write_block: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Select the advertising profile (builder; `main` passes `--profile`). Filters what
    /// `tools/list` advertises — every tool stays callable regardless (MC-10).
    pub fn with_profile(self, profile: crate::catalog::Profile) -> Self {
        Self { profile, ..self }
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
        let version = version_line(&detected);
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
            Some(head) => epoch_line(&head, self.view.last_seen().await.as_ref()),
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
            "{SERVER_NAME} {}\nprotocol: {negotiated}\nprofile: {}\n{backend}\nuptime: {}s",
            env!("CARGO_PKG_VERSION"),
            self.profile,
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
        self.read_args_tool(
            raw,
            async |hledger, args: AccountBalanceArgs| hledger.balance(Some(&args.account)).await,
            balance_text,
        )
        .await
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
        self.read_args_tool(
            raw,
            async |hledger, args: ListTransactionsArgs| {
                hledger
                    .list_transactions(&args.query.unwrap_or_default())
                    .await
            },
            |txns: &Vec<Transaction>| render_transactions(txns),
        )
        .await
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

    /// [`Self::guarded_tool`] with argument parsing folded in: deserialize `raw` into `A`
    /// (a malformed call returns the uniform `isError` **before** any write machinery runs),
    /// then dispatch `op` with the owned args. The frame every write tool shares.
    async fn guarded_args_tool<A, T, F>(
        &self,
        raw: JsonObject,
        class: ToolClass,
        op: F,
        render: impl FnOnce(&T) -> String,
    ) -> Result<CallToolResult, McpError>
    where
        A: serde::de::DeserializeOwned,
        F: AsyncFnOnce(WriteContext<'_>, A) -> Result<T, WriteError>,
    {
        let args: A = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        self.guarded_tool(class, async |ctx| op(ctx, args).await, render)
            .await
    }

    /// The read-side sibling of [`Self::guarded_args_tool`]: parse `raw` into `A`, run `op`
    /// through this connection's [`ConnectionView::grounded_read`] (pre-read epoch sample,
    /// bump on success), and render the result — adapter failures become tool-level
    /// `isError` results via [`adapter_error`].
    async fn read_args_tool<A, T, F>(
        &self,
        raw: JsonObject,
        op: F,
        render: impl FnOnce(&T) -> String,
    ) -> Result<CallToolResult, McpError>
    where
        A: serde::de::DeserializeOwned,
        F: AsyncFnOnce(&Hledger, A) -> Result<T, HledgerError>,
    {
        let args: A = match crate::tools::parse_args(raw) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        self.read_tool(async |hledger| op(hledger, args).await, render)
            .await
    }

    /// The zero-argument read frame ([`Self::read_args_tool`] without the parse step):
    /// run `op` through [`ConnectionView::grounded_read`] and render the result.
    async fn read_tool<T, F>(
        &self,
        op: F,
        render: impl FnOnce(&T) -> String,
    ) -> Result<CallToolResult, McpError>
    where
        F: AsyncFnOnce(&Hledger) -> Result<T, HledgerError>,
    {
        let result = self
            .view
            .grounded_read(&self.hledger, || op(&self.hledger))
            .await;
        match result {
            Ok(out) => Ok(CallToolResult::success(vec![Content::text(render(&out))])),
            Err(err) => Ok(adapter_error(&err)),
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: DeclareAccountArgs| write::declare_account(&ctx, &args.account).await,
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: CloseAccountArgs| write::tombstone_account(&ctx, &args.account).await,
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: DeclareCommodityArgs| {
                let places = args.decimal_places.unwrap_or(2);
                let out = write::declare_commodity(&ctx, &args.commodity, places).await?;
                Ok((places, out))
            },
            |(places, out): &(u8, CommitOutcome)| {
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, input: TransactionInput| write::post_transaction(&ctx, input).await,
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: VoidTransactionArgs| {
                let outcome = write::void_transaction(&ctx, &args.id).await?;
                Ok((args.id, outcome))
            },
            |(target, outcome): &(String, WriteOutcome)| {
                format!(
                    "voided '{}' with reversing entry id:{} (commit {})",
                    target,
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: UpdateTransactionArgs| {
                let outcome = write::update_transaction(&ctx, &args.id, args.transaction).await?;
                Ok((args.id, outcome))
            },
            |(target, outcome): &(String, WriteOutcome)| {
                format!(
                    "updated: voided '{}', posted replacement id:{} (commit {})",
                    target,
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: FundProjectArgs| {
                let input = crate::domain::fund_project_input(
                    args.date,
                    args.money.amount,
                    args.money.commodity,
                    args.money.idem,
                );
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: ReceiveInvoiceArgs| {
                let input = crate::domain::receive_invoice_input(
                    args.date,
                    &args.vendor,
                    args.expense_account,
                    args.money.amount,
                    args.money.commodity,
                    args.invoice_ref,
                    args.money.idem,
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: PayInvoiceArgs| {
                let vendor = &args.vendor;
                let ap_account = crate::domain::vendor_ap_account(vendor);
                let balance = ctx
                    .hledger
                    .balance_flat(Some(&ap_account))
                    .await
                    .map_err(|e| WriteError::Internal(format!("AP balance query: {e}")))?;
                if !crate::domain::has_outstanding_ap(&balance) {
                    return Err(WriteError::Input(format!(
                        "vendor '{vendor}' has no outstanding AP balance"
                    )));
                }
                let input = crate::domain::pay_invoice_input(
                    args.date,
                    vendor,
                    args.money.amount,
                    args.money.commodity,
                    args.money.idem,
                );
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: PostInterestArgs| {
                let input = crate::domain::post_interest_input(
                    args.date,
                    args.money.amount,
                    args.money.commodity,
                    args.money.idem,
                );
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
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: VendorAddArgs| {
                let expense_account = crate::domain::vendor_expense_account(
                    args.vendor_type,
                    &args.vendor,
                    args.trade.as_deref(),
                )
                .map_err(WriteError::Input)?;
                let ap_account = crate::domain::vendor_ap_account(&args.vendor);
                write::vendor_add(&ctx, &args.vendor, &ap_account, &expense_account).await
            },
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
        self.read_tool(
            async |hledger| hledger.declared_accounts().await,
            |accounts: &Vec<String>| {
                let vendors: Vec<&str> = accounts
                    .iter()
                    .filter(|a| a.starts_with(crate::domain::VENDOR_AP_PREFIX))
                    .map(String::as_str)
                    .collect();
                if vendors.is_empty() {
                    "(no vendors declared — use vendor_add first)".to_string()
                } else {
                    vendors.join("\n")
                }
            },
        )
        .await
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
        self.read_args_tool(
            raw,
            async |hledger, args: GetApAgingArgs| {
                let as_of = args.as_of.unwrap_or_else(crate::domain::today);
                let ap_query = [crate::domain::AP_ROOT.to_string()];
                let (balance, txns) = tokio::try_join!(
                    hledger.balance_flat(Some(crate::domain::AP_ROOT)),
                    hledger.list_transactions(&ap_query),
                )?;
                Ok((as_of, balance, txns))
            },
            |(as_of, balance, txns): &(NaiveDate, BalanceReport, Vec<Transaction>)| {
                let entries = crate::domain::compute_ap_aging(balance, txns, *as_of);
                let mut text = crate::domain::render_ap_aging(&entries, *as_of);
                let flags = crate::flags::ap_aging_flags(&entries);
                if !flags.is_empty() {
                    text.push('\n');
                    text.push_str(&crate::flags::render_flags(&flags));
                }
                text
            },
        )
        .await
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
        self.read_tool(
            async |hledger| tokio::try_join!(hledger.balancesheet(), hledger.incomestatement()),
            |(bs, is)| {
                format!(
                    "{}\n\n{}",
                    crate::domain::render_composite(bs),
                    crate::domain::render_composite(is),
                )
            },
        )
        .await
    }

    // ---- M5: budget (record/read) + change orders (the first decide tool) --------------

    /// Set (or replace) one account's per-period budget goal. **Record** — rewrites the
    /// dedicated budget file wholesale inside the epoch-commit pipeline (see
    /// [`write::budget`] for why replace, not append).
    #[tool(
        description = "Set or replace the budget goal for one account and period. Requires \
                      `account` (declared), `period` (daily|weekly|monthly|quarterly|yearly), \
                      `amount` (e.g. \"500.00\"), `commodity` (e.g. \"$\"). Setting the same \
                      account+period again replaces the goal. One call = one git commit.",
        input_schema = schema_for_type::<BudgetSetArgs>()
    )]
    async fn budget_set(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: BudgetSetArgs| {
                let quantity = crate::hledger::Quantity::parse(&args.amount).ok_or_else(|| {
                    WriteError::Input(format!("invalid amount '{}'", args.amount))
                })?;
                let out = write::budget::set_budget(
                    &ctx,
                    &args.account,
                    args.period,
                    quantity,
                    args.commodity.clone(),
                )
                .await?;
                Ok((args, out))
            },
            |(args, out): &(BudgetSetArgs, CommitOutcome)| {
                format!(
                    "budget set: {} {} = {} {} (commit {})",
                    out.id,
                    args.period,
                    args.amount,
                    args.commodity,
                    out.commit.short()
                )
            },
        )
        .await
    }

    /// List the current budget rules. **Read**.
    #[tool(
        description = "List the current budget rules (account, period, goal amount). \
                      Takes no arguments."
    )]
    async fn budget_list(
        &self,
        Parameters(_raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        self.read_tool(
            async |hledger| {
                let journal = hledger.journal_path().ok_or(HledgerError::NoJournal)?;
                write::budget::read_budget_rules(journal)
                    .map_err(|e| HledgerError::BadVersion(e.to_string()))
            },
            |rules: &Vec<write::budget::BudgetRule>| {
                if rules.is_empty() {
                    "(no budget rules set — use budget_set first)".to_string()
                } else {
                    rules
                        .iter()
                        .map(|r| {
                            format!(
                                "{}  {} = {} {}",
                                r.account,
                                r.period,
                                r.quantity.render(),
                                r.commodity
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            },
        )
        .await
    }

    /// Budget vs actual via `hledger balance --budget`. **Read** — over-budget surfaces as a
    /// soft-invariant flag, never a rejection (C-6).
    #[tool(
        description = "Budget vs actual per account, from the journal's budget rules. \
                      Optional `account` query to scope the report. Accounts whose actual \
                      exceeds the goal are flagged over-budget (informational, never \
                      enforced).",
        input_schema = schema_for_type::<BudgetVsActualArgs>()
    )]
    async fn get_budget_vs_actual(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        self.read_args_tool(
            raw,
            async |hledger, args: BudgetVsActualArgs| {
                hledger.budget_report(args.account.as_deref()).await
            },
            budget_text,
        )
        .await
    }

    /// Record a **pending** change order: the CO amount posts to the pending CO subtree
    /// against the vendor's AP. **Record**.
    #[tool(
        description = "Record a change order as PENDING: posts the amount to \
                      `expenses:change orders:pending:{trade}` against the vendor's AP \
                      account. Requires `eco` (your CO id), `trade`, `vendor` (declared), \
                      `description`, `date` (YYYY-MM-DD), `amount`, `commodity`. Optional \
                      `idem`. Approve later with eco_approve. See ledger://eco-guide.",
        input_schema = schema_for_type::<EcoCreateArgs>()
    )]
    async fn eco_create(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: EcoCreateArgs| {
                if args.trade == "pending" || args.trade.starts_with("pending:") {
                    return Err(WriteError::Input(
                        "'pending' is a reserved trade name under change orders".to_string(),
                    ));
                }
                if !write::find_by_exact_tag(ctx.hledger, "eco", &args.eco)
                    .await
                    .map_err(|e| WriteError::Internal(format!("eco lookup: {e}")))?
                    .is_empty()
                {
                    return Err(WriteError::Input(format!(
                        "change order '{}' already exists",
                        args.eco
                    )));
                }
                // Auto-declare the CO accounts (the vendor_add precedent) — one commit, only
                // when missing.
                write::ensure_declared_accounts(
                    &ctx,
                    &[
                        &crate::domain::eco_pending_account(&args.trade),
                        &crate::domain::eco_account(&args.trade),
                    ],
                )
                .await?;
                let input = crate::domain::eco_create_input(
                    args.date,
                    &args.eco,
                    &args.trade,
                    &args.vendor,
                    &args.description,
                    args.money.amount,
                    args.money.commodity,
                    args.money.idem,
                );
                let outcome = write::post_transaction(&ctx, input).await?;
                Ok((args.eco, outcome))
            },
            |(eco, outcome): &(String, WriteOutcome)| {
                format!(
                    "ECO {eco} recorded as pending, id:{} (commit {})",
                    outcome.base.id,
                    outcome.base.commit.short()
                )
            },
        )
        .await
    }

    /// Approve a pending change order — the first **decide** tool: it acts on a belief about
    /// the current budget state, so it is **epoch-checked** inside the write locks (M3 CAS).
    /// A `STALE` rejection means the ledger moved since this connection's last read: re-read,
    /// re-evaluate the budget, retry.
    #[tool(
        description = "Approve a pending change order: transfers its amount from the pending \
                      subtree into the budget-tracked `expenses:change orders:{trade}`. \
                      EPOCH-CHECKED (decide): fails with STALE if the ledger changed since \
                      your last read — re-read, re-evaluate, retry. Requires `eco` and `date`. \
                      The response includes the account's budget-vs-actual standing.",
        input_schema = schema_for_type::<EcoRefArgs>()
    )]
    async fn eco_approve(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        self.guarded_args_tool(
            raw,
            ToolClass::Decide,
            async |ctx, args: EcoRefArgs| {
                let events = eco_events(ctx.hledger, &args.eco).await?;
                let created = events.created()?;
                if events.approved {
                    return Err(WriteError::Input(format!(
                        "change order '{}' is already approved",
                        args.eco
                    )));
                }
                let (trade, amount) = crate::domain::eco_details(created).ok_or_else(|| {
                    WriteError::Internal(format!(
                        "ECO '{}' create transaction has no pending-CO posting",
                        args.eco
                    ))
                })?;
                let input = crate::domain::eco_approve_input(
                    args.date,
                    &args.eco,
                    &trade,
                    amount.quantity.render(),
                    amount.commodity.clone(),
                );
                let outcome = write::post_transaction(&ctx, input).await?;
                // Ground the decision in the response: this trade's budget standing now.
                let account = crate::domain::eco_account(&trade);
                let budget = ctx.hledger.budget_report(Some(&account)).await.ok();
                Ok((args.eco, outcome, budget))
            },
            |(eco, outcome, budget): &(
                String,
                WriteOutcome,
                Option<crate::hledger::BudgetReport>,
            )| {
                let mut text = format!(
                    "ECO {eco} approved, id:{} (commit {})",
                    outcome.base.id,
                    outcome.base.commit.short()
                );
                if let Some(report) = budget {
                    text.push('\n');
                    text.push_str(&budget_text(report));
                }
                text
            },
        )
        .await
    }

    /// Void a change order: reverse its unreversed transactions (append-only). **Record**.
    #[tool(
        description = "Void a change order: posts reversing entries for its create (and \
                      approval, if approved) transactions. Append-only — nothing is edited \
                      or removed. Requires `eco` and `date`.",
        input_schema = schema_for_type::<EcoRefArgs>()
    )]
    async fn eco_void(
        &self,
        Parameters(raw): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        self.guarded_args_tool(
            raw,
            ToolClass::Record,
            async |ctx, args: EcoRefArgs| {
                let events = eco_events(ctx.hledger, &args.eco).await?;
                events.created()?; // unknown ECO → correctable input error
                let targets = events.unreversed;
                if targets.is_empty() {
                    return Err(WriteError::Input(format!(
                        "change order '{}' is already fully voided",
                        args.eco
                    )));
                }
                let mut last = None;
                let count = targets.len();
                for id in targets {
                    last = Some(write::void_transaction(&ctx, &id).await?);
                }
                let last = last.expect("targets is non-empty");
                Ok((args.eco, count, last))
            },
            |(eco, count, outcome): &(String, usize, WriteOutcome)| {
                format!(
                    "ECO {eco} voided: {count} reversing entr{} posted (last commit {})",
                    if *count == 1 { "y" } else { "ies" },
                    outcome.base.commit.short()
                )
            },
        )
        .await
    }

    /// The dynamic `ledger://vendors` body: declared vendor AP accounts, each with its
    /// outstanding balance. A grounded read (bumps last-seen), like any other ledger read —
    /// the one resource that touches hledger, and only when actually fetched.
    async fn vendors_resource_text(&self) -> Result<String, HledgerError> {
        self.view
            .grounded_read(&self.hledger, || async {
                let (accounts, balance) = tokio::try_join!(
                    self.hledger.declared_accounts(),
                    self.hledger.balance_flat(Some(crate::domain::AP_ROOT)),
                )?;
                Ok(vendors_text(&accounts, &balance))
            })
            .await
    }

    /// The `resources/read` payload for `uri`: static guides come from the compiled-in
    /// markdown (no hledger); `ledger://vendors` is the dynamic exception; anything else is
    /// the MCP resource-not-found error.
    async fn resource_contents(
        &self,
        uri: &str,
    ) -> Result<rmcp::model::ReadResourceResult, McpError> {
        if let Some(resource) = crate::resources::find_static(uri) {
            return Ok(rmcp::model::ReadResourceResult::new(vec![
                rmcp::model::ResourceContents::text(resource.content, uri),
            ]));
        }
        if uri == crate::resources::VENDORS_URI {
            let text = self.vendors_resource_text().await.map_err(|err| {
                McpError::internal_error(format!("vendors resource: {err}"), None)
            })?;
            return Ok(rmcp::model::ReadResourceResult::new(vec![
                rmcp::model::ResourceContents::text(text, uri),
            ]));
        }
        Err(McpError::resource_not_found(
            format!("unknown resource '{uri}' — see ledger://session-context for the index"),
            None,
        ))
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

/// The hledger-version line of `status`: pinned / MISMATCH / unavailable verdict plus the
/// raw `--version` banner. Pure — unit-testable against a mismatched [`Version`].
fn version_line(detected: &Result<crate::hledger::Version, HledgerError>) -> String {
    match detected {
        Ok(v) if v.pin_matches() => {
            format!("hledger: {}.{} (pinned) — {:?}", v.major, v.minor, v.raw)
        }
        Ok(v) => format!(
            "hledger: {}.{} (MISMATCH — expected {}.{}) — {:?}",
            v.major, v.minor, PINNED_VERSION.0, PINNED_VERSION.1, v.raw
        ),
        Err(err) => format!("hledger: unavailable ({err})"),
    }
}

/// The epoch line of `status`: current `HEAD` vs this connection's last-seen
/// (fresh / STALE / no read yet). Pure.
fn epoch_line(head: &Epoch, seen: Option<&Epoch>) -> String {
    let connection = match seen {
        Some(seen) if seen == head => format!("last-seen {} (fresh)", seen.short()),
        Some(seen) => format!("last-seen {} (STALE — re-read)", seen.short()),
        None => "no read yet this connection".to_string(),
    };
    format!("epoch: {} — {connection}", head.short())
}

/// Body of `get_account_balance`: the rendered report plus the overdraft-flag footer
/// (present only when an asset balance is negative — C-6: surfaced, never enforced).
fn balance_text(report: &BalanceReport) -> String {
    let mut text = render_balance(report);
    let flags = crate::flags::overdraft_flags(report);
    if !flags.is_empty() {
        text.push('\n');
        text.push_str(&crate::flags::render_flags(&flags));
    }
    text
}

/// Body of the dynamic `ledger://vendors` resource: each **declared vendor** AP account with
/// its outstanding balance — `0` when the AP balance report has no row for it (a paid-up
/// vendor drops out of the report; it must not drop out of the list). Pure.
fn vendors_text(accounts: &[String], balance: &BalanceReport) -> String {
    let mut lines: Vec<String> = accounts
        .iter()
        .filter(|a| a.starts_with(crate::domain::VENDOR_AP_PREFIX))
        .map(|account| {
            let outstanding = balance
                .rows
                .iter()
                .find(|r| &r.account == account)
                .map(|r| render_amounts(&r.amounts))
                .unwrap_or_else(|| "0".to_string());
            format!("{account}  outstanding {outstanding}")
        })
        .collect();
    if lines.is_empty() {
        lines.push("(no vendors declared — use vendor_add first)".to_string());
    }
    lines.join("\n")
}

/// Body of `get_budget_vs_actual` (and the `eco_approve` footer): the rendered budget report
/// plus the over-budget flag footer (C-6: surfaced, never enforced).
fn budget_text(report: &crate::hledger::BudgetReport) -> String {
    let mut text = crate::domain::render_budget(report);
    let flags = crate::flags::over_budget_flags(report);
    if !flags.is_empty() {
        text.push('\n');
        text.push_str(&crate::flags::render_flags(&flags));
    }
    text
}

/// One change order's recorded lifecycle, as read back from its `eco:`-tagged transactions.
struct EcoEvents {
    /// The id passed by the caller (for error text).
    eco: String,
    /// The `eco_event:created` transaction, if the CO exists.
    created: Option<Transaction>,
    /// Whether an `eco_event:approved` transaction exists.
    approved: bool,
    /// `id:` tag values of the CO's transactions not yet reversed (void targets).
    unreversed: Vec<String>,
}

impl EcoEvents {
    /// The create transaction, or the correctable unknown-ECO input error.
    fn created(&self) -> Result<&Transaction, WriteError> {
        self.created.as_ref().ok_or_else(|| {
            WriteError::Input(format!(
                "unknown change order '{}' — record it first with eco_create",
                self.eco
            ))
        })
    }
}

/// Read one CO's transactions (exact `eco:` tag match) and classify them. Reversals don't
/// carry the `eco:` tag, so reversed-ness is resolved per transaction via its `reverses:<id>`
/// back-reference (the same belt-and-suspenders exact-tag query as the dedup path).
async fn eco_events(hledger: &Hledger, eco: &str) -> Result<EcoEvents, WriteError> {
    let internal = |e: HledgerError| WriteError::Internal(format!("eco lookup: {e}"));
    let txns = write::find_by_exact_tag(hledger, "eco", eco)
        .await
        .map_err(internal)?;
    let event =
        |txn: &Transaction, ev: &str| txn.tags.iter().any(|(k, v)| k == "eco_event" && v == ev);
    let created = txns
        .iter()
        .find(|t| event(t, crate::domain::ECO_EVENT_CREATED))
        .cloned();
    let approved = txns
        .iter()
        .any(|t| event(t, crate::domain::ECO_EVENT_APPROVED));
    let mut unreversed = Vec::new();
    for txn in &txns {
        let Some(id) = txn.tags.iter().find(|(k, _)| k == "id").map(|(_, v)| v) else {
            continue;
        };
        if write::find_by_exact_tag(hledger, "reverses", id)
            .await
            .map_err(internal)?
            .is_empty()
        {
            unreversed.push(id.clone());
        }
    }
    Ok(EcoEvents {
        eco: eco.to_string(),
        created,
        approved,
        unreversed,
    })
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

/// The `resources/list` payload (pure): the static `ledger://` guides plus the dynamic
/// `vendors` entry. Listing never touches hledger.
fn resource_listing() -> Vec<rmcp::model::Resource> {
    use rmcp::model::AnnotateAble as _;
    let mut resources: Vec<rmcp::model::Resource> = crate::resources::STATIC
        .iter()
        .map(|r| {
            rmcp::model::RawResource::new(r.uri, r.name)
                .with_title(r.title)
                .with_description(r.description)
                .no_annotation()
        })
        .collect();
    resources.push(
        rmcp::model::RawResource::new(crate::resources::VENDORS_URI, "vendors")
            .with_title("Vendors (live)")
            .with_description(
                "Declared vendor AP accounts with outstanding balances — dynamic: \
                     reads the ledger when fetched.",
            )
            .no_annotation(),
    );
    resources
}

/// Filter + reshape the advertised tool list for a profile (MC-8/MC-10, pure): drop tools
/// the profile doesn't advertise, and replace each Tier-2 tool's description with its
/// one-line summary (the detail lives in a `ledger://` resource). Dispatch is untouched —
/// this transforms only what `tools/list` returns.
fn advertised_tools(
    profile: crate::catalog::Profile,
    tools: Vec<rmcp::model::Tool>,
) -> Vec<rmcp::model::Tool> {
    tools
        .into_iter()
        .filter(|t| crate::catalog::advertised(profile, &t.name))
        .map(|mut t| {
            if let Some(meta) = crate::catalog::meta(&t.name)
                && meta.tier == crate::catalog::Tier::Administrative
            {
                t.description = Some(std::borrow::Cow::Borrowed(meta.summary));
            }
            t
        })
        .collect()
}

#[tool_handler]
impl ServerHandler for HledgerMcp {
    fn get_info(&self) -> ServerInfo {
        // `tools` + `resources` (M5; M0 declared tools only). `InitializeResult` is
        // `#[non_exhaustive]`, so build it via its constructor + builder methods.
        let capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        ServerInfo::new(capabilities)
            .with_server_info(rmcp::model::Implementation::new(
                SERVER_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    /// `tools/list` under the active profile: the full router list, profile-filtered, with
    /// Tier-2 descriptions reduced to one line. (`call_tool`/`get_tool` come from
    /// `#[tool_handler]` over the **full** router — a non-advertised tool still dispatches.)
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        Ok(rmcp::model::ListToolsResult::with_all_items(
            advertised_tools(self.profile, Self::tool_router().list_all()),
        ))
    }

    /// `resources/list`: the static `ledger://` guides plus the dynamic `vendors` resource.
    /// **Never touches hledger** — discovery stays off the cold-start path (asserted by the
    /// bogus-binary e2e).
    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, McpError> {
        Ok(rmcp::model::ListResourcesResult::with_all_items(
            resource_listing(),
        ))
    }

    /// `resources/read`: static guides are served from the compiled-in markdown (no hledger);
    /// the one dynamic resource, `ledger://vendors`, is a grounded read of the live ledger.
    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResult, McpError> {
        self.resource_contents(request.uri.as_str()).await
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

    #[test]
    fn version_line_distinguishes_pinned_mismatch_and_unavailable() {
        use crate::hledger::Version;
        let v = |major, minor| Version {
            raw: format!("hledger {major}.{minor}"),
            major,
            minor,
        };
        let pinned = version_line(&Ok(v(PINNED_VERSION.0, PINNED_VERSION.1)));
        assert!(pinned.contains("(pinned)"), "{pinned}");
        assert!(!pinned.contains("MISMATCH"), "{pinned}");

        let mismatch = version_line(&Ok(v(1, 99)));
        assert!(mismatch.contains("MISMATCH"), "{mismatch}");
        assert!(!mismatch.contains("(pinned)"), "{mismatch}");

        let unavailable = version_line(&Err(crate::hledger::HledgerError::BadVersion(
            "garbage".to_string(),
        )));
        assert!(unavailable.contains("unavailable"), "{unavailable}");
    }

    #[test]
    fn epoch_line_distinguishes_fresh_stale_and_unread() {
        use crate::epoch::CommitOid;
        let epoch = |c: char| Epoch::new(Some(CommitOid::new(c.to_string().repeat(40))));
        let head = epoch('a');

        let fresh = epoch_line(&head, Some(&epoch('a')));
        assert!(fresh.contains("(fresh)"), "{fresh}");
        assert!(!fresh.contains("STALE"), "{fresh}");

        let stale = epoch_line(&head, Some(&epoch('b')));
        assert!(stale.contains("STALE"), "{stale}");
        assert!(!stale.contains("(fresh)"), "{stale}");

        let unread = epoch_line(&head, None);
        assert!(unread.contains("no read yet"), "{unread}");
    }

    #[test]
    fn vendors_text_pairs_each_declared_vendor_with_its_own_balance_row() {
        let accounts = vec![
            "assets:checking".to_string(), // not a vendor — filtered out
            "liabilities:ap:vendor:Acme".to_string(),
            "liabilities:ap:vendor:PaidUp".to_string(), // no balance row → 0
        ];
        let balance = BalanceReport {
            rows: vec![AccountBalance {
                account: "liabilities:ap:vendor:Acme".to_string(),
                amounts: vec![Amount {
                    commodity: "$".into(),
                    quantity: Quantity::new(-250000, 2),
                    commodity_left: true,
                    spaced: false,
                }],
            }],
            totals: vec![],
        };
        let text = vendors_text(&accounts, &balance);
        assert_eq!(
            text,
            "liabilities:ap:vendor:Acme  outstanding $-2500.00\n\
             liabilities:ap:vendor:PaidUp  outstanding 0",
            "each vendor must get ITS row's amount (and 0 only when rowless)"
        );
        assert!(!text.contains("assets:checking"), "{text}");

        let none = vendors_text(&["assets:checking".to_string()], &balance);
        assert!(none.contains("no vendors declared"), "{none}");
    }

    #[test]
    fn balance_text_appends_flag_footer_only_on_overdraft() {
        let report = |mantissa: i128| BalanceReport {
            rows: vec![AccountBalance {
                account: "assets:checking".to_string(),
                amounts: vec![Amount {
                    commodity: "$".into(),
                    quantity: Quantity::new(mantissa, 2),
                    commodity_left: true,
                    spaced: false,
                }],
            }],
            totals: vec![],
        };
        let clean = balance_text(&report(100));
        assert!(!clean.contains("flag"), "{clean}");
        assert!(!clean.ends_with('\n'), "no dangling newline: {clean:?}");

        let overdrawn = balance_text(&report(-100));
        assert!(overdrawn.contains("flag overdraft:"), "{overdrawn}");
    }

    /// The `#[serde(flatten)]` of [`MoneyArgs`] must keep the advertised schema **flat** —
    /// the model sees `amount`/`commodity`/`idem` as top-level properties beside the
    /// per-tool `date`, never nested under a `money` key.
    #[test]
    fn money_args_flatten_keeps_schema_flat() {
        let schema =
            serde_json::to_value(schema_for_type::<PayInvoiceArgs>()).expect("schema to json");
        let props = schema["properties"]
            .as_object()
            .expect("schema has properties");
        for field in ["date", "vendor", "amount", "commodity", "idem"] {
            assert!(
                props.contains_key(field),
                "{field} missing from flattened schema: {props:?}"
            );
        }
        assert!(
            !props.contains_key("money"),
            "flatten must not nest a 'money' object: {props:?}"
        );
    }

    /// Flatten switches serde to buffered deserialization; the parse-error contract
    /// (accurate, field-naming messages — the model's self-correction loop) must survive it.
    #[test]
    fn money_args_flatten_keeps_parse_errors_field_accurate() {
        let missing = crate::tools::parse_args::<PayInvoiceArgs>(
            serde_json::json!({ "date": "2026-01-01", "vendor": "Acme", "commodity": "$" })
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap_err();
        let text = &missing.content[0].as_text().expect("text").text;
        assert!(text.contains("amount"), "names the missing field: {text}");
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
    fn get_info_declares_tools_and_resources_and_points_at_session_context() {
        let info = test_server().get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability declared"
        );
        assert!(
            info.capabilities.resources.is_some(),
            "resources capability declared (M5)"
        );
        let instructions = info.instructions.expect("server_instructions present");
        assert!(
            instructions.contains(crate::resources::SESSION_CONTEXT_URI),
            "server_instructions must direct clients to session-context: {instructions}"
        );
    }

    /// The catalog must classify **exactly** the router's tools — adding a tool without
    /// classifying it (or classifying a phantom) fails here, keeping MC-8/MC-10 exhaustive.
    #[test]
    fn catalog_matches_the_tool_router_exactly() {
        let mut router: Vec<String> = HledgerMcp::tool_router()
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let mut catalog: Vec<String> = crate::catalog::TOOLS
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        router.sort();
        catalog.sort();
        assert_eq!(catalog, router);
    }

    /// MC-8: under `full`, Tier-2 tools advertise their one-line summary while Tier-1 tools
    /// keep their full descriptions; under a filtered profile the list shrinks but the full
    /// router still resolves everything for dispatch (MC-10 at the unit level).
    #[test]
    fn advertised_tools_rewrites_tier_two_and_filters_by_profile() {
        use crate::catalog::Profile;
        let all = advertised_tools(Profile::Full, HledgerMcp::tool_router().list_all());
        let vendor_add = all
            .iter()
            .find(|t| t.name == "vendor_add")
            .expect("vendor_add");
        assert_eq!(
            vendor_add.description.as_deref(),
            crate::catalog::meta("vendor_add").map(|m| m.summary),
            "tier-2 description is the one-line summary"
        );
        let post = all
            .iter()
            .find(|t| t.name == "post_transaction")
            .expect("post");
        let full_desc = post.description.as_deref().unwrap_or_default();
        assert!(
            full_desc.contains("postings"),
            "tier-1 keeps its full description: {full_desc}"
        );

        let operational =
            advertised_tools(Profile::Operational, HledgerMcp::tool_router().list_all());
        assert!(operational.iter().all(|t| {
            crate::catalog::meta(&t.name)
                .is_some_and(|m| m.tier == crate::catalog::Tier::Operational)
        }));
        assert!(operational.len() < all.len(), "filtering actually filters");
        assert!(
            !operational.iter().any(|t| t.name == "vendor_add"),
            "vendor_add not advertised under operational"
        );
        assert!(
            HledgerMcp::tool_router().get("vendor_add").is_some(),
            "…but still dispatchable from the full router"
        );
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

    /// One in-process pass over the M4+M5 domain handlers against real hledger. This is what
    /// gives the handlers line *coverage* — the wire e2e spawns a separate server process
    /// that llvm-cov cannot see, so every handler is also driven directly here.
    #[tokio::test]
    async fn domain_and_m5_handlers_in_process_lifecycle() {
        use serde_json::json;
        let Some(bin) = hledger_bin() else { return };
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = dir.path().join("main.journal");
        let server = HledgerMcp::new(Hledger::new(bin, Some(journal)));

        fn ok(result: CallToolResult) -> String {
            assert_eq!(result.is_error, Some(false), "{result:?}");
            result.content[0].as_text().expect("text").text.clone()
        }
        fn err_text(result: CallToolResult) -> String {
            assert_eq!(result.is_error, Some(true), "{result:?}");
            result.content[0].as_text().expect("text").text.clone()
        }

        // Bootstrap declarations + vendor.
        ok(server
            .declare_commodity(args(json!({"commodity": "$"})))
            .await
            .unwrap());
        for account in [
            "assets:checking",
            "equity:owner capital",
            "income:interest",
            "expenses:construction:plumbing",
        ] {
            ok(server
                .declare_account(args(json!({"account": account})))
                .await
                .unwrap());
        }
        ok(server
            .vendor_add(args(
                json!({"vendor": "GC", "vendor_type": "trade", "trade": "general"}),
            ))
            .await
            .unwrap());

        // Money writes.
        ok(server
            .fund_project(args(
                json!({"date": "2020-01-01", "amount": "50000.00", "commodity": "$"}),
            ))
            .await
            .unwrap());
        ok(server
            .receive_invoice(args(json!({
                "date": "2020-01-10", "vendor": "GC",
                "expense_account": "expenses:construction:plumbing",
                "amount": "800.00", "commodity": "$", "invoice_ref": "INV-1"
            })))
            .await
            .unwrap());
        ok(server
            .post_interest(args(
                json!({"date": "2020-01-15", "amount": "10.00", "commodity": "$"}),
            ))
            .await
            .unwrap());

        // Reads.
        assert!(
            ok(server
                .get_account_balance(args(json!({"account": "assets:checking"})))
                .await
                .unwrap())
            .contains("assets:checking")
        );
        assert!(
            ok(server.list_transactions(args(json!({}))).await.unwrap()).contains("GC invoice")
        );
        assert!(ok(server.get_ap_aging(args(json!({}))).await.unwrap()).contains("GC"));
        assert!(
            ok(server.get_project_summary(args(json!({}))).await.unwrap())
                .contains("Balance Sheet")
        );
        assert!(
            ok(server.vendor_list(args(json!({}))).await.unwrap())
                .contains("liabilities:ap:vendor:GC")
        );
        ok(server
            .pay_invoice(args(
                json!({"date": "2020-02-01", "vendor": "GC", "amount": "800.00", "commodity": "$"}),
            ))
            .await
            .unwrap());

        // Budget: set → list → over-budget flag (actual 800 > goal 500).
        ok(server
            .budget_set(args(json!({
                "account": "expenses:construction:plumbing",
                "period": "monthly", "amount": "300.00", "commodity": "$"
            })))
            .await
            .unwrap());
        assert!(
            ok(server.budget_list(args(json!({}))).await.unwrap()).contains("monthly = 300.00 $")
        );
        let report = ok(server.get_budget_vs_actual(args(json!({}))).await.unwrap());
        assert!(report.contains("flag over-budget"), "{report}");

        // ECO lifecycle incl. every correctable-error branch.
        ok(server
            .eco_create(args(json!({
                "eco": "7", "trade": "electrical", "vendor": "GC", "description": "outlets",
                "date": "2020-02-10", "amount": "1500.00", "commodity": "$"
            })))
            .await
            .unwrap());
        let dup = err_text(
            server
                .eco_create(args(json!({
                    "eco": "7", "trade": "electrical", "vendor": "GC", "description": "again",
                    "date": "2020-02-10", "amount": "1.00", "commodity": "$"
                })))
                .await
                .unwrap(),
        );
        assert!(dup.contains("already exists"), "{dup}");
        let reserved = err_text(
            server
                .eco_create(args(json!({
                    "eco": "8", "trade": "pending", "vendor": "GC", "description": "x",
                    "date": "2020-02-10", "amount": "1.00", "commodity": "$"
                })))
                .await
                .unwrap(),
        );
        assert!(reserved.contains("reserved"), "{reserved}");
        let unknown = err_text(
            server
                .eco_approve(args(json!({"eco": "404", "date": "2020-02-11"})))
                .await
                .unwrap(),
        );
        assert!(unknown.contains("unknown change order"), "{unknown}");
        let approved = ok(server
            .eco_approve(args(json!({"eco": "7", "date": "2020-02-11"})))
            .await
            .unwrap());
        assert!(approved.contains("approved"), "{approved}");
        let again = err_text(
            server
                .eco_approve(args(json!({"eco": "7", "date": "2020-02-12"})))
                .await
                .unwrap(),
        );
        assert!(again.contains("already approved"), "{again}");
        let voided = ok(server
            .eco_void(args(json!({"eco": "7", "date": "2020-03-01"})))
            .await
            .unwrap());
        assert!(voided.contains("2 reversing"), "{voided}");
        let fully = err_text(
            server
                .eco_void(args(json!({"eco": "7", "date": "2020-03-02"})))
                .await
                .unwrap(),
        );
        assert!(fully.contains("already fully voided"), "{fully}");

        // Corrections: post → update (void + repost) → close (tombstone).
        let posted = ok(server
            .post_transaction(args(json!({
                "date": "2020-03-05", "description": "misc",
                "postings": [
                    {"account": "expenses:construction:plumbing",
                     "amount": {"quantity": "5.00", "commodity": "$"}},
                    {"account": "assets:checking"}
                ]
            })))
            .await
            .unwrap());
        let id = posted
            .split("id:")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .expect("posted id")
            .to_string();
        ok(server
            .update_transaction(args(json!({
                "id": id,
                "transaction": {
                    "date": "2020-03-05", "description": "misc fixed",
                    "postings": [
                        {"account": "expenses:construction:plumbing",
                         "amount": {"quantity": "6.00", "commodity": "$"}},
                        {"account": "assets:checking"}
                    ]
                }
            })))
            .await
            .unwrap());
        ok(server
            .close_account(args(json!({"account": "income:interest"})))
            .await
            .unwrap());

        // Resources: the ctx-free payloads (the rmcp wrappers are wire-tested in e2e).
        assert_eq!(resource_listing().len(), 7);
        let read_text = |result: rmcp::model::ReadResourceResult| -> String {
            match result.contents.into_iter().next().expect("contents") {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => text,
                other => panic!("expected text contents: {other:?}"),
            }
        };
        let session = server
            .resource_contents(crate::resources::SESSION_CONTEXT_URI)
            .await
            .expect("static read");
        assert!(read_text(session).contains("Tool groups"));
        let vendors = server
            .resource_contents(crate::resources::VENDORS_URI)
            .await
            .expect("vendors read");
        assert!(read_text(vendors).contains("liabilities:ap:vendor:GC"));
        assert!(server.resource_contents("ledger://nope").await.is_err());
    }
}
