//! Input types for the write tools — these double as the MCP argument schemas
//! (`#[derive(JsonSchema)]`). They are *unvalidated*; [`super::validate`] turns them into the
//! checked form the formatter consumes.

use rmcp::schemars::JsonSchema;
use serde::Deserialize;

/// An amount on a posting: an exact decimal **string** (never a float) plus its commodity.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PostingAmount {
    /// Exact decimal quantity as a string, e.g. `"100.00"` or `"-44.00"`. Parsed losslessly;
    /// not a JSON number (floats can't represent money exactly).
    pub quantity: String,
    /// Commodity symbol, e.g. `"$"` or `"EUR"`. Must already be declared.
    pub commodity: String,
}

/// One posting line of a transaction.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PostingInput {
    /// Account to post to. Must already be declared (`declare_account` first).
    pub account: String,
    /// The amount; omit on **exactly one** posting to let it balance the others.
    #[serde(default)]
    pub amount: Option<PostingAmount>,
}

/// A transaction to post.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TransactionInput {
    /// Transaction date, `YYYY-MM-DD`.
    pub date: String,
    /// Description / payee (no newline or `;`).
    pub description: String,
    /// Postings: at least 2; at most one may omit `amount` (the balancing posting).
    pub postings: Vec<PostingInput>,
    /// Optional extra `key:value` tags (the reserved `id`/`idem`/`reverses` are not allowed).
    #[serde(default)]
    pub tags: Vec<(String, String)>,
    /// Optional idempotency key — pass the **same** value on a retry to avoid a duplicate post.
    /// Generated if omitted (so a retry without one is *not* deduplicated).
    #[serde(default)]
    pub idem: Option<String>,
}
