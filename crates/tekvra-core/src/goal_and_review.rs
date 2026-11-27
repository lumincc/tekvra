//! Tool-less `/goal <condition>` transcript-evidence evaluator and read-only `/review` workflow.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tekvra_config::AgentSettings;
use tekvra_protocol::{PermissionMode, ReviewFinding, TokenUsage};
use tekvra_providers::{
    CompletionRequest, JobRole, MessagePart, MessageRole, ModelRouter, ProviderDriver,
    ProviderMessage,
};
use tekvra_tools::{
    MontyCodeSandbox, NativeToolRegistry, PermissionEvaluator, PtySessionManager, SafeWebFetcher,
    ToolExecutionContext, WorkspacePathLocks, build_tool_definitions_for_model,
    is_sensitive_credential_path,
};
use tokio::process::Command;

use crate::CoreError;
use crate::frame::{extract_assistant_text, render_messages_for_summary};

/// Verdict returned by the tool-less `/goal` evaluator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalVerdict {
    /// Whether concrete transcript evidence satisfies the `/goal` condition.
    pub met: bool,
    /// Actionable feedback injected into the next work turn when `met == false`.
    pub feedback: String,
    /// Token usage consumed by the evaluator pass.
    #[serde(skip)]
    pub usage: TokenUsage,
}

#[derive(Debug, Deserialize)]
struct RawGoalVerdict {
    met: bool,
    #[serde(default)]
    missing_evidence: Option<String>,
    #[serde(default)]
    feedback: Option<String>,
}

/// Evaluates `goal_condition` strictly without tools (`tools: vec![]`) using `JobRole::Goal`
/// against the accumulated session transcript messages.
///
/// # Errors
/// Returns [`CoreError`] if model routing or provider invocation fails.
pub async fn evaluate_goal(
    conversation_id: &str,
    goal_condition: &str,
    transcript_messages: &[ProviderMessage],
    router: &ModelRouter,
    driver: &dyn ProviderDriver,
) -> Result<GoalVerdict, CoreError> {
    let (provider, model) = router.route(JobRole::Goal)?;
    let transcript_evidence = render_messages_for_summary(transcript_messages);

    let system_prompt = concat!(
        "You are Tekvra's strict tool-less `/goal` completion evaluator.\n",
        "Inspect ONLY the provided conversation and tool-output transcript.\n",
        "CRITICAL RULE: Never accept unverified assistant claims. If the goal requires tests, builds, or checks to pass, ",
        "there MUST be a corresponding `[ToolResult ... error=false]` in the transcript proving execution succeeded.\n",
        "Respond with ONLY a JSON object: {\"met\": boolean, \"feedback\": \"concise actionable explanation of what concrete evidence or work is still missing\"}."
    );

    let user_prompt = format!(
        "Goal condition:\n{goal_condition}\n\nSession transcript evidence:\n{transcript_evidence}"
    );

    let req = CompletionRequest {
        conversation_id: format!("{conversation_id}:goal"),
        messages: vec![
            ProviderMessage::developer_cached(system_prompt),
            ProviderMessage::user_text(user_prompt),
        ],
        tools: Vec::new(), // Strictly tool-less!
        reasoning_effort: None,
        output_schema: None,
    };

    let resp = driver.complete(provider, &model, &req).await?;
    let raw_text = extract_assistant_text(&resp.assistant_message);
    let parsed = extract_json_from_text::<RawGoalVerdict>(&raw_text).unwrap_or(RawGoalVerdict {
        met: false,
        missing_evidence: None,
        feedback: Some(format!(
            "Goal evaluator returned non-JSON verdict ({}); verify goal `{goal_condition}` with concrete tool output.",
            raw_text.trim()
        )),
    });

    let feedback = parsed
        .feedback
        .or(parsed.missing_evidence)
        .unwrap_or_else(|| {
            if parsed.met {
                "Goal satisfied by transcript evidence.".to_string()
            } else {
                format!(
                    "Goal `{goal_condition}` is not yet backed by concrete transcript evidence."
                )
            }
        });

    Ok(GoalVerdict {
        met: parsed.met,
        feedback,
        usage: resp.usage,
    })
}

/// Target diff selection for `/review` (`tekvra review` and `review/start`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewTarget {
    /// Review staged, unstaged, and untracked working-tree changes (`git diff HEAD`).
    UncommittedChanges,
    /// Review changes on the current branch relative to `branch` (`git diff <branch>...HEAD`).
    BaseBranch {
        /// Base branch ref (e.g., `"main"`).
        branch: String,
    },
    /// Review a single commit (`git show <sha>`).
    Commit {
        /// Commit SHA or ref.
        sha: String,
    },
    /// Review with custom instructions and optional diff text.
    Custom {
        /// Review instructions.
        instructions: String,
    },
}

/// Completed read-only `/review` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewOutcome {
    /// Summary prose returned by the reviewer.
    pub summary: String,
    /// Structured file/line/severity findings.
    pub findings: Vec<ReviewFinding>,
    /// Cumulative token usage across the review pass.
    pub usage: TokenUsage,
}

#[derive(Debug, Deserialize)]
struct RawReviewPayload {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    findings: Vec<RawFinding>,
}

#[derive(Debug, Deserialize)]
struct RawFinding {
    #[serde(default = "default_severity_str")]
    severity: String,
    path: String,
    #[serde(default = "default_line_one")]
    start_line: usize,
    #[serde(default)]
    end_line: Option<usize>,
    title: String,
    body: String,
}

fn default_severity_str() -> String {
    "medium".to_string()
}

const fn default_line_one() -> usize {
    1
}

/// Collects the Git patch/diff text for `target` inside `workspace_root`.
///
/// # Errors
/// Returns [`CoreError::Review`] if `git` fails to execute.
pub async fn collect_review_diff(
    workspace_root: &Path,
    target: &ReviewTarget,
) -> Result<String, CoreError> {
    match target {
        ReviewTarget::UncommittedChanges => {
            let head_diff = run_git(workspace_root, &["diff", "HEAD"]).await;
            let mut patch = match head_diff {
                Ok(d) => d,
                Err(_) => run_git(workspace_root, &["diff"]).await?,
            };
            if let Ok(untracked) = run_git(
                workspace_root,
                &["ls-files", "--others", "--exclude-standard"],
            )
            .await
            {
                for rel in untracked.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    let rel_path = Path::new(rel);
                    if is_sensitive_credential_path(rel_path) {
                        continue;
                    }
                    let abs = workspace_root.join(rel_path);
                    if let Ok(content) = std::fs::read_to_string(&abs) {
                        patch.push_str(&format!("\n--- /dev/null\n+++ b/{rel}\n"));
                        for line in content.lines().take(500) {
                            patch.push('+');
                            patch.push_str(line);
                            patch.push('\n');
                        }
                    }
                }
            }
            Ok(patch)
        }
        ReviewTarget::BaseBranch { branch } => {
            let range = format!("{branch}...HEAD");
            match run_git(workspace_root, &["diff", &range]).await {
                Ok(d) if !d.trim().is_empty() => Ok(d),
                _ => run_git(workspace_root, &["diff", branch]).await,
            }
        }
        ReviewTarget::Commit { sha } => {
            run_git(workspace_root, &["show", "--format=medium", "--patch", sha]).await
        }
        ReviewTarget::Custom { instructions } => Ok(instructions.clone()),
    }
}

/// Executes a dedicated `PermissionMode::ReadOnly` code review pass (`JobRole::Review`) over `target`,
/// allowing read-only context inspection tools (`file_read`, `file_grep`, `file_glob`, `file_index`)
/// while blocking all writes/mutations, and extracts structured [`ReviewFinding`] items.
///
/// # Errors
/// Returns [`CoreError`] if Git diff collection or provider calls fail.
pub async fn run_code_review(
    conversation_id: &str,
    workspace_root: &Path,
    target: &ReviewTarget,
    router: &ModelRouter,
    driver: &dyn ProviderDriver,
    settings: &AgentSettings,
) -> Result<ReviewOutcome, CoreError> {
    let diff_text = collect_review_diff(workspace_root, target).await?;
    let (provider, model) = router.route(JobRole::Review)?;

    let canonical_ws =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let evaluator = PermissionEvaluator::new(
        PermissionMode::ReadOnly,
        canonical_ws.clone(),
        Some(canonical_ws.clone()),
        Vec::new(),
        Vec::new(),
    );
    let ctx = ToolExecutionContext {
        session_id: format!("{conversation_id}:review"),
        turn_id: "review_turn".to_string(),
        workspace_root: canonical_ws,
        settings: settings.clone(),
        path_locks: WorkspacePathLocks::new(),
        pty_manager: PtySessionManager::new(None),
        web_fetcher: SafeWebFetcher::new(&settings.webfetch_allow_private_hosts),
        code_sandbox: MontyCodeSandbox,
        permissions: Arc::new(Mutex::new(evaluator)),
        state_index: None,
        snapshots: None,
        mcp_gateway: None,
        approval_cb: None,
        question_cb: None,
        lsp_manager: None,
        plan_markdown: Arc::new(Mutex::new(String::new())),
    };

    // Expose only read-only workspace inspection tools during `/review`
    let read_only_tools: Vec<_> = build_tool_definitions_for_model(&model)
        .into_iter()
        .filter(|t| {
            matches!(
                t.name.as_str(),
                "file_read" | "file_grep" | "file_glob" | "file_index"
            )
        })
        .collect();

    let dev_prompt = concat!(
        "You are Tekvra's dedicated read-only code reviewer.\n",
        "Inspect the diff carefully (optionally calling `file_read`, `file_grep`, `file_glob`, or `file_index` for surrounding context).\n",
        "Return ONLY valid JSON matching:\n",
        "{\n",
        "  \"summary\": \"concise overall assessment\",\n",
        "  \"findings\": [\n",
        "    {\"severity\": \"critical|high|medium|low\", \"path\": \"relative/file.rs\", \"start_line\": 10, \"end_line\": 14, \"title\": \"Short issue title\", \"body\": \"Concrete explanation and fix\"}\n",
        "  ]\n",
        "}"
    );

    let mut messages = vec![
        ProviderMessage::developer_cached(dev_prompt),
        ProviderMessage::user_text(format!(
            "Review the following diff:\n```diff\n{diff_text}\n```"
        )),
    ];

    let registry = NativeToolRegistry::new();
    let mut cumulative_usage = TokenUsage::default();

    for _step in 0..6 {
        let req = CompletionRequest {
            conversation_id: format!("{conversation_id}:review"),
            messages: messages.clone(),
            tools: read_only_tools.clone(),
            reasoning_effort: settings.reasoning_effort.clone(),
            output_schema: None,
        };
        let resp = driver.complete(provider, &model, &req).await?;
        accumulate_usage(&mut cumulative_usage, &resp.usage);

        let tool_calls: Vec<(String, String, String)> = resp
            .assistant_message
            .parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::ToolCall {
                    call_id,
                    name,
                    arguments_json,
                } => Some((call_id.clone(), name.clone(), arguments_json.clone())),
                _ => None,
            })
            .collect();

        messages.push(resp.assistant_message.clone());

        if tool_calls.is_empty() {
            let text = extract_assistant_text(&resp.assistant_message);
            let parsed =
                extract_json_from_text::<RawReviewPayload>(&text).unwrap_or(RawReviewPayload {
                    summary: text.clone(),
                    findings: Vec::new(),
                });
            let findings = parsed
                .findings
                .into_iter()
                .map(|f| {
                    let start = (f.start_line.max(1)) as u32;
                    let end = f.end_line.map_or(start, |e| (e.max(1)) as u32);
                    ReviewFinding {
                        severity: normalize_severity(&f.severity).to_string(),
                        path: PathBuf::from(f.path),
                        start_line: start,
                        end_line: end,
                        title: f.title,
                        body: f.body,
                    }
                })
                .collect();
            return Ok(ReviewOutcome {
                summary: if parsed.summary.is_empty() {
                    text
                } else {
                    parsed.summary
                },
                findings,
                usage: cumulative_usage,
            });
        }

        let mut result_parts = Vec::with_capacity(tool_calls.len());
        for (call_id, name, args_json) in tool_calls {
            let args: Value = serde_json::from_str(&args_json).unwrap_or(Value::Null);
            match registry.execute(&ctx, &call_id, &name, &args).await {
                Ok(out) => result_parts.push(MessagePart::ToolResult {
                    call_id,
                    output: out.to_model_text(),
                    is_error: false,
                }),
                Err(err) => result_parts.push(MessagePart::ToolResult {
                    call_id,
                    output: err.to_string(),
                    is_error: true,
                }),
            }
        }
        messages.push(ProviderMessage {
            role: MessageRole::Tool,
            parts: result_parts,
        });
    }

    Ok(ReviewOutcome {
        summary: "Review reached step limit without final summary.".to_string(),
        findings: Vec::new(),
        usage: cumulative_usage,
    })
}

fn normalize_severity(s: &str) -> &'static str {
    match s.trim().to_ascii_lowercase().as_str() {
        "critical" | "p0" => "critical",
        "high" | "p1" => "high",
        "low" | "p3" | "nit" => "low",
        _ => "medium",
    }
}

pub(crate) fn accumulate_usage(dst: &mut TokenUsage, delta: &TokenUsage) {
    dst.input_tokens = dst.input_tokens.saturating_add(delta.input_tokens);
    dst.cached_input_tokens = dst
        .cached_input_tokens
        .saturating_add(delta.cached_input_tokens);
    dst.output_tokens = dst.output_tokens.saturating_add(delta.output_tokens);
    dst.reasoning_tokens = dst.reasoning_tokens.saturating_add(delta.reasoning_tokens);
    dst.cost_usd += delta.cost_usd;
}

fn extract_json_from_text<T: serde::de::DeserializeOwned>(raw: &str) -> Option<T> {
    let trimmed = raw.trim();
    if let Ok(direct) = serde_json::from_str::<T>(trimmed) {
        return Some(direct);
    }
    // Try fenced ```json ... ``` block or outermost `{ ... }`
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end >= start {
        serde_json::from_str::<T>(&trimmed[start..=end]).ok()
    } else {
        None
    }
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<String, CoreError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| CoreError::Review {
            message: format!("failed to invoke `git {}`: {e}", args.join(" ")),
        })?;
    if !out.status.success() {
        return Err(CoreError::Review {
            message: format!(
                "`git {}` exited with {}: {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
