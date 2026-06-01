//! MCP `protocolVersion` negotiation.
//!
//! Kept as a **pure function** of (requested version, supported set) → negotiated
//! version so the lifecycle rule is unit-testable in isolation, separate from the
//! transport and the `rmcp` handler that calls it (M0).
//!
//! **Lifecycle rule** (MCP spec): if the server supports the client's requested
//! version, it responds with the *same* version; otherwise it responds with the
//! *highest* version it supports, and the client decides whether to proceed. We do
//! **not** blind-echo an unknown version back (the gap flagged in
//! `docs/development/mcp-protocol-versions.md`): an unrecognized request is capped to
//! our newest validated revision.

use rmcp::model::ProtocolVersion;

/// Revisions this server has actually validated against, oldest → newest.
///
/// For M0 this is the full set the `rmcp` SDK knows; the newest is the target
/// (`2025-11-25`) and the oldest is the baseline (`2024-11-05`). Narrow this if a
/// future revision changes wire framing in a way we have not validated.
pub const SUPPORTED: &[ProtocolVersion] = ProtocolVersion::KNOWN_VERSIONS;

/// The newest validated revision — what we cap an unknown request to.
pub fn latest_supported() -> ProtocolVersion {
    // `SUPPORTED` is ordered oldest → newest and is never empty.
    SUPPORTED
        .last()
        .cloned()
        .unwrap_or(ProtocolVersion::V_2025_11_25)
}

/// Negotiate the response `protocolVersion` for a client's requested version.
///
/// - requested ∈ supported → echo it (same version);
/// - otherwise → the newest validated revision (cap, never blind-echo).
pub fn negotiate(requested: &ProtocolVersion) -> ProtocolVersion {
    if SUPPORTED.contains(requested) {
        requested.clone()
    } else {
        latest_supported()
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
    fn echoes_each_supported_version() {
        for v in SUPPORTED {
            assert_eq!(
                &negotiate(v),
                v,
                "a supported version must be echoed verbatim"
            );
        }
    }

    #[test]
    fn newest_supported_is_2025_11_25() {
        assert_eq!(latest_supported(), ProtocolVersion::V_2025_11_25);
    }

    #[test]
    fn caps_unknown_future_version_to_latest() {
        // A release-candidate / future revision we have NOT validated.
        let negotiated = negotiate(&version("2026-07-28"));
        assert_eq!(negotiated, latest_supported());
    }

    #[test]
    fn caps_unknown_legacy_version_to_latest() {
        // An ancient/unrecognized revision is likewise not blind-echoed.
        let negotiated = negotiate(&version("2024-01-01"));
        assert_eq!(negotiated, latest_supported());
        assert_ne!(negotiated, version("2024-01-01"));
    }

    #[test]
    fn baseline_2024_11_05_is_supported_and_echoed() {
        let baseline = ProtocolVersion::V_2024_11_05;
        assert!(SUPPORTED.contains(&baseline));
        assert_eq!(negotiate(&baseline), baseline);
    }
}
