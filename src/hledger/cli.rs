//! The CLI-command builder half of the adapter seam.
//!
//! Pure functions that construct the exact `hledger` argument vectors — no spawning, no I/O.
//! Keeping argv construction here (and unit-testing it) means the shape of every invocation
//! is reviewable in one place and a query change can't silently alter an unrelated command.

use std::path::Path;

/// `hledger --version` — the startup pin check; needs no journal.
pub fn version_argv() -> Vec<String> {
    vec!["--version".to_string()]
}

/// `hledger balance [account] -O json -f <journal>`.
///
/// `account` is an optional account-name query; `None` reports all accounts.
pub fn balance_argv(journal: &Path, account: Option<&str>) -> Vec<String> {
    let mut argv = vec!["balance".to_string()];
    if let Some(account) = account {
        argv.push(account.to_string());
    }
    argv.push("-O".to_string());
    argv.push("json".to_string());
    argv.push("-f".to_string());
    argv.push(journal.display().to_string());
    argv
}

/// `hledger print [query…] -O json -f <journal>`.
///
/// `query` is hledger's query language (account/`desc:`/`date:`/`tag:` terms) passed through
/// verbatim as separate argv tokens; an empty query prints every transaction.
pub fn print_argv(journal: &Path, query: &[String]) -> Vec<String> {
    let mut argv = vec!["print".to_string()];
    argv.extend(query.iter().cloned());
    argv.push("-O".to_string());
    argv.push("json".to_string());
    argv.push("-f".to_string());
    argv.push(journal.display().to_string());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn journal() -> PathBuf {
        PathBuf::from("/x/main.journal")
    }

    #[test]
    fn version_is_just_the_flag() {
        assert_eq!(version_argv(), vec!["--version"]);
    }

    #[test]
    fn balance_with_account() {
        assert_eq!(
            balance_argv(&journal(), Some("assets:checking")),
            vec![
                "balance",
                "assets:checking",
                "-O",
                "json",
                "-f",
                "/x/main.journal"
            ]
        );
    }

    #[test]
    fn balance_without_account_omits_the_query_token() {
        assert_eq!(
            balance_argv(&journal(), None),
            vec!["balance", "-O", "json", "-f", "/x/main.journal"]
        );
    }

    #[test]
    fn print_passes_query_terms_through_verbatim() {
        assert_eq!(
            print_argv(
                &journal(),
                &["desc:Acme".to_string(), "date:2026".to_string()]
            ),
            vec![
                "print",
                "desc:Acme",
                "date:2026",
                "-O",
                "json",
                "-f",
                "/x/main.journal"
            ]
        );
    }

    #[test]
    fn print_with_empty_query() {
        assert_eq!(
            print_argv(&journal(), &[]),
            vec!["print", "-O", "json", "-f", "/x/main.journal"]
        );
    }
}
