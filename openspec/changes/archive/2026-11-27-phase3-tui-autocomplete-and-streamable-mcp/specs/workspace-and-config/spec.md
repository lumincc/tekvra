# Spec Delta

## ADDED Requirements

### Requirement: Automated multi-platform CI quality gates and tagged release CD pipeline
The repository SHALL include automated CI workflows enforcing formatting (`cargo fmt`), lint cleanliness (`cargo clippy -D warnings`), workspace test execution, dependency/license policy (`cargo deny`), and Apache-2.0 attribution completeness on pushes and pull requests, alongside a tagged release CD workflow (`v*`) that validates version/tag parity and packages checksummed binaries across Linux, macOS, and Windows.

#### Scenario: Pull request and main branch CI gate verification
- **WHEN** changes are pushed to `main` or opened in a pull request
- **THEN** the CI workflow checks formatting, Clippy warnings across all targets, cross-platform workspace builds/tests, `cargo-deny` license/ban rules, and packaging attribution metadata before passing the required `CI` gate

#### Scenario: Tagged release packaging validation
- **WHEN** a release tag `vX.Y.Z` is validated by the release pipeline
- **THEN** packaging verification confirms workspace `Cargo.toml` version parity, Apache-2.0 `LICENSE`/`NOTICE.md`/`THIRD_PARTY_LICENSES/` completeness, and produces SHA-256 checksummed release archives per target platform
