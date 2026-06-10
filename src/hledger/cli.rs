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

/// `hledger check --strict -f <journal>` — the write-path validator (parse + balanced +
/// declared accounts/commodities). Exit 0 = valid; non-zero stderr carries the diagnostic.
pub fn check_strict_argv(journal: &Path) -> Vec<String> {
    vec![
        "check".to_string(),
        "--strict".to_string(),
        "-f".to_string(),
        journal.display().to_string(),
    ]
}

/// `hledger accounts --declared -f <journal>` — the declared account set (plain text, one per
/// line; `accounts` does **not** support `-O json` in 1.52).
pub fn accounts_declared_argv(journal: &Path) -> Vec<String> {
    vec![
        "accounts".to_string(),
        "--declared".to_string(),
        "-f".to_string(),
        journal.display().to_string(),
    ]
}

/// `hledger accounts --declared tag:^tombstoned$ -f <journal>` — the **tombstoned** subset of
/// the declared accounts (M3 soft-delete). The tag query is anchored: hledger's `tag:` name
/// match is an unanchored regex (verified on 1.52 — `tag:tomb` also matches), so `^…$` pins it
/// to the exact tag name. Empty value (`; tombstoned:`) matches — presence is the flag.
pub fn accounts_tombstoned_argv(journal: &Path) -> Vec<String> {
    vec![
        "accounts".to_string(),
        "--declared".to_string(),
        "tag:^tombstoned$".to_string(),
        "-f".to_string(),
        journal.display().to_string(),
    ]
}

/// `hledger commodities -f <journal>` — the declared commodity set (plain text, one per line;
/// no `-O json` in 1.52).
pub fn commodities_argv(journal: &Path) -> Vec<String> {
    vec![
        "commodities".to_string(),
        "-f".to_string(),
        journal.display().to_string(),
    ]
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

    #[test]
    fn check_strict_argv_shape() {
        assert_eq!(
            check_strict_argv(&journal()),
            vec!["check", "--strict", "-f", "/x/main.journal"]
        );
    }

    #[test]
    fn declared_set_argv_shapes() {
        assert_eq!(
            accounts_declared_argv(&journal()),
            vec!["accounts", "--declared", "-f", "/x/main.journal"]
        );
        assert_eq!(
            commodities_argv(&journal()),
            vec!["commodities", "-f", "/x/main.journal"]
        );
    }

    #[test]
    fn tombstoned_query_is_anchored() {
        // The anchor is load-bearing: hledger tag-name matching is an unanchored regex.
        assert_eq!(
            accounts_tombstoned_argv(&journal()),
            vec![
                "accounts",
                "--declared",
                "tag:^tombstoned$",
                "-f",
                "/x/main.journal"
            ]
        );
    }
}
