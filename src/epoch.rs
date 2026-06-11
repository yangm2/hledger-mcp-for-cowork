//! The epoch CAS — the agent-side correctness layer (M3, `concurrency-model.md`).
//!
//! The **epoch is the git commit**: one validated write = one commit, so a `HEAD` oid
//! identifies exactly what a client has seen. The server tracks, per connection, the
//! last epoch that connection *read*; a **decide** call (one acting on a belief about
//! ledger state) is rejected [`Stale`] unless that last-seen epoch equals the current
//! `HEAD` — forcing a re-read instead of acting on a stale belief. **Record** calls
//! (append-only posts/corrections) carry a transaction-local invariant and an
//! idempotency key, so they are safe at any epoch and never epoch-checked.
//!
//! This module is the *pure* state machine: no I/O, no locks. Lock ordering and the
//! TOCTOU discipline (the check must run **inside** the write locks) live in
//! [`crate::write::ConnectionView::guarded`]; the formal model is `proofs/tla/Ledger.tla`.

/// A git commit oid — the epoch identifier and the outcome stamp on every write.
///
/// Wraps the 40-char hex string produced by libgit2. `short()` trims to 12 chars for
/// human-facing messages; `Display` emits the full oid.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitOid(String);

impl CommitOid {
    pub fn new(s: String) -> Self {
        debug_assert!(
            s.chars().all(|c| c.is_ascii_hexdigit()),
            "CommitOid must contain only hex digits, got: {s:?}"
        );
        Self(s)
    }

    /// First 12 hex chars — enough to identify a commit in human-facing messages.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(12)]
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CommitOid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::ops::Deref for CommitOid {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

/// The record-vs-decide partition (`concurrency-model.md`). Every write tool declares
/// which class it is in; only [`Decide`](ToolClass::Decide) calls are epoch-checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// Append-only (post / void-as-reversal / declare): balanced-by-construction and
    /// idempotency-keyed, so safe at any epoch. **Never** rejected `STALE`.
    Record,
    /// Consequential (approve-because-budget, release-because-cash-positive): acts on a
    /// belief about ledger state, so it must have read the **latest** epoch.
    Decide,
}

/// An epoch: the journal repo's `HEAD` commit oid. `None` = unborn `HEAD` (a fresh
/// repo with no commits yet) — still a legitimate, comparable epoch.
///
/// Sampled fresh per use (never cached process-wide the way the hledger version is —
/// the version is process-constant, the epoch changes on every write).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Epoch(Option<CommitOid>);

impl Epoch {
    /// Wrap a `HEAD` oid (`None` for an unborn `HEAD`).
    pub fn new(oid: Option<CommitOid>) -> Self {
        Self(oid)
    }

    /// The underlying commit oid as a `&str`, if `HEAD` is born.
    pub fn oid(&self) -> Option<&str> {
        self.0.as_ref().map(CommitOid::as_str)
    }

    /// Short (12-char) form for human-facing messages.
    pub fn short(&self) -> String {
        match &self.0 {
            Some(oid) => oid.short().to_string(),
            None => "(unborn)".to_string(),
        }
    }
}

/// A decide call was built on a stale read: the connection's last-seen epoch (if it
/// ever read) does not match the current `HEAD`. The remedy is always available and
/// held-resource-free: re-read, then retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    /// What the connection last saw (`None` = it never read).
    pub seen: Option<Epoch>,
    /// The current `HEAD` epoch it must catch up to.
    pub head: Epoch,
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seen = match &self.seen {
            Some(epoch) => epoch.short(),
            None => "(never read)".to_string(),
        };
        write!(
            f,
            "STALE: this decision was made against epoch {seen} but the ledger is now at \
             {}. Re-read the relevant state, then retry.",
            self.head.short()
        )
    }
}

/// The CAS itself: pure, total, and the subject of the mutation-testing gate.
///
/// - [`ToolClass::Record`] always passes (append-only calls are epoch-free).
/// - [`ToolClass::Decide`] passes iff the connection has read (`last_seen` is `Some`)
///   **and** what it read is the current `HEAD`. A connection that never read is
///   stale by definition — it has no grounded belief to act on.
pub fn check(class: ToolClass, last_seen: Option<&Epoch>, head: &Epoch) -> Result<(), Stale> {
    match class {
        ToolClass::Record => Ok(()),
        ToolClass::Decide if last_seen == Some(head) => Ok(()),
        ToolClass::Decide => Err(Stale {
            seen: last_seen.cloned(),
            head: head.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(oid: &str) -> Epoch {
        Epoch::new(Some(CommitOid::new(oid.to_string())))
    }

    #[test]
    fn record_passes_at_any_epoch() {
        let head = epoch("aaaa");
        assert_eq!(check(ToolClass::Record, None, &head), Ok(()));
        assert_eq!(
            check(ToolClass::Record, Some(&epoch("bbbb")), &head),
            Ok(())
        );
        assert_eq!(check(ToolClass::Record, Some(&head), &head), Ok(()));
    }

    #[test]
    fn decide_passes_only_on_exact_match() {
        let head = epoch("aaaa");
        assert_eq!(
            check(ToolClass::Decide, Some(&epoch("aaaa")), &head),
            Ok(())
        );
        let stale = check(ToolClass::Decide, Some(&epoch("bbbb")), &head).unwrap_err();
        assert_eq!(stale.seen, Some(epoch("bbbb")));
        assert_eq!(stale.head, head);
    }

    #[test]
    fn decide_without_a_prior_read_is_stale() {
        // A connection that never read has no grounded belief — even on an unborn repo
        // a decide must be preceded by a read.
        let stale = check(ToolClass::Decide, None, &epoch("aaaa")).unwrap_err();
        assert_eq!(stale.seen, None);
        assert!(stale.to_string().contains("never read"));
    }

    #[test]
    fn unborn_head_is_a_real_epoch() {
        let unborn = Epoch::new(None);
        // Read-then-decide on a fresh repo: last-seen = unborn = HEAD → passes.
        assert_eq!(check(ToolClass::Decide, Some(&unborn), &unborn), Ok(()));
        // But unborn ≠ a born epoch.
        assert!(check(ToolClass::Decide, Some(&unborn), &epoch("aaaa")).is_err());
        assert!(check(ToolClass::Decide, Some(&epoch("aaaa")), &unborn).is_err());
    }

    #[test]
    fn short_forms_render_for_humans() {
        assert_eq!(epoch("0123456789abcdef").short(), "0123456789ab");
        assert_eq!(epoch("ab").short(), "ab");
        assert_eq!(Epoch::new(None).short(), "(unborn)");
        let msg = Stale {
            seen: Some(epoch("0123456789abcdef")),
            head: epoch("fedcba9876543210"),
        }
        .to_string();
        assert!(msg.contains("STALE"), "{msg}");
        assert!(msg.contains("0123456789ab"), "{msg}");
        assert!(msg.contains("fedcba987654"), "{msg}");
        assert!(msg.contains("Re-read"), "re-read hint present: {msg}");
    }
}
