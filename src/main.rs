//! hledger MCP server — binary entrypoint (M0).
//!
//! Parses the CLI, installs the platform logging subscriber, then serves the MCP
//! protocol over **stdio** until the client disconnects (stdin EOF) or a termination
//! signal arrives.

#![forbid(unsafe_code)]

use anyhow::Context as _;
use clap::Parser;
use hledger_mcp_for_cowork::HledgerMcp;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tokio_util::sync::CancellationToken;

/// hledger MCP server for Claude Cowork (stdio transport).
#[derive(Debug, Parser)]
#[command(name = "hledger-mcp", version, about)]
struct Cli {
    /// Increase log verbosity: `-v` = debug, `-vv` (or more) = trace. Ignored when
    /// `RUST_LOG` is set (that wins).
    #[arg(long, short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    hledger_mcp_for_cowork::logging::init(cli.verbose);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting hledger-mcp (stdio)"
    );

    let ct = CancellationToken::new();
    let signal_ct = ct.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received; cancelling");
        signal_ct.cancel();
    });

    let service = HledgerMcp::new()
        .serve_with_ct(stdio(), ct)
        .await
        .context("failed to start MCP stdio service")?;

    let reason = service.waiting().await.context("MCP service task failed")?;
    tracing::info!(?reason, "server stopped");
    Ok(())
}

/// Resolve when the process receives SIGINT (Ctrl-C) or, on Unix, SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(error) => {
                tracing::warn!(%error, "cannot install SIGTERM handler; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
