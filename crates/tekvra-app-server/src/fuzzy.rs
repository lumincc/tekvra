//! Pure Apache-2.0 boundary-aware `fuzzyFileSearch` engine skipping `.git` and sensitive credentials
//! (avoiding MPL-2.0 dependencies so the entire dependency graph remains 100% permissive).

use std::path::Path;

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use tekvra_tools::is_sensitive_credential_path;

/// Single file match entry conforming to Codex `FuzzyFileSearchResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuzzyFileMatch {
    /// Base filename.
    pub file_name: String,
    /// Match kind (`"file"`).
    pub match_type: String,
    /// Relative or full path.
    pub path: String,
    /// Root directory searched.
    pub root: String,
    /// Relevance score (higher is better).
    pub score: u32,
    /// Matched character indices in `path`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indices: Vec<u32>,
}

/// Response payload conforming to Codex `FuzzyFileSearchResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuzzySearchOutput {
    /// Ranked matching files.
    pub files: Vec<FuzzyFileMatch>,
}

/// Executes a fast fuzzy file search across `roots`, skipping `.git` and `.env*`/credential files.
#[must_use]
pub fn fuzzy_file_search(query: &str, roots: &[String], max_results: usize) -> FuzzySearchOutput {
    let trimmed = query.trim();
    let case_sensitive = trimmed.chars().any(char::is_uppercase);
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();

    let mut matches = Vec::new();

    for root_str in roots {
        let root_path = Path::new(root_str);
        if !root_path.exists() {
            continue;
        }
        let walker = WalkBuilder::new(root_path)
            .hidden(false)
            .git_ignore(true)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build();

        for dir_entry in walker.flatten() {
            let path = dir_entry.path();
            if !path.is_file() || is_sensitive_credential_path(path) {
                continue;
            }
            let rel = path
                .strip_prefix(root_path)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&rel)
                .to_string();

            if tokens.is_empty() {
                matches.push(FuzzyFileMatch {
                    file_name,
                    match_type: "file".to_string(),
                    path: rel,
                    root: root_str.clone(),
                    score: 1,
                    indices: Vec::new(),
                });
            } else if let Some((score, indices)) =
                score_multi_token(&rel, &file_name, &tokens, case_sensitive)
            {
                matches.push(FuzzyFileMatch {
                    file_name,
                    match_type: "file".to_string(),
                    path: rel,
                    root: root_str.clone(),
                    score,
                    indices,
                });
            }

            if matches.len() >= 5_000 {
                break;
            }
        }
    }

    matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    matches.truncate(max_results.max(1));

    FuzzySearchOutput { files: matches }
}

fn score_multi_token(
    rel_path: &str,
    file_name: &str,
    tokens: &[&str],
    case_sensitive: bool,
) -> Option<(u32, Vec<u32>)> {
    let mut total_score = 0u32;
    let mut all_indices = Vec::new();

    for tok in tokens {
        let (sc, idxs) = score_single_token(rel_path, file_name, tok, case_sensitive)?;
        total_score = total_score.saturating_add(sc);
        all_indices.extend(idxs);
    }

    all_indices.sort_unstable();
    all_indices.dedup();
    Some((total_score, all_indices))
}

fn score_single_token(
    rel_path: &str,
    file_name: &str,
    needle: &str,
    case_sensitive: bool,
) -> Option<(u32, Vec<u32>)> {
    let hay_chars: Vec<char> = rel_path.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() {
        return Some((1, Vec::new()));
    }

    // Prefer contiguous match if present, otherwise greedy boundary-preferring subsequence
    let indices = find_contiguous(&hay_chars, &needle_chars, case_sensitive)
        .or_else(|| find_subsequence(&hay_chars, &needle_chars, case_sensitive))?;

    let mut score = 10u32;
    for (pos_in_match, &char_idx) in indices.iter().enumerate() {
        let idx = char_idx as usize;
        score += 10;
        if pos_in_match > 0 && indices[pos_in_match - 1] + 1 == char_idx {
            score += 24;
        }
        if idx == 0 || matches!(hay_chars[idx - 1], '/' | '_' | '-' | '.') {
            score += 20;
        }
    }
    if (case_sensitive && file_name == needle)
        || (!case_sensitive && file_name.eq_ignore_ascii_case(needle))
    {
        score += 100;
    }
    score = score.saturating_add(64u32.saturating_sub(hay_chars.len().min(64) as u32));

    Some((score, indices))
}

fn chars_eq(a: char, b: char, case_sensitive: bool) -> bool {
    if case_sensitive {
        a == b
    } else {
        a.eq_ignore_ascii_case(&b)
    }
}

fn find_contiguous(hay: &[char], needle: &[char], case_sensitive: bool) -> Option<Vec<u32>> {
    if needle.len() > hay.len() {
        return None;
    }
    // Prefer last occurrence so filename hits beat parent-directory hits!
    for start in (0..=(hay.len() - needle.len())).rev() {
        if hay[start..start + needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(&h, &n)| chars_eq(h, n, case_sensitive))
        {
            return Some((start..start + needle.len()).map(|i| i as u32).collect());
        }
    }
    None
}

fn find_subsequence(hay: &[char], needle: &[char], case_sensitive: bool) -> Option<Vec<u32>> {
    let mut indices = Vec::with_capacity(needle.len());
    let mut h_idx = 0usize;
    for &nc in needle {
        let mut found = None;
        while h_idx < hay.len() {
            if chars_eq(hay[h_idx], nc, case_sensitive) {
                found = Some(h_idx as u32);
                h_idx += 1;
                break;
            }
            h_idx += 1;
        }
        indices.push(found?);
    }
    Some(indices)
}
