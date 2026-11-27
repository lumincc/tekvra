//! Hierarchical `AGENTS.md` / `AGENTS.override.md` instruction discovery and `SKILL.md` scanning.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::trust::canonicalize_or_clean;

/// Single discovered instruction file (`AGENTS.md` or `AGENTS.override.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionDoc {
    /// Canonical path to the instruction markdown file.
    pub path: PathBuf,
    /// Scope origin (`"global"` or `"project"`).
    pub scope: String,
    /// UTF-8 file contents.
    pub content: String,
}

/// Discovered `SKILL.md` package metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Skill identifier from frontmatter `name` (or parent folder name).
    pub name: String,
    /// Skill trigger description from frontmatter `description`.
    pub description: String,
    /// Path to `SKILL.md`.
    pub path: PathBuf,
    /// Origin (`"global"` or `"project"`).
    pub scope: String,
}

/// Locates the enclosing Git repository root containing `.git` (directory or worktree file),
/// or returns `None` if `cwd` is not inside a Git repository.
#[must_use]
pub fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    let mut cursor = Some(canonicalize_or_clean(cwd));
    while let Some(dir) = cursor {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        cursor = dir.parent().map(Path::to_path_buf);
    }
    None
}

/// Discovers `AGENTS.md` and `AGENTS.override.md` from `global_config_dir` first, then
/// in deterministic root-to-leaf order from the Git repository root down to `cwd`.
#[must_use]
pub fn discover_instructions(global_config_dir: &Path, cwd: &Path) -> Vec<InstructionDoc> {
    let mut docs = Vec::new();
    append_dir_instructions(global_config_dir, "global", &mut docs);

    let canonical_cwd = canonicalize_or_clean(cwd);
    let top = find_git_root(&canonical_cwd).unwrap_or_else(|| canonical_cwd.clone());

    let mut chain = Vec::new();
    let mut cur: Option<&Path> = Some(&canonical_cwd);
    while let Some(dir) = cur {
        chain.push(dir.to_path_buf());
        if dir == top {
            break;
        }
        cur = dir.parent();
    }
    chain.reverse();

    for dir in chain {
        append_dir_instructions(&dir, "project", &mut docs);
    }
    docs
}

fn append_dir_instructions(dir: &Path, scope: &str, out: &mut Vec<InstructionDoc>) {
    for filename in ["AGENTS.md", "AGENTS.override.md"] {
        let candidate = dir.join(filename);
        if candidate.is_file()
            && let Ok(raw) = fs::read_to_string(&candidate)
        {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                out.push(InstructionDoc {
                    path: candidate,
                    scope: scope.to_string(),
                    content: raw,
                });
            }
        }
    }
}

/// Renders merged `AGENTS.md` instructions in deterministic root-to-leaf order.
#[must_use]
pub fn merge_instructions_markdown(docs: &[InstructionDoc]) -> String {
    let mut sections = Vec::with_capacity(docs.len());
    for doc in docs {
        sections.push(format!(
            "<!-- source: {} ({}) -->\n{}",
            doc.path.display(),
            doc.scope,
            doc.content.trim()
        ));
    }
    sections.join("\n\n")
}

/// Discovers `SKILL.md` files from global (`<config>/skills`, `<home>/.agents/skills`,
/// `<home>/.claude/skills`) and project (`<project_root>/.agents/skills`, `<project_root>/.claude/skills`)
/// directories. Later entries with the same skill `name` override earlier ones cleanly.
#[must_use]
pub fn discover_skills(
    global_config_dir: &Path,
    home_dir: Option<&Path>,
    project_root: &Path,
) -> Vec<SkillMetadata> {
    let mut by_name: BTreeMap<String, SkillMetadata> = BTreeMap::new();

    let mut roots: Vec<(PathBuf, &str)> = vec![(global_config_dir.join("skills"), "global")];
    if let Some(home) = home_dir {
        roots.push((home.join(".agents").join("skills"), "global"));
        roots.push((home.join(".claude").join("skills"), "global"));
    }
    roots.push((project_root.join(".agents").join("skills"), "project"));
    roots.push((project_root.join(".claude").join("skills"), "project"));

    for (skills_root, scope) in roots {
        if !skills_root.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&skills_root) else {
            continue;
        };
        let mut subdirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|d| d.path()))
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();

        for subdir in subdirs {
            let skill_md = subdir.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&skill_md) else {
                continue;
            };
            let fallback_name = subdir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed-skill")
                .to_string();
            let (name, description) = parse_skill_frontmatter(&raw, &fallback_name);
            by_name.insert(
                name.clone(),
                SkillMetadata {
                    name,
                    description,
                    path: skill_md,
                    scope: scope.to_string(),
                },
            );
        }
    }

    by_name.into_values().collect()
}

fn parse_skill_frontmatter(raw: &str, fallback_name: &str) -> (String, String) {
    let mut name = fallback_name.to_string();
    let mut description = String::new();

    if let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        && let Some((yaml_block, _body)) = rest.split_once("\n---")
    {
        for line in yaml_block.lines() {
            let trimmed = line.trim();
            if let Some(v) = trimmed.strip_prefix("name:") {
                let cleaned = v.trim().trim_matches('"').trim_matches('\'');
                if !cleaned.is_empty() {
                    name = cleaned.to_string();
                }
            } else if let Some(v) = trimmed.strip_prefix("description:") {
                let cleaned = v.trim().trim_matches('"').trim_matches('\'');
                if !cleaned.is_empty() {
                    description = cleaned.to_string();
                }
            }
        }
    }
    (name, description)
}
