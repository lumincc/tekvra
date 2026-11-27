//! Git worktree lifecycle for worktree-isolated `task` subagents: creation on
//! task-scoped branches, diff summaries, conflict-safe merge-back, and discard.
//!
//! Everything shells out to `git` (`tokio::process`, mirroring the in-repo
//! `run_git` precedent); no `git2` dependency by workspace policy.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::CoreError;

/// A materialized task worktree: checkout path, branch, and the recorded base commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWorktree {
    /// Absolute worktree checkout path.
    pub path: PathBuf,
    /// Task-scoped branch (`tekvra/<task_id>`).
    pub branch: String,
    /// Parent HEAD recorded at creation (diff/merge baseline).
    pub base_commit: String,
}

/// Result of a successful merge-back into the parent working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Short SHA of the parent's new HEAD after the merge.
    pub merged_head: String,
    /// Branch-side changed paths that were merged.
    pub merged_paths: Vec<String>,
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<String, CoreError> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await
        .map_err(|e| CoreError::Subagent {
            message: format!(
                "failed to run `git {}`: {e}",
                args.first().copied().unwrap_or("")
            ),
        })?;
    if !output.status.success() {
        return Err(CoreError::Subagent {
            message: format!(
                "`git {}` failed: {}",
                args.first().copied().unwrap_or(""),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Git result where failure yields `None` (probes such as branch existence).
async fn probe_git(cwd: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Resolves the repository root containing `workspace_root`, or `None` when
/// the directory is not inside a Git repository.
pub async fn detect_repo_root(workspace_root: &Path) -> Option<PathBuf> {
    probe_git(workspace_root, &["rev-parse", "--show-toplevel"])
        .await
        .map(PathBuf::from)
}

fn repo_slug(repo_root: &Path) -> Result<String, CoreError> {
    let slug = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| {
            let mut slug = s.to_ascii_lowercase();
            slug.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            slug
        })
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Subagent {
            message: format!("cannot derive repository slug from {}", repo_root.display()),
        })?;
    Ok(slug)
}

/// Creates (or reuses) the deterministic task worktree
/// `<state_dir>/worktrees/<repo-slug>/<task_id>` on branch `tekvra/<task_id>`,
/// recording the parent HEAD as the diff/merge base.
///
/// # Errors
/// Returns [`CoreError::Subagent`] when `workspace_root` is not inside a Git
/// repository or any `git` invocation fails.
pub async fn ensure_task_worktree(
    workspace_root: &Path,
    state_dir: &Path,
    task_id: &str,
) -> Result<TaskWorktree, CoreError> {
    let repo_root = detect_repo_root(workspace_root)
        .await
        .ok_or_else(|| CoreError::Subagent {
            message: "`worktree: true` requires a Git repository; the workspace is not one"
                .to_string(),
        })?;
    let slug = repo_slug(&repo_root)?;
    let branch = format!("tekvra/{task_id}");
    let dir = state_dir.join("worktrees").join(&slug).join(task_id);

    let base_commit = if dir.join(".git").exists() {
        // Reuse: re-read the worktree's own HEAD as the baseline.
        probe_git(&dir, &["rev-parse", "HEAD"])
            .await
            .unwrap_or_default()
    } else {
        let head = run_git(&repo_root, &["rev-parse", "HEAD"]).await?;
        let branch_exists = probe_git(
            &repo_root,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .await
        .is_some();
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CoreError::Subagent {
                message: format!("creating {}: {source}", parent.display()),
            })?;
        }
        let mut add = Command::new("git");
        add.current_dir(&repo_root).arg("worktree").arg("add");
        if !branch_exists {
            add.arg("-b");
        }
        add.arg(&branch).arg(dir.display().to_string());
        let output = add.output().await.map_err(|e| CoreError::Subagent {
            message: format!("failed to run `git worktree add`: {e}"),
        })?;
        if !output.status.success() {
            return Err(CoreError::Subagent {
                message: format!(
                    "`git worktree add` failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        head
    };

    Ok(TaskWorktree {
        path: dir,
        branch,
        base_commit,
    })
}

/// Paths changed on the worktree branch relative to the recorded base commit,
/// including uncommitted edits and untracked files (forward-slash relative).
///
/// # Errors
/// Returns [`CoreError::Subagent`] on `git` failure.
pub async fn worktree_changes(worktree: &TaskWorktree) -> Result<Vec<String>, CoreError> {
    let dir = &worktree.path;
    let mut paths = run_git(
        dir,
        &[
            "diff",
            "--name-only",
            &format!("{}..HEAD", worktree.base_commit),
        ],
    )
    .await?;
    paths.push('\n');
    paths.push_str(&run_git(dir, &["diff", "--name-only", "HEAD"]).await?);
    paths.push('\n');
    paths.push_str(&run_git(dir, &["ls-files", "--others", "--exclude-standard"]).await?);
    let mut unique: Vec<String> = paths
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    unique.sort();
    unique.dedup();
    Ok(unique)
}

/// Human-facing changed-file summary (diffstat per segment plus path list),
/// bounded to keep tool results compact.
///
/// # Errors
/// Returns [`CoreError::Subagent`] on `git` failure.
pub async fn worktree_diff_summary(worktree: &TaskWorktree) -> Result<String, CoreError> {
    let dir = &worktree.path;
    let committed = probe_git(
        dir,
        &["diff", "--stat", &format!("{}..HEAD", worktree.base_commit)],
    )
    .await
    .unwrap_or_default();
    let uncommitted = probe_git(dir, &["diff", "--stat", "HEAD"])
        .await
        .unwrap_or_default();
    let paths = worktree_changes(worktree).await?;
    let mut out = String::new();
    let committed_empty = committed.trim().is_empty();
    let uncommitted_empty = uncommitted.trim().is_empty();
    if committed_empty && uncommitted_empty {
        out.push_str("no file changes on branch\n");
    } else {
        if !committed_empty {
            out.push_str("committed changes:\n");
            out.push_str(committed.trim());
            out.push('\n');
        }
        if !uncommitted_empty {
            out.push_str("uncommitted changes:\n");
            out.push_str(uncommitted.trim());
            out.push('\n');
        }
    }
    if !paths.is_empty() {
        out.push_str("changed paths:\n");
        for p in paths.iter().take(40) {
            out.push_str("  ");
            out.push_str(p);
            out.push('\n');
        }
        if paths.len() > 40 {
            out.push_str(&format!("  … and {} more\n", paths.len() - 40));
        }
    }
    Ok(out)
}

/// Stages and commits all worktree changes as a checkpoint commit; returns the
/// created commit subject, or `None` when the worktree is already clean.
///
/// # Errors
/// Returns [`CoreError::Subagent`] on `git` failure.
pub async fn commit_worktree_changes(
    worktree: &TaskWorktree,
    task_id: &str,
) -> Result<Option<String>, CoreError> {
    let dir = &worktree.path;
    let dirty = run_git(dir, &["status", "--porcelain"]).await?;
    if dirty.trim().is_empty() {
        return Ok(None);
    }
    run_git(dir, &["add", "-A"]).await?;
    run_git(
        dir,
        &[
            "commit",
            "-m",
            &format!("tekvra(task): checkpoint {task_id}"),
        ],
    )
    .await?;
    Ok(Some(format!("tekvra(task): checkpoint {task_id}")))
}

/// Merges the task branch back into the parent's current branch, refusing when
/// the merge would touch files that are dirty in the parent tree, and aborting
/// cleanly (parent untouched, worktree + branch kept) on conflicts.
///
/// # Errors
/// Returns [`CoreError::Subagent`] describing dirty overlaps or conflicting
/// paths; on conflict the parent tree is left exactly as before.
pub async fn merge_task_worktree(
    workspace_root: &Path,
    worktree: &TaskWorktree,
    task_id: &str,
) -> Result<MergeOutcome, CoreError> {
    commit_worktree_changes(worktree, task_id).await?;

    let dirty: Vec<String> = run_git(workspace_root, &["diff", "--name-only", "HEAD"])
        .await?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    let branch_changed: Vec<String> = run_git(
        workspace_root,
        &[
            "diff",
            "--name-only",
            &format!("HEAD...{}", worktree.branch),
        ],
    )
    .await?
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(str::to_string)
    .collect();
    let overlap: Vec<String> = dirty
        .iter()
        .filter(|d| branch_changed.iter().any(|c| c == *d))
        .cloned()
        .collect();
    if !overlap.is_empty() {
        return Err(CoreError::Subagent {
            message: format!(
                "merge-back refused: parent tree has uncommitted edits to {} — commit or stash them first",
                overlap.join(", ")
            ),
        });
    }

    let merge_args = [
        "merge",
        "--no-ff",
        "-m",
        &format!("Merge branch {0} (task {task_id})", worktree.branch),
        &worktree.branch,
    ];
    let merge_probe = probe_git(workspace_root, &merge_args).await;
    if merge_probe.is_none() {
        let conflicted: Vec<String> = run_git(workspace_root, &["ls-files", "--unmerged"])
            .await?
            .lines()
            .filter_map(|l| l.split_whitespace().nth(3).map(str::to_string))
            .collect();
        let abort = Command::new("git")
            .current_dir(workspace_root)
            .args(["merge", "--abort"])
            .output()
            .await;
        if let Ok(out) = &abort
            && !out.status.success()
        {
            eprintln!(
                "tekvra: `git merge --abort` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let mut unique = conflicted;
        unique.sort();
        unique.dedup();
        return Err(CoreError::Subagent {
            message: format!(
                "merge-back conflict on {} — worktree `{}` and branch `{}` kept for manual resolution",
                unique.join(", "),
                worktree.path.display(),
                worktree.branch
            ),
        });
    }

    // Merge succeeded: the checkout is fully committed, so cleanup is safe.
    // Strict removal (force clears any residue); a failure here would leave the
    // branch checked out and undeletable, so surface it instead of ignoring.
    let merged_head = probe_git(workspace_root, &["rev-parse", "--short", "HEAD"])
        .await
        .unwrap_or_default();
    run_git(
        workspace_root,
        &[
            "worktree",
            "remove",
            "--force",
            &worktree.path.display().to_string(),
        ],
    )
    .await?;
    let del = Command::new("git")
        .current_dir(workspace_root)
        .args(["branch", "-D", &worktree.branch])
        .output()
        .await;
    if let Ok(out) = &del
        && !out.status.success()
    {
        eprintln!(
            "tekvra: `git branch -D {}` failed: {}",
            worktree.branch,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(MergeOutcome {
        merged_head,
        merged_paths: branch_changed,
    })
}

/// Removes the worktree checkout and force-deletes its branch (task dropped).
///
/// # Errors
/// Returns [`CoreError::Subagent`] when the worktree removal fails outright.
pub async fn discard_task_worktree(
    workspace_root: &Path,
    worktree: &TaskWorktree,
) -> Result<(), CoreError> {
    run_git(
        workspace_root,
        &[
            "worktree",
            "remove",
            "--force",
            &worktree.path.display().to_string(),
        ],
    )
    .await?;
    // Branch may not exist for a reused-but-never-created branch; best effort.
    let _ = probe_git(workspace_root, &["branch", "-D", &worktree.branch]).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Creates a temp git repo with one committed file and a distinct user identity.
    fn seed_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@tekvra.local"]);
        git(&root, &["config", "user.name", "Tekvra Test"]);
        std::fs::write(root.join("lib.rs"), "fn a() {}\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        (tmp, root)
    }

    fn commit_file(cwd: &Path, rel: &str, contents: &str, message: &str) {
        let p = cwd.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, contents).unwrap();
        git(cwd, &["add", "-A"]);
        git(cwd, &["commit", "-q", "-m", message]);
    }

    #[tokio::test]
    async fn rejects_worktree_outside_git_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let err = ensure_task_worktree(tmp.path(), tmp.path(), "task_x")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Git repository"), "{err}");
    }

    #[tokio::test]
    async fn creates_reuses_and_diffs_task_worktree() {
        let (_guard, root) = seed_repo();
        let state = tempfile::tempdir().unwrap();
        let wt = ensure_task_worktree(&root, state.path(), "task_abc")
            .await
            .unwrap();
        assert_eq!(wt.branch, "tekvra/task_abc");
        assert!(wt.path.join(".git").exists());

        // Reuse is stable and does not recreate.
        let again = ensure_task_worktree(&root, state.path(), "task_abc")
            .await
            .unwrap();
        assert_eq!(again, wt);

        // No changes yet.
        assert!(worktree_changes(&wt).await.unwrap().is_empty());

        // Edit an existing file (uncommitted) and add a new module file.
        std::fs::write(wt.path.join("lib.rs"), "fn b() {}\n").unwrap();
        std::fs::write(wt.path.join("new_mod.rs"), "pub fn c() {}\n").unwrap();
        // Commit ONLY the new module so `lib.rs` stays uncommitted.
        git(&wt.path, &["add", "new_mod.rs"]);
        let out = StdCommand::new("git")
            .current_dir(&wt.path)
            .args(["commit", "-q", "-m", "add module"])
            .output()
            .unwrap();
        assert!(out.status.success());

        let changes = worktree_changes(&wt).await.unwrap();
        assert_eq!(changes, vec!["lib.rs", "new_mod.rs"]);
        let summary = worktree_diff_summary(&wt).await.unwrap();
        assert!(summary.contains("committed changes:"), "{summary}");
        assert!(summary.contains("uncommitted changes:"), "{summary}");
        assert!(summary.contains("new_mod.rs"), "{summary}");
    }

    #[tokio::test]
    async fn merges_worktree_back_and_cleans_up() {
        let (_guard, root) = seed_repo();
        let state = tempfile::tempdir().unwrap();
        let wt = ensure_task_worktree(&root, state.path(), "task_m1")
            .await
            .unwrap();

        commit_file(&wt.path, "feature.rs", "pub fn f() {}\n", "feat");
        std::fs::write(wt.path.join("lib.rs"), "fn c() {}\n").unwrap();
        let parent_head_before = git(&root, &["rev-parse", "HEAD"]);

        let outcome = merge_task_worktree(&root, &wt, "task_m1").await.unwrap();
        assert!(!outcome.merged_head.is_empty());
        assert!(outcome.merged_paths.contains(&"feature.rs".to_string()));
        assert!(root.join("feature.rs").exists(), "merged file must land");
        assert_ne!(
            git(&root, &["rev-parse", "HEAD"]),
            parent_head_before,
            "merge must advance parent HEAD"
        );
        assert!(!wt.path.exists(), "worktree removed after merge");
        assert!(
            probe_git(
                &root,
                &["rev-parse", "--verify", "refs/heads/tekvra/task_m1"]
            )
            .await
            .is_none()
        );
        // Merge commit exists (--no-ff).
        let subject = git(&root, &["log", "-1", "--format=%s"]);
        assert!(
            subject.starts_with("Merge branch tekvra/task_m1"),
            "{subject}"
        );
    }

    #[tokio::test]
    async fn conflicting_merge_aborts_and_keeps_trees_intact() {
        let (_guard, root) = seed_repo();
        let state = tempfile::tempdir().unwrap();
        let wt = ensure_task_worktree(&root, state.path(), "task_c1")
            .await
            .unwrap();

        // Same line edited on both sides.
        std::fs::write(wt.path.join("lib.rs"), "fn conflict() {}\n").unwrap();
        git(&wt.path, &["add", "-A"]);
        git(&wt.path, &["commit", "-q", "-m", "wt edit lib"]);
        std::fs::write(root.join("lib.rs"), "fn parent_edit() {}\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "parent edit lib"]);

        let parent_head = git(&root, &["rev-parse", "HEAD"]);
        let err = merge_task_worktree(&root, &wt, "task_c1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("conflict"), "{err}");
        assert!(err.to_string().contains("lib.rs"), "{err}");
        // Parent tree untouched.
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), parent_head);
        let status = git(&root, &["status", "--porcelain"]);
        assert!(status.trim().is_empty(), "parent must be clean: {status}");
        // Worktree + branch kept.
        assert!(wt.path.exists());
        assert!(
            probe_git(
                &root,
                &["rev-parse", "--verify", "refs/heads/tekvra/task_c1"]
            )
            .await
            .is_some()
        );

        discard_task_worktree(&root, &wt).await.unwrap();
        assert!(!wt.path.exists());
        assert!(
            probe_git(
                &root,
                &["rev-parse", "--verify", "refs/heads/tekvra/task_c1"]
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn merge_refused_when_parent_dirty_on_same_paths() {
        let (_guard, root) = seed_repo();
        let state = tempfile::tempdir().unwrap();
        let wt = ensure_task_worktree(&root, state.path(), "task_d1")
            .await
            .unwrap();
        commit_file(&wt.path, "lib.rs", "fn wt_edit() {}\n", "wt edit");

        // Dirty the same file in the parent without committing.
        std::fs::write(root.join("lib.rs"), "fn dirty() {}\n").unwrap();
        let err = merge_task_worktree(&root, &wt, "task_d1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("uncommitted edits"), "{err}");
        assert!(wt.path.exists(), "worktree kept on refusal");
    }
}
