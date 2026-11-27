#!/usr/bin/env python3
"""Deterministic CI/CD workflow, workspace manifest, Apache-2.0 attribution, and release packager."""

from __future__ import annotations

import argparse
import hashlib
import re
import shutil
import sys
import tarfile
import tomllib
import zipfile
from pathlib import Path

REQUIRED_TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)

REQUIRED_CI_JOBS = ("fmt:", "lint:", "test:", "windows:", "deny:", "ci-gate:")
REQUIRED_RELEASE_JOBS = ("validate:", "verify-ci:", "build:", "publish:")


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def fail(msg: str) -> None:
    print(f"error: {msg}", file=sys.stderr)
    raise SystemExit(1)


def verify_workspace_and_attribution(root: Path) -> str:
    cargo_toml_path = root / "Cargo.toml"
    if not cargo_toml_path.is_file():
        fail("missing root Cargo.toml")

    with cargo_toml_path.open("rb") as f:
        manifest = tomllib.load(f)

    ws = manifest.get("workspace", {})
    pkg = ws.get("package", {})
    version = pkg.get("version")
    if not isinstance(version, str) or not re.fullmatch(r"\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?", version):
        fail(f"invalid [workspace.package].version: {version!r}")

    if pkg.get("license") != "Apache-2.0":
        fail(f"[workspace.package].license must be 'Apache-2.0', got {pkg.get('license')!r}")

    rust_lints = ws.get("lints", {}).get("rust", {})
    if rust_lints.get("unsafe_code") != "forbid":
        fail("[workspace.lints.rust].unsafe_code must be 'forbid'")

    members = ws.get("members", [])
    if not members:
        fail("[workspace].members must not be empty")

    for member in members:
        member_toml = root / member / "Cargo.toml"
        if not member_toml.is_file():
            fail(f"missing member manifest: {member_toml}")
        with member_toml.open("rb") as mf:
            mdata = tomllib.load(mf)
        mpkg = mdata.get("package", {})
        lic = mpkg.get("license")
        if lic != {"workspace": True} and lic != "Apache-2.0":
            fail(f"crate {member} must inherit license.workspace = true or declare Apache-2.0")
        mlints = mdata.get("lints", {})
        if mlints.get("workspace") is not True:
            fail(f"crate {member} must inherit [lints] workspace = true")

    if not (root / "Cargo.lock").is_file():
        fail("missing Cargo.lock (required for --locked CI/CD builds)")

    license_file = root / "LICENSE"
    if not license_file.is_file() or "Apache License" not in license_file.read_text(encoding="utf-8"):
        fail("root LICENSE must exist and contain the Apache License 2.0 text")

    notice_file = root / "NOTICE.md"
    if not notice_file.is_file() or len(notice_file.read_text(encoding="utf-8").strip()) < 40:
        fail("root NOTICE.md must exist and document third-party provenance")

    third_party_dir = root / "THIRD_PARTY_LICENSES"
    if not third_party_dir.is_dir() or not list(third_party_dir.glob("*.txt")):
        fail("THIRD_PARTY_LICENSES/ must contain upstream attribution license texts")

    return version


def verify_workflows(root: Path) -> None:
    ci_yml = root / ".github" / "workflows" / "ci.yml"
    release_yml = root / ".github" / "workflows" / "release.yml"
    dependabot_yml = root / ".github" / "dependabot.yml"

    for wf in (ci_yml, release_yml, dependabot_yml):
        if not wf.is_file():
            fail(f"missing GitHub configuration file: {wf.relative_to(root)}")
        raw = wf.read_text(encoding="utf-8")
        if "\t" in raw:
            fail(f"YAML file contains tab characters: {wf.relative_to(root)}")

    ci_text = ci_yml.read_text(encoding="utf-8")
    for job in REQUIRED_CI_JOBS:
        if f"\n  {job}" not in ci_text:
            fail(f"ci.yml missing required job `{job[:-1]}`")
    if "workflow_call:" not in ci_text:
        fail("ci.yml must expose `workflow_call:` for release.yml reuse")
    if "cargo clippy --locked --workspace --all-targets -- -D warnings" not in ci_text:
        fail("ci.yml must enforce `cargo clippy --locked --workspace --all-targets -- -D warnings`")
    if "cargo deny --locked check" not in ci_text:
        fail("ci.yml must enforce `cargo deny --locked check`")

    rel_text = release_yml.read_text(encoding="utf-8")
    for job in REQUIRED_RELEASE_JOBS:
        if f"\n  {job}" not in rel_text:
            fail(f"release.yml missing required job `{job[:-1]}`")
    for target in REQUIRED_TARGETS:
        if target not in rel_text:
            fail(f"release.yml matrix missing target `{target}`")
    if "uses: ./.github/workflows/ci.yml" not in rel_text:
        fail("release.yml must gate releases on `./.github/workflows/ci.yml`")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def package_release(
    root: Path,
    version_tag: str,
    target: str,
    binary_path: Path,
    out_dir: Path,
) -> Path:
    if not version_tag.startswith("v"):
        fail(f"--version must start with 'v' (got {version_tag!r})")
    if not binary_path.is_file():
        fail(f"binary does not exist: {binary_path}")

    out_dir.mkdir(parents=True, exist_ok=True)
    stem = f"tekvra-{version_tag}-{target}"
    staging_root = out_dir / f".staging-{stem}"
    if staging_root.exists():
        shutil.rmtree(staging_root)
    bundle_dir = staging_root / stem
    bundle_dir.mkdir(parents=True, exist_ok=True)

    is_windows = "windows" in target
    bin_name = "tekvra.exe" if is_windows else "tekvra"
    dst_bin = bundle_dir / bin_name
    shutil.copy2(binary_path, dst_bin)
    if not is_windows:
        dst_bin.chmod(0o755)

    for doc_name in ("README.md", "LICENSE", "NOTICE.md"):
        shutil.copy2(root / doc_name, bundle_dir / doc_name)

    shutil.copytree(root / "THIRD_PARTY_LICENSES", bundle_dir / "THIRD_PARTY_LICENSES")
    if (root / "docs").is_dir():
        shutil.copytree(root / "docs", bundle_dir / "docs")

    if is_windows:
        archive_path = out_dir / f"{stem}.zip"
        with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
            for item in sorted(bundle_dir.rglob("*")):
                if item.is_file():
                    zf.write(item, item.relative_to(staging_root).as_posix())
    else:
        archive_path = out_dir / f"{stem}.tar.gz"
        with tarfile.open(archive_path, "w:gz") as tf:
            tf.add(bundle_dir, arcname=stem)

    shutil.rmtree(staging_root)

    digest = sha256_file(archive_path)
    checksum_path = out_dir / f"{archive_path.name}.sha256"
    checksum_path.write_text(f"{digest}  {archive_path.name}\n", encoding="utf-8")
    print(f"packaged {archive_path} (sha256={digest})")
    return archive_path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check-workflows",
        action="store_true",
        help="Validate workspace manifests, Apache-2.0 attribution, and GitHub Actions workflows",
    )
    parser.add_argument(
        "--verify-tag",
        metavar="TAG",
        help="Verify TAG (e.g. v0.1.0) matches [workspace.package].version in Cargo.toml",
    )
    parser.add_argument(
        "--package-release",
        action="store_true",
        help="Bundle a target release archive (.tar.gz / .zip) + .sha256 checksum",
    )
    parser.add_argument("--version", help="Release tag (e.g. v0.1.0) for --package-release")
    parser.add_argument("--target", help="Rust target triple for --package-release")
    parser.add_argument("--binary", type=Path, help="Compiled binary path for --package-release")
    parser.add_argument("--out-dir", type=Path, default=Path("dist"), help="Output directory")

    args = parser.parse_args()
    root = repo_root()

    if args.check_workflows:
        ver = verify_workspace_and_attribution(root)
        verify_workflows(root)
        print(f"ok: workspace v{ver}, Apache-2.0 attribution, and CI/CD workflows verified")

    if args.verify_tag:
        ver = verify_workspace_and_attribution(root)
        verify_workflows(root)
        expected = f"v{ver}"
        if args.verify_tag != expected:
            fail(f"tag `{args.verify_tag}` does not match Cargo.toml version `{expected}`")
        print(f"ok: tag `{args.verify_tag}` matches workspace version `{ver}`")

    if args.package_release:
        if not (args.version and args.target and args.binary):
            fail("--package-release requires --version, --target, and --binary")
        verify_workspace_and_attribution(root)
        package_release(root, args.version, args.target, args.binary, args.out_dir)

    if not (args.check_workflows or args.verify_tag or args.package_release):
        parser.print_help()
        raise SystemExit(2)


if __name__ == "__main__":
    main()
