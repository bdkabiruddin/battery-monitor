## Summary

<!-- What does this change, and why? -->

## Test plan

<!--
List what you actually ran/verified, and how. If you touched UI or a
pkexec-gated feature (PowerTop scan, charge threshold write), say what
you verified by hand against a real display/session, and what you
couldn't verify and why. Don't claim something works based on the code
compiling or reading correctly.
-->

- [ ] `cargo test --manifest-path core/Cargo.toml` passes
- [ ] `cargo clippy` clean on `core/` and (if `app/` changed) `app/`
- [ ] (if UI/PowerTop/charge-threshold changed) manually verified against a real display/session
