//! Input validation — the **input/internal boundary**. Everything here runs *before* the
//! formatter, so a validated transaction that later fails `hledger check --strict` can only be
//! our formatter bug (an internal error), never bad input. Failures are returned as correctable
//! tool errors (`Err(String)`).
//!
//! Checks: ≥2 postings with at most one missing amount; every
//! account/commodity **declared** (require-pre-declare); exact-decimal amounts; balanced per
//! commodity when no posting is left to balance; safe description/tag text; no reserved tags.

use std::collections::{HashMap, HashSet};

use crate::hledger::amount::Quantity;

use super::format::EntryPosting;
use super::input::TransactionInput;

/// Tag names the system owns; user input may not set them.
const RESERVED_TAGS: [&str; 3] = ["id", "idem", "reverses"];

/// A validated transaction: parsed amounts, verified balanced & declared, ready to format.
#[derive(Debug)]
pub struct ValidatedTxn {
    pub date: chrono::NaiveDate,
    pub description: String,
    pub postings: Vec<EntryPosting>,
    pub tags: Vec<(String, String)>,
}

/// Validate `input` against the journal's declared accounts/commodities.
pub fn validate(
    input: &TransactionInput,
    declared_accounts: &HashSet<String>,
    declared_commodities: &HashSet<String>,
) -> Result<ValidatedTxn, String> {
    validate_text("description", &input.description)?;

    if input.postings.len() < 2 {
        return Err("a transaction needs at least 2 postings".to_string());
    }

    let mut postings: Vec<EntryPosting> = Vec::with_capacity(input.postings.len());
    let mut missing = 0usize;
    for posting in &input.postings {
        if !declared_accounts.contains(&posting.account) {
            return Err(format!(
                "account not declared: '{}' — declare it first with declare_account",
                posting.account
            ));
        }
        let amount = match &posting.amount {
            None => {
                missing += 1;
                None
            }
            Some(amount) => {
                let quantity = Quantity::parse(&amount.quantity).ok_or_else(|| {
                    format!(
                        "invalid amount '{}' for account '{}'",
                        amount.quantity, posting.account
                    )
                })?;
                if !declared_commodities.contains(&amount.commodity) {
                    return Err(format!(
                        "commodity not declared: '{}' — declare it first with declare_commodity",
                        amount.commodity
                    ));
                }
                Some((quantity, amount.commodity.clone()))
            }
        };
        postings.push((posting.account.clone(), amount));
    }

    if missing > 1 {
        return Err("at most one posting may omit its amount (the balancing posting)".to_string());
    }
    if missing == 0 {
        // No posting left to balance, so the explicit amounts must sum to zero per commodity.
        let mut sums: HashMap<&str, Quantity> = HashMap::new();
        for (_, amount) in &postings {
            if let Some((quantity, commodity)) = amount {
                let entry = sums
                    .entry(commodity.as_str())
                    .or_insert(Quantity::new(0, 0));
                *entry = *entry + *quantity;
            }
        }
        for (commodity, sum) in &sums {
            if !sum.is_zero() {
                return Err(format!(
                    "unbalanced: {commodity} postings sum to {} (must be 0, or omit one amount to balance)",
                    sum.render()
                ));
            }
        }
    }

    let mut tags = Vec::with_capacity(input.tags.len());
    for (key, value) in &input.tags {
        if RESERVED_TAGS.contains(&key.as_str()) {
            return Err(format!("'{key}' is a reserved tag and cannot be set"));
        }
        validate_tag_key(key)?;
        validate_text("tag value", value)?;
        if value.contains(',') {
            return Err(format!("tag value for '{key}' must not contain a comma"));
        }
        tags.push((key.clone(), value.clone()));
    }

    Ok(ValidatedTxn {
        date: input.date,
        description: input.description.clone(),
        postings,
        tags,
    })
}

/// Free text that must stay on one line and not open a comment (`;`).
fn validate_text(what: &str, text: &str) -> Result<(), String> {
    if text.contains('\n') || text.contains(';') {
        return Err(format!("{what} must not contain a newline or ';'"));
    }
    Ok(())
}

/// A tag key: non-empty, no whitespace, no `:`/`,`/newline.
fn validate_tag_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || key
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ':' | ','))
    {
        return Err(format!("invalid tag name: '{key}'"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::input::{PostingAmount, PostingInput};

    fn declared() -> (HashSet<String>, HashSet<String>) {
        (
            [
                "assets:checking",
                "expenses:supplies",
                "equity:opening balances",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            ["$", "EUR"].into_iter().map(String::from).collect(),
        )
    }

    fn posting(account: &str, qty: Option<&str>, commodity: &str) -> PostingInput {
        PostingInput {
            account: account.to_string(),
            amount: qty.map(|q| PostingAmount {
                quantity: q.to_string(),
                commodity: commodity.to_string(),
            }),
        }
    }

    fn txn(postings: Vec<PostingInput>) -> TransactionInput {
        TransactionInput {
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
            description: "Acme".to_string(),
            postings,
            tags: vec![],
            idem: None,
        }
    }

    #[test]
    fn accepts_balanced_with_explicit_amounts() {
        let (a, c) = declared();
        let t = txn(vec![
            posting("expenses:supplies", Some("12.34"), "$"),
            posting("assets:checking", Some("-12.34"), "$"),
        ]);
        let v = validate(&t, &a, &c).expect("valid");
        assert_eq!(v.postings.len(), 2);
    }

    #[test]
    fn accepts_one_omitted_amount_as_balancer() {
        let (a, c) = declared();
        let t = txn(vec![
            posting("expenses:supplies", Some("12.34"), "$"),
            posting("assets:checking", None, ""),
        ]);
        assert!(validate(&t, &a, &c).is_ok());
    }

    #[test]
    fn rejects_unbalanced_when_no_balancer() {
        let (a, c) = declared();
        let t = txn(vec![
            posting("expenses:supplies", Some("12.34"), "$"),
            posting("assets:checking", Some("-10.00"), "$"),
        ]);
        let err = validate(&t, &a, &c).unwrap_err();
        assert!(err.contains("unbalanced"), "{err}");
    }

    #[test]
    fn rejects_undeclared_account_and_commodity() {
        let (a, c) = declared();
        let bad_acct = txn(vec![
            posting("assets:savings", Some("1.00"), "$"),
            posting("assets:checking", None, ""),
        ]);
        assert!(
            validate(&bad_acct, &a, &c)
                .unwrap_err()
                .contains("account not declared")
        );

        let bad_comm = txn(vec![
            posting("expenses:supplies", Some("1.00"), "GBP"),
            posting("assets:checking", None, ""),
        ]);
        assert!(
            validate(&bad_comm, &a, &c)
                .unwrap_err()
                .contains("commodity not declared")
        );
    }

    #[test]
    fn rejects_too_few_postings_and_multiple_balancers() {
        let (a, c) = declared();
        let one = txn(vec![posting("assets:checking", Some("1.00"), "$")]);
        assert!(validate(&one, &a, &c).unwrap_err().contains("at least 2"));

        let two_missing = txn(vec![
            posting("expenses:supplies", None, ""),
            posting("assets:checking", None, ""),
        ]);
        assert!(
            validate(&two_missing, &a, &c)
                .unwrap_err()
                .contains("at most one")
        );
    }

    #[test]
    fn rejects_bad_amount() {
        let (a, c) = declared();
        let bad_amt = txn(vec![
            posting("expenses:supplies", Some("12.3.4"), "$"),
            posting("assets:checking", None, ""),
        ]);
        assert!(
            validate(&bad_amt, &a, &c)
                .unwrap_err()
                .contains("invalid amount")
        );
    }

    #[test]
    fn rejects_reserved_tags_and_unsafe_text() {
        let (a, c) = declared();
        let mut t = txn(vec![
            posting("expenses:supplies", Some("1.00"), "$"),
            posting("assets:checking", None, ""),
        ]);
        t.tags = vec![("id".to_string(), "x".to_string())];
        assert!(validate(&t, &a, &c).unwrap_err().contains("reserved"));

        t.tags = vec![];
        t.description = "bad ; comment".to_string();
        assert!(validate(&t, &a, &c).unwrap_err().contains("';'"));
    }

    #[test]
    fn validate_tag_key_isolates_each_rejection() {
        assert!(validate_tag_key("vendor").is_ok());
        assert!(validate_tag_key("").is_err(), "empty");
        assert!(validate_tag_key("a b").is_err(), "space");
        assert!(validate_tag_key("a\tb").is_err(), "tab");
        assert!(validate_tag_key("a:b").is_err(), "colon");
        assert!(validate_tag_key("a,b").is_err(), "comma");
    }
}
