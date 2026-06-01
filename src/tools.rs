//! Shared plumbing for MCP tool handlers.
//!
//! The convention this establishes (M0, used by every tool from M1 on): a tool advertises
//! its argument schema with `schema_for_type::<T>()` and extracts arguments with
//! [`parse_args`], keeping the `#[derive(Deserialize, JsonSchema)]` struct as the **single
//! source of truth** for both shape and validation.
//!
//! Why not take `Parameters<T>` directly? `rmcp` maps a `Parameters<T>` deserialization
//! failure to a JSON-RPC `invalid_params` *protocol* error before the handler runs, which a
//! model cannot self-correct from. Taking `Parameters<JsonObject>` and deserializing here
//! instead lets us return a tool-level `isError` result (the SHOULD from
//! `docs/development/mcp-protocol-versions.md`). Centralizing it means tools don't each
//! hand-roll lenient parsing + bespoke error strings (which drift from the advertised
//! schema); `serde` produces accurate, uniform messages — distinguishing a missing field
//! ("missing field `x`") from a wrong type ("invalid type: …, expected …") for free.

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::de::DeserializeOwned;

/// Deserialize raw tool arguments into `T`, or return a uniform `isError` result.
///
/// On success, `Ok(T)`. On failure, `Err(CallToolResult)` already flagged `isError` with a
/// serde-derived message — the handler just `return`s it.
pub fn parse_args<T: DeserializeOwned>(raw: JsonObject) -> Result<T, CallToolResult> {
    serde_json::from_value(serde_json::Value::Object(raw)).map_err(|err| {
        CallToolResult::error(vec![Content::text(format!("invalid arguments: {err}"))])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct Demo {
        message: String,
    }

    fn obj(json: serde_json::Value) -> JsonObject {
        json.as_object().expect("object").clone()
    }

    #[test]
    fn ok_on_valid_args() {
        let parsed: Demo = parse_args(obj(serde_json::json!({ "message": "hi" }))).expect("ok");
        assert_eq!(parsed.message, "hi");
    }

    #[test]
    fn missing_field_is_iserror_and_says_missing() {
        let err = parse_args::<Demo>(obj(serde_json::json!({ "nope": 1 }))).unwrap_err();
        assert_eq!(err.is_error, Some(true));
        let text = &err.content[0].as_text().expect("text").text;
        assert!(text.contains("missing"), "missing-field message: {text}");
    }

    #[test]
    fn wrong_type_is_iserror_and_distinct_from_missing() {
        let err = parse_args::<Demo>(obj(serde_json::json!({ "message": 42 }))).unwrap_err();
        assert_eq!(err.is_error, Some(true));
        let text = &err.content[0].as_text().expect("text").text;
        assert!(
            text.contains("invalid type"),
            "type message names the type: {text}"
        );
        assert!(
            !text.contains("missing"),
            "type error must not say missing: {text}"
        );
    }
}
