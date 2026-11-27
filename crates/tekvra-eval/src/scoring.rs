//! Golden-task scoring: ordered `(tool, arg-substring)` presence against a recorded
//! rollout plus final file-state assertions.

/// One expected tool invocation: tool name + a substring that must appear in the
/// serialized arguments JSON of THAT invocation, in order.
#[derive(Debug, Clone)]
pub struct ExpectedToolCall {
    /// Roster tool name (`write_file`, `grep_files`, ...).
    pub tool: &'static str,
    /// Substring required in the arguments JSON (e.g. `"src/lib.rs"`).
    pub args_contain: &'static str,
}

/// Scores an ordered list of `(tool, args_json)` observations.
///
/// Extra calls are tolerated (models may re-read files); each expectation must be
/// satisfied IN ORDER by a distinct observation. Returns human-readable failure
/// strings; empty = pass.
#[must_use]
pub fn score_tool_sequence(
    observed: &[(String, String)],
    expected: &[ExpectedToolCall],
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut cursor = 0usize;
    for exp in expected {
        let mut matched = false;
        while cursor < observed.len() {
            let (tool, args) = &observed[cursor];
            cursor += 1;
            if tool == exp.tool && args.contains(exp.args_contain) {
                matched = true;
                break;
            }
        }
        if !matched {
            failures.push(format!(
                "expected in-order call `{}` with args containing {:?} not satisfied \
                 (observed {} calls)",
                exp.tool,
                exp.args_contain,
                observed.len()
            ));
        }
    }
    failures
}

/// Asserts the final content of a workspace file after the task ran.
///
/// # Panics
/// Panics with a diff-ish message when the file content diverges.
pub fn assert_final_file(path: &Path, expected_contains: &str) {
    let actual = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("final-state read failed for {}: {e}", path.display());
    });
    assert!(
        actual.contains(expected_contains),
        "final state of {} missing {expected_contains:?}; got:\n{actual}",
        path.display()
    );
}

use std::path::Path;
