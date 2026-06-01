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
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use serde_json::Value;

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

/// The MCP server handler.
#[derive(Clone)]
pub struct HledgerMcp {
    started: Instant,
}

impl Default for HledgerMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl HledgerMcp {
    /// Construct a fresh server with its uptime clock started.
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    /// Report server health: name, version, active protocol version, and uptime.
    #[tool(
        description = "Report server status: name, version, protocol version, and uptime \
                         in seconds. Takes no arguments."
    )]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        let uptime = self.started.elapsed().as_secs();
        let body = format!(
            "{SERVER_NAME} {} — protocol {}, uptime {uptime}s",
            env!("CARGO_PKG_VERSION"),
            ProtocolVersion::LATEST,
        );
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    /// Echo a message back unchanged — a minimal tool-invocation connectivity check.
    ///
    /// Arguments are read leniently: a missing/non-string `message` yields an
    /// `isError` result (self-correctable) rather than a JSON-RPC `invalid_params`.
    #[tool(
        description = "Echo a message back unchanged. A minimal connectivity / \
                      tool-invocation check. Requires a string field `message`.",
        input_schema = schema_for_type::<EchoArgs>()
    )]
    async fn echo(
        &self,
        Parameters(args): Parameters<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        match args.get("message").and_then(Value::as_str) {
            Some(message) => Ok(CallToolResult::success(vec![Content::text(
                message.to_owned(),
            )])),
            None => Ok(CallToolResult::error(vec![Content::text(
                "echo: missing required string field `message`",
            )])),
        }
    }
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
        let negotiated = crate::protocol::negotiate(&requested);
        let roots = request.capabilities.roots.is_some();

        tracing::info!(
            client.name = %request.client_info.name,
            client.version = %request.client_info.version,
            protocol.requested = %requested,
            protocol.negotiated = %negotiated,
            roots,
            "initialize",
        );

        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }

        Ok(self.get_info().with_protocol_version(negotiated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> Parameters<JsonObject> {
        Parameters(json.as_object().expect("test args are an object").clone())
    }

    #[tokio::test]
    async fn echo_returns_message_on_success() {
        let server = HledgerMcp::new();
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
        let server = HledgerMcp::new();
        // Dispatch succeeds (no JSON-RPC error); the *result* is flagged isError so
        // the model can self-correct.
        let result = server
            .echo(args(serde_json::json!({ "msg": "typo'd key" })))
            .await
            .expect("echo dispatch must not raise a protocol error on bad args");
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn echo_wrong_type_is_iserror() {
        let server = HledgerMcp::new();
        let result = server
            .echo(args(serde_json::json!({ "message": 42 })))
            .await
            .expect("echo dispatch");
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn status_reports_name_and_version() {
        let server = HledgerMcp::new();
        let result = server.status().await.expect("status dispatch");
        assert_eq!(result.is_error, Some(false));
        let text = &result.content[0].as_text().expect("text content").text;
        assert!(
            text.contains(SERVER_NAME),
            "status names the server: {text}"
        );
        assert!(
            text.contains(env!("CARGO_PKG_VERSION")),
            "status reports the version: {text}"
        );
    }

    #[test]
    fn get_info_declares_tools_only_and_instructions() {
        let info = HledgerMcp::new().get_info();
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
}
