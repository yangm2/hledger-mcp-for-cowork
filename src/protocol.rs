//! MCP `protocolVersion` negotiation.
//!
//! Kept as **pure functions** of the client's requested version so the lifecycle rule is
//! unit-testable in isolation, separate from the transport and the `rmcp` handler (M0).
//!
//! **Lifecycle rule** (MCP spec): if the server supports the client's requested version it
//! responds with the *same* version; otherwise it responds with a version it supports
//! (we use the newest) and the client decides whether to proceed.
//!
//! **rmcp performs the final reconciliation — read this before trusting [`negotiate`].**
//! Our handler returns a *preferred* response version, but `rmcp`'s serve loop then sets
//! the wire version to `min(client_requested, our_response)` — a **lexicographic** compare
//! over the date string (`rmcp` `service/server.rs`, `Ordering::Less => client`). So the
//! value a peer actually receives is [`effective_version`], not [`negotiate`] alone:
//!
//! - a **known** requested version is echoed;
//! - an **unknown newer** version (lexically `>` our newest) is capped to
//!   [`latest_supported`];
//! - an **unknown older** version (lexically `<` our newest) is returned **as the client
//!   requested it** — `rmcp`'s `min` picks the client's value and the handler cannot
//!   override it from its return value. We surface this here (and test it) rather than
//!   pretend [`negotiate`]'s cap reaches the wire for that case.

use std::cmp::Ordering;

use rmcp::model::ProtocolVersion;

/// Protocol revisions this server accepts — the set the `rmcp` SDK knows how to frame.
///
/// The **tested target** is `2025-11-25` (newest) with `2024-11-05` as the baseline; the
/// in-between revisions (`2025-03-26`, `2025-06-18`) are accepted because their wire framing
/// is compatible with our single-object-per-line transport, **not** because each has
/// dedicated coverage. Narrow this set if a revision's framing ever diverges (e.g. a client
/// that negotiates `2025-03-26` may attempt JSON-RPC batching, removed in `2025-06-18`).
pub const SUPPORTED: &[ProtocolVersion] = ProtocolVersion::KNOWN_VERSIONS;

/// Our newest accepted revision — what an unknown-newer request is capped to.
///
/// Single source of truth is the SDK's [`ProtocolVersion::LATEST`]; a unit test asserts it
/// equals the last entry of [`SUPPORTED`], so the two cannot silently drift.
pub fn latest_supported() -> ProtocolVersion {
    ProtocolVersion::LATEST
}

/// Our **preferred** response version for a client's request, *before* rmcp's reconciliation
/// (see the module docs): a known version is echoed; anything unknown caps to
/// [`latest_supported`]. The value that reaches the wire is [`effective_version`].
pub fn negotiate(requested: &ProtocolVersion) -> ProtocolVersion {
    if SUPPORTED.contains(requested) {
        requested.clone()
    } else {
        latest_supported()
    }
}

/// The version a client will **actually** receive on the wire: `min(requested,
/// negotiate(requested))`, mirroring rmcp's `service/server.rs` reconciliation. Use this for
/// logging/diagnostics (and the `status` tool) so reported state matches what the peer sees.
pub fn effective_version(requested: &ProtocolVersion) -> ProtocolVersion {
    let preferred = negotiate(requested);
    match requested.partial_cmp(&preferred) {
        Some(Ordering::Less) => requested.clone(),
        _ => preferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `ProtocolVersion` from a wire string (the only public constructor is
    /// `Deserialize`), so tests can express arbitrary/unknown revisions.
    fn version(s: &str) -> ProtocolVersion {
        serde_json::from_value(serde_json::Value::String(s.to_owned()))
            .expect("ProtocolVersion deserializes from any string")
    }

    #[test]
    fn supported_newest_equals_latest_supported() {
        // Guards the single-source-of-truth: `latest_supported()` (== SDK LATEST) must be
        // the newest entry of `SUPPORTED`, else `negotiate`/`effective_version` cap wrong.
        assert_eq!(SUPPORTED.last().cloned(), Some(latest_supported()));
        assert_eq!(latest_supported(), ProtocolVersion::V_2025_11_25);
    }

    // --- negotiate: our preferred response (pre-reconciliation) ---

    #[test]
    fn negotiate_echoes_each_supported_version() {
        for v in SUPPORTED {
            assert_eq!(
                &negotiate(v),
                v,
                "a supported version is the preferred response"
            );
        }
    }

    #[test]
    fn negotiate_caps_unknown_versions_to_latest() {
        // Both an unknown future RC and an unknown legacy revision *prefer* the newest.
        assert_eq!(negotiate(&version("2026-07-28")), latest_supported());
        assert_eq!(negotiate(&version("2024-01-01")), latest_supported());
    }

    // --- effective_version: what actually reaches the wire (rmcp's min) ---

    #[test]
    fn effective_echoes_supported_version() {
        assert_eq!(
            effective_version(&ProtocolVersion::V_2024_11_05),
            ProtocolVersion::V_2024_11_05
        );
    }

    #[test]
    fn effective_caps_unknown_newer_to_latest() {
        // Lexically > newest → rmcp keeps our cap.
        assert_eq!(
            effective_version(&version("2026-07-28")),
            latest_supported()
        );
    }

    #[test]
    fn effective_returns_unknown_older_as_requested() {
        // Lexically < newest → rmcp's min picks the client's version; our cap does NOT reach
        // the wire here. This documents the rmcp limitation rather than asserting a fiction.
        let legacy = version("2024-01-01");
        assert_eq!(effective_version(&legacy), legacy);
        assert_ne!(effective_version(&legacy), latest_supported());
    }
}
