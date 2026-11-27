//! tekvra-eval: hermetic golden-task evals + perf smokes.
//!
//! Run as part of `cargo test --workspace`; `cargo test --locked -p tekvra-eval`
//! alone is the eval gate referenced in the README dev section. Everything is
//! deterministic: providers are scripted in-process (no network), workspaces are
//! tempdir copies of the fixtures in this crate.

/// Fixture workspace staging (tempdir copies of this crate's fixtures dir).
pub mod fixtures;

/// Scorer helpers shared by the golden-task specs.
pub mod scoring;
