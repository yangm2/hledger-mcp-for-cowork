//! The tool catalog's **advertising metadata** (M5, `tool-design.md` MC-8/MC-10): which tier
//! each tool sits in, its one-line Tier-2 summary, and which tools each `--profile`
//! advertises.
//!
//! Advertising only — **dispatch is never filtered**: `tools/call` (and `get_tool`
//! validation) run against the full compiled-in router, so a tool named from a prior session
//! still works under any profile (the MC-10 invariant). Pure module: name sets in, name sets
//! out — unit- and mutation-testable without a server.

use std::str::FromStr;

/// The two always-advertised tiers (`tool-design.md` MC-8). Resources are the third "tier"
/// (zero startup cost) and live in [`crate::resources`], not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Daily read/write/correction tools + diagnostics — full descriptions.
    Operational,
    /// Setup/admin tools — **one-line** descriptions; the detail lives in a resource.
    Administrative,
}

/// One tool's advertising metadata.
pub struct ToolMeta {
    pub name: &'static str,
    pub tier: Tier,
    /// Read-only (never takes the write path) — the `readonly`/`reconcile` profile filter.
    pub read_only: bool,
    /// The one-line description advertised for Tier-2 tools (points at the relevant guide).
    pub summary: &'static str,
}

/// The full catalog. **Must list exactly the router's tool names** — pinned by a test, so
/// adding a tool without classifying it fails the suite.
pub const TOOLS: &[ToolMeta] = &[
    // ---- Tier 1: operational (full descriptions, advertised in every working profile) ----
    ToolMeta {
        name: "status",
        tier: Tier::Operational,
        read_only: true,
        summary: "",
    },
    ToolMeta {
        name: "get_account_balance",
        tier: Tier::Operational,
        read_only: true,
        summary: "",
    },
    ToolMeta {
        name: "list_transactions",
        tier: Tier::Operational,
        read_only: true,
        summary: "",
    },
    ToolMeta {
        name: "get_ap_aging",
        tier: Tier::Operational,
        read_only: true,
        summary: "",
    },
    ToolMeta {
        name: "get_project_summary",
        tier: Tier::Operational,
        read_only: true,
        summary: "",
    },
    ToolMeta {
        name: "post_transaction",
        tier: Tier::Operational,
        read_only: false,
        summary: "",
    },
    ToolMeta {
        name: "receive_invoice",
        tier: Tier::Operational,
        read_only: false,
        summary: "",
    },
    ToolMeta {
        name: "pay_invoice",
        tier: Tier::Operational,
        read_only: false,
        summary: "",
    },
    ToolMeta {
        name: "fund_project",
        tier: Tier::Operational,
        read_only: false,
        summary: "",
    },
    ToolMeta {
        name: "post_interest",
        tier: Tier::Operational,
        read_only: false,
        summary: "",
    },
    ToolMeta {
        name: "update_transaction",
        tier: Tier::Operational,
        read_only: false,
        summary: "",
    },
    ToolMeta {
        name: "void_transaction",
        tier: Tier::Operational,
        read_only: false,
        summary: "",
    },
    // ---- Tier 2: administrative (one-line descriptions; detail in ledger:// guides) ------
    ToolMeta {
        name: "declare_account",
        tier: Tier::Administrative,
        read_only: false,
        summary: "Declare an account before posting to it (see ledger://account-guide).",
    },
    ToolMeta {
        name: "declare_commodity",
        tier: Tier::Administrative,
        read_only: false,
        summary: "Declare a commodity before amounts can use it (see ledger://account-guide).",
    },
    ToolMeta {
        name: "close_account",
        tier: Tier::Administrative,
        read_only: false,
        summary: "Close (tombstone) an account — soft delete (see ledger://account-guide).",
    },
    ToolMeta {
        name: "vendor_add",
        tier: Tier::Administrative,
        read_only: false,
        summary: "Declare a vendor's AP + expense accounts (see ledger://vendor-guide).",
    },
    ToolMeta {
        name: "vendor_list",
        tier: Tier::Administrative,
        read_only: true,
        summary: "List declared vendor AP accounts (see ledger://vendor-guide).",
    },
    ToolMeta {
        name: "budget_set",
        tier: Tier::Administrative,
        read_only: false,
        summary: "Set/replace one account's per-period budget goal (see ledger://budget-guide).",
    },
    ToolMeta {
        name: "budget_list",
        tier: Tier::Administrative,
        read_only: true,
        summary: "List the current budget rules (see ledger://budget-guide).",
    },
    ToolMeta {
        name: "get_budget_vs_actual",
        tier: Tier::Administrative,
        read_only: true,
        summary: "Budget vs actual per account, flagging over-budget (see ledger://budget-guide).",
    },
    ToolMeta {
        name: "eco_create",
        tier: Tier::Administrative,
        read_only: false,
        summary: "Record a pending change order (see ledger://eco-guide).",
    },
    ToolMeta {
        name: "eco_approve",
        tier: Tier::Administrative,
        read_only: false,
        summary: "Approve a pending change order — epoch-checked (see ledger://eco-guide).",
    },
    ToolMeta {
        name: "eco_void",
        tier: Tier::Administrative,
        read_only: false,
        summary: "Void a change order with reversing entries (see ledger://eco-guide).",
    },
    ToolMeta {
        name: "echo",
        tier: Tier::Administrative,
        read_only: true,
        summary: "Echo a message back — connectivity check.",
    },
];

/// Look up a tool's metadata by name.
pub fn meta(name: &str) -> Option<&'static ToolMeta> {
    TOOLS.iter().find(|t| t.name == name)
}

/// The advertising profiles (`tool-design.md` MC-10). Selected once at start (`--profile`);
/// filters `tools/list` only — never dispatch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Profile {
    /// All tools (the default) — general use, agentic tasks.
    #[default]
    Full,
    /// Tier 1 only — daily ledger work.
    Operational,
    /// Read-only tools — balances & reports.
    Readonly,
    /// The administrative tier — pre-construction setup.
    Setup,
    /// Tier 1 + the ECO tools — active construction, tracking change orders.
    Construction,
    /// Reporting/read-only — month-end review. (The reconciliation tools proper — balance
    /// assertions, the `STALE`-meaningful path — land in a later milestone and join here.)
    Reconcile,
}

impl Profile {
    /// All profile names, for `--profile` help/error text.
    pub const NAMES: &[&str] = &[
        "full",
        "operational",
        "readonly",
        "setup",
        "construction",
        "reconcile",
    ];

    /// The lowercase CLI/Display name.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Full => "full",
            Profile::Operational => "operational",
            Profile::Readonly => "readonly",
            Profile::Setup => "setup",
            Profile::Construction => "construction",
            Profile::Reconcile => "reconcile",
        }
    }
}

impl std::fmt::Display for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Profile {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "full" => Ok(Profile::Full),
            "operational" => Ok(Profile::Operational),
            "readonly" => Ok(Profile::Readonly),
            "setup" => Ok(Profile::Setup),
            "construction" => Ok(Profile::Construction),
            "reconcile" => Ok(Profile::Reconcile),
            other => Err(format!(
                "unknown profile '{other}' (expected one of: {})",
                Profile::NAMES.join(", ")
            )),
        }
    }
}

/// Whether `profile` advertises the tool named `name` in `tools/list`. A name missing from
/// [`TOOLS`] is advertised only by `full` (fail-open for the default, fail-closed for the
/// filtered profiles — and the exhaustiveness test makes the case unreachable in practice).
pub fn advertised(profile: Profile, name: &str) -> bool {
    if profile == Profile::Full {
        return true;
    }
    let Some(meta) = meta(name) else {
        return false;
    };
    match profile {
        Profile::Full => true,
        Profile::Operational => meta.tier == Tier::Operational,
        Profile::Readonly | Profile::Reconcile => meta.read_only,
        Profile::Setup => meta.tier == Tier::Administrative || meta.name == "status",
        Profile::Construction => meta.tier == Tier::Operational || meta.name.starts_with("eco_"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_advertises_everything_including_unknown_names() {
        for tool in TOOLS {
            assert!(advertised(Profile::Full, tool.name), "{}", tool.name);
        }
        assert!(advertised(Profile::Full, "tool_from_a_newer_build"));
    }

    #[test]
    fn filtered_profiles_exclude_unknown_names() {
        for profile in [
            Profile::Operational,
            Profile::Readonly,
            Profile::Setup,
            Profile::Construction,
            Profile::Reconcile,
        ] {
            assert!(!advertised(profile, "no_such_tool"), "{profile}");
        }
    }

    #[test]
    fn operational_is_exactly_tier_one() {
        for tool in TOOLS {
            assert_eq!(
                advertised(Profile::Operational, tool.name),
                tool.tier == Tier::Operational,
                "{}",
                tool.name
            );
        }
    }

    #[test]
    fn readonly_and_reconcile_are_exactly_the_read_only_tools() {
        for profile in [Profile::Readonly, Profile::Reconcile] {
            for tool in TOOLS {
                assert_eq!(
                    advertised(profile, tool.name),
                    tool.read_only,
                    "{}",
                    tool.name
                );
            }
        }
        // The boundary that matters: writes are not advertised, reads are.
        assert!(!advertised(Profile::Readonly, "post_transaction"));
        assert!(advertised(Profile::Readonly, "get_account_balance"));
    }

    #[test]
    fn setup_is_the_administrative_tier_plus_status() {
        assert!(advertised(Profile::Setup, "status"));
        assert!(advertised(Profile::Setup, "vendor_add"));
        assert!(advertised(Profile::Setup, "budget_set"));
        assert!(!advertised(Profile::Setup, "post_transaction"));
        assert!(!advertised(Profile::Setup, "get_account_balance"));
    }

    #[test]
    fn construction_is_tier_one_plus_eco() {
        assert!(advertised(Profile::Construction, "post_transaction"));
        assert!(advertised(Profile::Construction, "eco_approve"));
        assert!(!advertised(Profile::Construction, "vendor_add"));
        assert!(!advertised(Profile::Construction, "budget_set"));
    }

    #[test]
    fn every_tier_two_tool_has_a_one_line_summary_and_tier_one_has_none() {
        for tool in TOOLS {
            match tool.tier {
                Tier::Administrative => assert!(
                    !tool.summary.is_empty() && !tool.summary.contains('\n'),
                    "{} needs a one-line summary",
                    tool.name
                ),
                Tier::Operational => {
                    assert!(tool.summary.is_empty(), "{} summary unused", tool.name);
                }
            }
        }
    }

    #[test]
    fn profile_round_trips_through_fromstr_and_display() {
        for name in Profile::NAMES {
            let profile: Profile = name.parse().expect(name);
            assert_eq!(profile.to_string(), *name);
        }
        assert!("bogus".parse::<Profile>().is_err());
    }
}
