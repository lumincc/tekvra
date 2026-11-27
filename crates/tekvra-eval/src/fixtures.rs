//! Fixture workspace staging: copies a golden-task fixture directory into a fresh
//! tempdir with a real `.git` (read tooling canonicalizes project roots).

use std::fs;
use std::path::{Path, PathBuf};

/// Copies `fixture_dir` into a fresh tempdir under `base`, re-creating `.git` so
/// project-root discovery behaves like a real checkout.
///
/// # Errors
/// Returns the I/O error when staging fails.
pub fn stage_fixture(fixture_dir: &Path, base: &Path) -> std::io::Result<PathBuf> {
    let workspace = base.join("repo");
    fs::create_dir_all(workspace.join(".git"))?;
    copy_tree(fixture_dir, &workspace)?;
    Ok(workspace)
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
