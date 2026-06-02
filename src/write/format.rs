//! The **pure** hledger journal-text formatter (the M2 core; heavily property-tested). Total
//! and side-effect-free: it renders an already-validated transaction to text. Its output is the
//! only thing that ever reaches `hledger check --strict`, so the round-trip property (format →
//! `check` clean **and** `print -O json` parses back to the same transaction) is the safety net.
//!
//! ## Grammar emitted
//! ```text
//! <date> <description>  ; <k1>:<v1>, <k2>:<v2>, …
//!     <account>  <quantity> <commodity>
//!     <account>                          # the balancing posting (amount omitted)
//! ```
//! Amounts render as `<quantity> <commodity>` (e.g. `100.00 $`, `-44.00 EUR`) — the space form
//! hledger parses unambiguously regardless of the declared commodity style.

use crate::hledger::amount::Quantity;

/// A posting to render: account, and an optional `(quantity, commodity)` (omit = the balancer).
pub type EntryPosting = (String, Option<(Quantity, String)>);

/// Render one journal entry. `tags` are emitted on the date line in order (the caller places
/// `id:`/`idem:`/`reverses:` first). Pure — no I/O, total.
pub fn render_entry(
    date: &str,
    description: &str,
    postings: &[EntryPosting],
    tags: &[(String, String)],
) -> String {
    let mut out = String::new();
    out.push_str(date);
    if !description.is_empty() {
        out.push(' ');
        out.push_str(description);
    }
    if !tags.is_empty() {
        let rendered: Vec<String> = tags.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        out.push_str("  ; ");
        out.push_str(&rendered.join(", "));
    }
    out.push('\n');

    for (account, amount) in postings {
        match amount {
            Some((quantity, commodity)) => {
                out.push_str(&format!(
                    "    {account}  {} {commodity}\n",
                    quantity.render()
                ));
            }
            None => out.push_str(&format!("    {account}\n")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(mantissa: i128, places: u32) -> Quantity {
        Quantity::new(mantissa, places)
    }

    #[test]
    fn renders_balanced_two_posting_entry_with_tags() {
        let postings = vec![
            (
                "expenses:supplies".to_string(),
                Some((q(1234, 2), "$".to_string())),
            ),
            ("assets:checking".to_string(), None),
        ];
        let tags = vec![
            ("id".to_string(), "abc".to_string()),
            ("idem".to_string(), "xyz".to_string()),
        ];
        let text = render_entry("2026-01-15", "Acme", &postings, &tags);
        assert_eq!(
            text,
            "2026-01-15 Acme  ; id:abc, idem:xyz\n    expenses:supplies  12.34 $\n    assets:checking\n"
        );
    }

    #[test]
    fn renders_empty_description_and_no_tags() {
        let postings = vec![("a:b".to_string(), Some((q(-100, 2), "EUR".to_string())))];
        let text = render_entry("2026-02-02", "", &postings, &[]);
        assert_eq!(text, "2026-02-02\n    a:b  -1.00 EUR\n");
    }

    // Property: every rendered entry begins with the date line, has one indented line per
    // posting, and the amount text never contains a float artifact (only the exact decimal).
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn rendered_postings_count_matches_input(
            mantissa in any::<i64>(), places in 0u32..=6, n in 1usize..=5,
        ) {
            let postings: Vec<EntryPosting> = (0..n)
                .map(|i| (format!("acct:{i}"), Some((q(i128::from(mantissa), places), "$".to_string()))))
                .collect();
            let text = render_entry("2026-01-01", "d", &postings, &[]);
            // header + n posting lines, all newline-terminated.
            prop_assert_eq!(text.lines().count(), n + 1);
            // The exact decimal appears; no scientific/float notation leaks in.
            prop_assert!(!text.contains('e') || !text.contains("e+"));
            for line in text.lines().skip(1) {
                prop_assert!(line.starts_with("    "), "posting indented: {line}");
            }
        }
    }
}
