//! The subprocess-execution half of the adapter seam: spawn `hledger`, capture stdout, map
//! failures to a typed [`HledgerError`]. The only place in the crate that runs `hledger`.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// Errors from invoking or parsing `hledger`.
#[derive(Debug, thiserror::Error)]
pub enum HledgerError {
    /// No journal path was configured, but the operation needs one.
    #[error("no journal configured (set --journal or LEDGER_FILE)")]
    NoJournal,

    /// The `hledger` process could not be spawned (binary missing / not executable).
    #[error("could not run hledger ({bin}): {source}")]
    Spawn {
        /// The binary path we attempted.
        bin: String,
        /// The underlying spawn error.
        source: std::io::Error,
    },

    /// `hledger` ran but exited non-zero. `stderr` is hledger's own diagnostic (never the
    /// journal contents — hledger writes errors to stderr, data to stdout).
    #[error("hledger exited unsuccessfully ({status}): {stderr}")]
    NonZero {
        /// The exit status, rendered (e.g. `exit status: 1`).
        status: String,
        /// Captured stderr, trimmed.
        stderr: String,
    },

    /// `hledger`'s stdout did not parse as the expected `-O json` shape.
    #[error("could not parse hledger JSON output: {0}")]
    Parse(#[from] serde_json::Error),

    /// `hledger --version` output was not in the expected `hledger X.Y…` form.
    #[error("unexpected `hledger --version` output: {0}")]
    BadVersion(String),
}

/// Run `bin` with `args`, returning captured stdout on success.
///
/// Logs the invocation (binary + args) at debug. Args may include the journal **path** and
/// query terms — never the journal **contents**, so this is safe to log (CLAUDE.md: never
/// log full journal contents).
pub async fn run(bin: &Path, args: &[String]) -> Result<String, HledgerError> {
    tracing::debug!(bin = %bin.display(), ?args, "hledger invoke");
    let output = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| HledgerError::Spawn {
            bin: bin.display().to_string(),
            source,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        tracing::warn!(status = %output.status, %stderr, "hledger non-zero exit");
        return Err(HledgerError::NonZero {
            status: output.status.to_string(),
            stderr,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn spawn_error_for_missing_binary() {
        let err = run(
            &PathBuf::from("/nonexistent/definitely-not-hledger"),
            &["--version".to_string()],
        )
        .await
        .expect_err("missing binary must error");
        assert!(matches!(err, HledgerError::Spawn { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn non_zero_exit_is_captured() {
        // `false` exits 1 with no output — a portable stand-in for an hledger failure.
        let err = run(&PathBuf::from("/usr/bin/false"), &[])
            .await
            .expect_err("false exits non-zero");
        assert!(matches!(err, HledgerError::NonZero { .. }), "{err:?}");
    }
}
