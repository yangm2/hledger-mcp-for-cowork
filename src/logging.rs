//! Platform-conventional logging, selected at runtime (M0).
//!
//! `tracing` is the facade everywhere. The subscriber is chosen per platform:
//!
//! - **macOS** → the system unified logger (`os_log`) via the [`apple-log`] crate,
//!   behind a small [`tracing_subscriber::Layer`]. Inspect with
//!   `log stream --predicate 'subsystem == "io.github.yangm2.hledger-mcp-for-cowork"'`
//!   or Console.app.
//! - **other (Linux/container)** → `tracing-subscriber` JSON.
//!
//! Verbosity comes from `RUST_LOG` (an [`EnvFilter`]); the `-v` count only changes the
//! default when `RUST_LOG` is unset (`0` = info, `1` = debug, `2+` = trace).
//!
//! **Logs never go to stdout.** stdout is the MCP JSON-RPC channel on the stdio
//! transport; writing logs there would corrupt the protocol. The Linux subscriber
//! writes JSON to **stderr**; the macOS subscriber writes to `os_log` (not a stream).
//!
//! [`apple-log`]: https://docs.rs/apple-log/0.6.0/apple_log

use tracing_subscriber::EnvFilter;

/// Reverse-DNS unified-logging subsystem (derived from the repo URL).
pub const SUBSYSTEM: &str = "io.github.yangm2.hledger-mcp-for-cowork";
/// Unified-logging category for this server's events.
pub const CATEGORY: &str = "mcp";

/// Build the `RUST_LOG`-aware filter. When `RUST_LOG` is unset, `verbosity` picks the
/// default level: `0` → info, `1` → debug, `2+` → trace.
fn filter(verbosity: u8) -> EnvFilter {
    let fallback = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback))
}

/// Install the process-wide subscriber. Call once, before serving. `verbosity` is the
/// `-v` count (0 = info, 1 = debug, 2+ = trace); ignored when `RUST_LOG` is set.
pub fn init(verbosity: u8) {
    use tracing_subscriber::prelude::*;

    let registry = tracing_subscriber::registry().with(filter(verbosity));

    #[cfg(target_os = "macos")]
    {
        registry.with(macos::OsLogLayer).init();
    }
    #[cfg(not(target_os = "macos"))]
    {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(std::io::stderr),
            )
            .init();
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::fmt::Write as _;

    use apple_log::{Logger, os_log::Level};
    use tracing::field::{Field, Visit};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::layer::{Context, Layer};

    thread_local! {
        /// One `os_log` handle per thread (`Logger` is `!Send`/`!Sync`). `None` only
        /// if the bridge rejects the static subsystem/category — then we drop the
        /// event rather than panic.
        static LOGGER: RefCell<Option<Logger>> =
            RefCell::new(Logger::new(super::SUBSYSTEM, super::CATEGORY).ok());
    }

    /// A `tracing` layer forwarding events to the macOS unified logger.
    ///
    /// Zero-sized and trivially `Send + Sync`; the non-thread-safe `Logger` lives in
    /// a `thread_local`, so the layer itself carries no `os_log` handle.
    pub struct OsLogLayer;

    /// Collects an event's `message` plus any structured fields into one line.
    struct LineVisitor {
        line: String,
    }

    impl Visit for LineVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                // Prepend the human message; keep fields after it.
                let rendered = format!("{value:?}");
                if self.line.is_empty() {
                    self.line = rendered;
                } else {
                    self.line = format!("{rendered} {}", self.line);
                }
            } else {
                let _ = write!(self.line, " {}={value:?}", field.name());
            }
        }
    }

    impl<S: Subscriber> Layer<S> for OsLogLayer {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let meta = event.metadata();
            let mut visitor = LineVisitor {
                line: String::new(),
            };
            event.record(&mut visitor);
            let line = format!("[{}] {}", meta.target(), visitor.line.trim());

            let level = match *meta.level() {
                tracing::Level::TRACE | tracing::Level::DEBUG => Level::Debug,
                tracing::Level::INFO => Level::Info,
                tracing::Level::WARN | tracing::Level::ERROR => Level::Error,
            };

            LOGGER.with(|cell| {
                if let Some(logger) = cell.borrow().as_ref() {
                    logger.log(level, &line);
                }
            });
        }
    }

    #[cfg(test)]
    mod tests {
        use super::OsLogLayer;
        use apple_log::{Logger, os_log::Level};
        use tracing_subscriber::prelude::*;

        // Reading the unified log back programmatically is **not automatable** on
        // macOS (it needs a privileged `log` redirect the operator runs — see the M0
        // milestone). So we assert the deterministic half: the `apple-log` bridge
        // builds a `Logger`, and events flow through `OsLogLayer` without panicking.
        // The end-to-end emit path is additionally exercised by the stdio integration
        // test, whose spawned server logs the handshake through this layer.

        #[test]
        fn bridge_constructs_logger_for_subsystem() {
            let logger = Logger::new(crate::logging::SUBSYSTEM, crate::logging::CATEGORY);
            assert!(
                logger.is_ok(),
                "apple-log bridge must build a Logger for our subsystem/category"
            );
            // Emitting at each level must not panic.
            if let Ok(logger) = logger {
                logger.log(Level::Debug, "logging self-test (debug)");
                logger.log(Level::Info, "logging self-test (info)");
                logger.log(Level::Error, "logging self-test (error)");
            }
        }

        #[test]
        fn events_flow_through_layer_without_panic() {
            // Scoped subscriber (not the global `init`) so this doesn't clash with the
            // process-wide default other tests may rely on.
            let subscriber = tracing_subscriber::registry().with(OsLogLayer);
            tracing::subscriber::with_default(subscriber, || {
                tracing::error!(token = "unit", "oslog layer self-test (error)");
                tracing::info!(
                    client.name = "itest",
                    protocol.negotiated = "2025-11-25",
                    "initialize",
                );
                tracing::debug!("oslog layer self-test (debug)");
            });
        }
    }
}
