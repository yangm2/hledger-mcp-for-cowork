//! The `ledger://` resources (M5, `tool-design.md` MC-8): the verbose guides that keep
//! Tier-2 tool descriptions one line, fetched on demand at **zero startup cost**.
//!
//! All static content is authored as real `.md` files under `src/resources/` and compiled
//! in via `include_str!` (CLAUDE.md *Conventions*) — diffable prose, single self-contained
//! binary. Serving a static resource (and `resources/list`) **never touches hledger**; the
//! one dynamic resource, [`VENDORS_URI`], is the documented exception (it reads the live
//! vendor list, and only when actually read).

/// One compiled-in static resource.
pub struct StaticResource {
    pub uri: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub content: &'static str,
}

/// The resource `server_instructions` directs clients to read before any tool call.
pub const SESSION_CONTEXT_URI: &str = "ledger://session-context";

/// The dynamic live-vendor resource (the only one that hits hledger, at read time).
pub const VENDORS_URI: &str = "ledger://vendors";

/// All static resources, in the order `resources/list` advertises them.
pub const STATIC: &[StaticResource] = &[
    StaticResource {
        uri: SESSION_CONTEXT_URI,
        name: "session-context",
        title: "Session context — read this first",
        description: "Tool groups, ledger conventions, and the resource index. Read before \
                      the first tool call.",
        content: include_str!("resources/session-context.md"),
    },
    StaticResource {
        uri: "ledger://account-guide",
        name: "account-guide",
        title: "Account guide",
        description: "Account types, naming conventions, declaration and soft-delete rules.",
        content: include_str!("resources/account-guide.md"),
    },
    StaticResource {
        uri: "ledger://vendor-guide",
        name: "vendor-guide",
        title: "Vendor guide",
        description: "Trade vs professional vendors, permits, and GC pass-through invoices.",
        content: include_str!("resources/vendor-guide.md"),
    },
    StaticResource {
        uri: "ledger://expected-chart",
        name: "expected-chart",
        title: "Expected chart of accounts",
        description: "The full expected account tree for a construction project.",
        content: include_str!("resources/expected-chart.md"),
    },
    StaticResource {
        uri: "ledger://budget-guide",
        name: "budget-guide",
        title: "Budget guide",
        description: "Budget workflow: periodic rules, budget_set semantics, budget vs actual.",
        content: include_str!("resources/budget-guide.md"),
    },
    StaticResource {
        uri: "ledger://eco-guide",
        name: "eco-guide",
        title: "Change-order (ECO) guide",
        description: "ECO lifecycle: create (pending) -> approve (epoch-checked) -> void.",
        content: include_str!("resources/eco-guide.md"),
    },
];

/// Look up a static resource by URI.
pub fn find_static(uri: &str) -> Option<&'static StaticResource> {
    STATIC.iter().find(|r| r.uri == uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_are_unique_and_findable() {
        for resource in STATIC {
            let found = find_static(resource.uri).expect(resource.uri);
            assert_eq!(found.name, resource.name);
        }
        assert!(find_static(VENDORS_URI).is_none(), "vendors is dynamic");
        assert!(find_static("ledger://nope").is_none());
    }

    #[test]
    fn every_guide_has_substantive_content_and_metadata() {
        for resource in STATIC {
            assert!(resource.uri.starts_with("ledger://"), "{}", resource.uri);
            assert!(
                resource.content.len() > 200,
                "{} content suspiciously short",
                resource.name
            );
            assert!(!resource.description.is_empty(), "{}", resource.name);
        }
    }

    /// The session-context resource indexes every other resource (including the dynamic one)
    /// and never goes stale against the list.
    #[test]
    fn session_context_indexes_all_resources() {
        let session = find_static(SESSION_CONTEXT_URI).expect("session-context");
        for resource in STATIC {
            assert!(
                session.content.contains(resource.uri),
                "session-context missing {}",
                resource.uri
            );
        }
        assert!(session.content.contains(VENDORS_URI));
    }
}
