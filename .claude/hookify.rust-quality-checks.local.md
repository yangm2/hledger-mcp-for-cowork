---
name: rust-quality-checks
enabled: true
event: file
conditions:
  - field: file_path
    operator: regex_match
    pattern: \.rs$
action: warn
---

🦀 **Rust source edited — hold the strict quality bar (CLAUDE.md).**

Before finishing, make sure these pass (the Stop hook will run them and block on failure):

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`  (zero warnings)
- `cargo nextest run` (or `cargo test`)

Also keep in mind: doc comments on public items, `proptest` coverage for any
parsing/formatting code, and `#![forbid(unsafe_code)]` at the crate root.
