//! hledger MCP server — binary entrypoint (M0).
//!
//! Parses the CLI, installs the platform logging subscriber, then serves the MCP
//! protocol over **stdio** until the client disconnects (stdin EOF) or a termination
//! signal arrives.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use hledger_mcp_for_cowork::HledgerMcp;
use hledger_mcp_for_cowork::hledger::Hledger;
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

    /// Path to the hledger journal to serve. Defaults to the `LEDGER_FILE` environment
    /// variable (hledger's own convention); read tools error until one is set.
    #[arg(long, env = "LEDGER_FILE")]
    journal: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    hledger_mcp_for_cowork::logging::init(cli.verbose);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting hledger-mcp (stdio)"
    );

    // Resolve the hledger adapter and check the version pin at startup. Policy (M1):
    // warn-and-continue for reads — a mismatch is logged loudly and surfaced in `status`,
    // not fatal. The write path (M2) hard-gates on the pin before mutating anything.
    let hledger = Hledger::from_env(cli.journal.clone());
    match hledger.version().await {
        Ok(version) if version.pin_matches() => {
            tracing::info!(hledger.version = %version.raw, "hledger detected (pin OK)");
        }
        Ok(version) => {
            tracing::warn!(
                hledger.version = %version.raw,
                expected = ?hledger_mcp_for_cowork::hledger::PINNED_VERSION,
                "hledger version does not match the pinned 1.52 — read output may differ; \
                 writes (M2) will refuse to run",
            );
        }
        Err(err) => {
            tracing::warn!(%err, "could not detect hledger version at startup; read tools will \
                                  error until a working hledger is configured");
        }
    }
    if !hledger.has_journal() {
        tracing::warn!(
            "no journal configured (--journal or LEDGER_FILE); read tools will error until set"
        );
    }

    let ct = CancellationToken::new();
    let signal_ct = ct.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received; cancelling");
        signal_ct.cancel();
    });

    let service = HledgerMcp::new(hledger)
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
