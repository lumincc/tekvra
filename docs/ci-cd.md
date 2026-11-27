# CI/CD & Release Pipeline

Tekvra ships two coordinated GitHub Actions workflows plus a zero-dependency packaging verifier script (`scripts/ci_verify_packaging.py`) and weekly Dependabot configuration.

## 1. Continuous Integration (`.github/workflows/ci.yml`)

Triggered on every push to `main` / `release/**`, every pull request, and via `workflow_call` from `release.yml`:

| Job | Runner | Responsibility |
| --- | --- | --- |
| `fmt` | `ubuntu-24.04` | `cargo fmt --all -- --check` + `python3 scripts/ci_verify_packaging.py --check-workflows` |
| `lint` | `ubuntu-24.04` | `cargo clippy --locked --workspace --all-targets -- -D warnings` |
| `test` | `ubuntu-24.04`, `macos-latest` | `cargo test --locked --workspace` across all 11 crates |
| `windows` | `windows-latest` | Windows Clippy check + `cargo build --locked --bin tekvra` |
| `deny` | `ubuntu-24.04` | `cargo deny --locked check advisories licenses bans sources` |
| `ci-gate` | `ubuntu-24.04` | Single required branch-protection check (`CI`) aggregating all upstream jobs |

## 2. Tagged Multi-Target Release CD (`.github/workflows/release.yml`)

Triggered on `v*` tags (e.g., `git tag v0.1.0 && git push origin v0.1.0`) or manually via `workflow_dispatch`:

1. **`validate`**: Runs `python3 scripts/ci_verify_packaging.py --verify-tag "$TAG"` to enforce exact parity between the Git tag (`vX.Y.Z`) and `[workspace.package].version` in `Cargo.toml`, `#![forbid(unsafe_code)]`, and `Apache-2.0` attribution completeness (`LICENSE`, `NOTICE.md`, `THIRD_PARTY_LICENSES/`).
2. **`verify-ci`**: Reuses `./.github/workflows/ci.yml` on the exact release commit before compiling release artifacts.
3. **`build`**: Compiles `--locked --profile release-lto` binaries across 5 targets, runs CLI smoke checks, bundles license/attribution files, and emits `.sha256` digests:
   - `x86_64-unknown-linux-gnu` (`.tar.gz`)
   - `aarch64-unknown-linux-gnu` (`.tar.gz`)
   - `x86_64-apple-darwin` (`.tar.gz`)
   - `aarch64-apple-darwin` (`.tar.gz`)
   - `x86_64-pc-windows-msvc` (`.zip`)
4. **`publish`**: Verifies `SHA256SUMS.txt` and publishes archives, checksums, and generated release notes to GitHub Releases with scoped `contents: write` permission.

## 3. Running Packaging & Workflow Checks Locally

```bash
# Audit Cargo manifests, Apache-2.0 attribution, and GitHub Actions workflows
python3 scripts/ci_verify_packaging.py --check-workflows

# Verify tag parity against Cargo.toml
python3 scripts/ci_verify_packaging.py --verify-tag v0.1.0

# Dry-run local release archive packaging + SHA-256 generation
cargo build --locked --release --bin tekvra
python3 scripts/ci_verify_packaging.py \
  --package-release \
  --version v0.1.0 \
  --target x86_64-unknown-linux-gnu \
  --binary target/release/tekvra \
  --out-dir target/dist-smoke
```
