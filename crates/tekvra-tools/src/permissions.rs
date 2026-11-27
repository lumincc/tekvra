//! Explicit non-sandboxed consent model: tree-sitter Bash AST command decomposition,
//! `Plan`/`ReadOnly`/`Build`/`Auto`/`Yolo` mode evaluation, and narrow-to-broad scope ladders.

use std::path::{Path, PathBuf};

use tekvra_config::PermissionRule;
use tekvra_protocol::{
    ApprovalPrompt, ApprovalResolution, ApprovalScope, GrantLifetime, PermissionMode, RuleAction,
    ScopeLadderOption,
};
use tekvra_storage::StateIndex;
use tree_sitter::{Node, Parser};
use uuid::Uuid;

use crate::ToolsError;

const DYNAMIC_SHELL_BUILTINS: &[&str] = &["eval", "source", "."];
const SHELL_WRAPPERS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];
const SAFE_READONLY_COMMANDS: &[&[&str]] = &[
    &["pwd"],
    &["ls"],
    &["cat"],
    &["head"],
    &["tail"],
    &["wc"],
    &["which"],
    &["uname"],
    &["git", "status"],
    &["git", "diff"],
    &["git", "log"],
    &["git", "show"],
    &["git", "branch"],
    &["git", "rev-parse"],
];

/// Single constituent subcommand extracted from a Bash AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashSubcommand {
    /// Raw source text of this subcommand.
    pub raw: String,
    /// Parsed argv tokens (leading `VAR=val` assignments stripped).
    pub tokens: Vec<String>,
    /// File paths targeted by output redirections (`>`, `>>`, `&>`).
    pub write_redirects: Vec<String>,
    /// Whether this subcommand invokes `eval`, dynamic `$VAR` command name, or dynamic `sh -c`.
    pub dynamic: bool,
}

/// Complete decomposition of a shell invocation produced by `tree-sitter-bash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecomposedBashCommand {
    /// Original command text.
    pub original: String,
    /// Ordered constituent subcommands across `&&`, `||`, `;`, `|`, subshells, and `$()`.
    pub subcommands: Vec<BashSubcommand>,
    /// True if syntax had errors or any subcommand cannot be statically proven (`Once`-only grant!).
    pub once_only: bool,
}

/// Parses `command` with `tree-sitter-bash` and extracts all constituent subcommands and redirections.
#[must_use]
pub fn decompose_bash_command(command: &str) -> DecomposedBashCommand {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return DecomposedBashCommand {
            original: command.to_string(),
            subcommands: Vec::new(),
            once_only: true,
        };
    }

    let mut parser = Parser::new();
    let lang: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
    if parser.set_language(&lang).is_err() {
        return DecomposedBashCommand {
            original: command.to_string(),
            subcommands: Vec::new(),
            once_only: true,
        };
    }

    let Some(tree) = parser.parse(trimmed, None) else {
        return DecomposedBashCommand {
            original: command.to_string(),
            subcommands: Vec::new(),
            once_only: true,
        };
    };

    let root = tree.root_node();
    let mut subcommands = Vec::new();
    let mut has_unparseable = root.has_error();
    walk_bash_ast(
        root,
        trimmed.as_bytes(),
        &[],
        &mut subcommands,
        &mut has_unparseable,
    );

    if subcommands.is_empty() {
        has_unparseable = true;
    }
    let any_dynamic = subcommands.iter().any(|s| s.dynamic);
    DecomposedBashCommand {
        original: command.to_string(),
        subcommands,
        once_only: has_unparseable || any_dynamic,
    }
}

/// Syntax family of the shell that will actually execute a command, mirroring
/// `crate::pty`'s platform shell selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSyntax {
    /// POSIX shells (`/bin/bash` / `sh -c`) parsed with the tree-sitter-bash AST.
    Bash,
    /// Windows PowerShell (`pwsh.exe` / `powershell.exe -Command`) parsed with the
    /// native lexical statement decomposition.
    PowerShell,
}

/// Returns the syntax family of the platform shell `exec_command` will spawn.
#[must_use]
pub fn active_shell_syntax() -> ShellSyntax {
    if cfg!(windows) {
        ShellSyntax::PowerShell
    } else {
        ShellSyntax::Bash
    }
}

/// Shared decomposition contract used by the evaluator regardless of shell syntax.
pub type DecomposedShellCommand = DecomposedBashCommand;

/// Decomposes `command` with the given syntax family.
#[must_use]
pub fn decompose_shell_command_for(syntax: ShellSyntax, command: &str) -> DecomposedShellCommand {
    match syntax {
        ShellSyntax::Bash => decompose_bash_command(command),
        ShellSyntax::PowerShell => crate::powershell_syntax::decompose_powershell_command(command),
    }
}

/// Decomposes `command` with the syntax family of the platform shell.
#[must_use]
pub fn decompose_shell_command(command: &str) -> DecomposedShellCommand {
    decompose_shell_command_for(active_shell_syntax(), command)
}

fn walk_bash_ast(
    node: Node<'_>,
    source: &[u8],
    inherited_redirects: &[String],
    out: &mut Vec<BashSubcommand>,
    has_unparseable: &mut bool,
) {
    match node.kind() {
        "ERROR" => {
            *has_unparseable = true;
        }
        "redirected_statement" => {
            let mut redirects = inherited_redirects.to_vec();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "file_redirect" || child.kind() == "herestring_redirect" {
                    extract_write_redirect(child, source, &mut redirects, has_unparseable);
                }
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_bash_ast(body, source, &redirects, out, has_unparseable);
            } else {
                let mut c2 = node.walk();
                for child in node.children(&mut c2) {
                    if child.kind() != "file_redirect" {
                        walk_bash_ast(child, source, &redirects, out, has_unparseable);
                    }
                }
            }
            return;
        }
        "command" => {
            let raw = node
                .utf8_text(source)
                .unwrap_or_default()
                .trim()
                .to_string();
            let mut tokens = Vec::new();
            let mut dynamic = false;
            let mut local_redirects = inherited_redirects.to_vec();

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "variable_assignment" => {}
                    "file_redirect" => {
                        extract_write_redirect(
                            child,
                            source,
                            &mut local_redirects,
                            has_unparseable,
                        );
                    }
                    "command_name" => {
                        if contains_expansion(child) {
                            dynamic = true;
                        }
                        if let Ok(txt) = child.utf8_text(source) {
                            tokens.push(unquote_shell_token(txt.trim()));
                        }
                        // Also walk inside in case command_name has `$(...)`!
                        walk_nested_substitutions(child, source, out, has_unparseable);
                    }
                    _ if child.is_named() => {
                        if let Ok(txt) = child.utf8_text(source) {
                            tokens.push(unquote_shell_token(txt.trim()));
                        }
                        walk_nested_substitutions(child, source, out, has_unparseable);
                    }
                    _ => {}
                }
            }

            if let Some(first) = tokens.first().map(String::as_str) {
                let prog = Path::new(first)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(first);
                if DYNAMIC_SHELL_BUILTINS.contains(&prog) {
                    dynamic = true;
                } else if SHELL_WRAPPERS.contains(&prog) {
                    // `bash -c "..."`: if `-c` script is static without `$` expansion, decompose it recursively;
                    // otherwise mark `dynamic = true` (`once_only`)!
                    if let Some(c_pos) = tokens.iter().position(|t| t == "-c" || t == "-lc") {
                        match tokens.get(c_pos + 1) {
                            Some(inline_script) if !raw.contains('$') && !raw.contains('`') => {
                                let nested = decompose_bash_command(inline_script);
                                if nested.once_only {
                                    dynamic = true;
                                } else {
                                    out.extend(nested.subcommands);
                                    return;
                                }
                            }
                            _ => {
                                dynamic = true;
                            }
                        }
                    } else if tokens.len() > 1 {
                        // Executing a script file (`bash script.sh`) cannot statically prove child commands
                        dynamic = true;
                    }
                }
            } else {
                dynamic = true;
            }

            out.push(BashSubcommand {
                raw,
                tokens,
                write_redirects: local_redirects,
                dynamic,
            });
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_bash_ast(child, source, inherited_redirects, out, has_unparseable);
    }
}

fn walk_nested_substitutions(
    node: Node<'_>,
    source: &[u8],
    out: &mut Vec<BashSubcommand>,
    has_unparseable: &mut bool,
) {
    if node.kind() == "command_substitution" || node.kind() == "process_substitution" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk_bash_ast(child, source, &[], out, has_unparseable);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_nested_substitutions(child, source, out, has_unparseable);
    }
}

fn extract_write_redirect(
    node: Node<'_>,
    source: &[u8],
    redirects: &mut Vec<String>,
    has_unparseable: &mut bool,
) {
    let text = node.utf8_text(source).unwrap_or_default().trim();
    if text.contains('>') {
        if let Some(dest) = node.child_by_field_name("destination") {
            if contains_expansion(dest) {
                *has_unparseable = true;
            }
            if let Ok(dtxt) = dest.utf8_text(source) {
                redirects.push(unquote_shell_token(dtxt.trim()));
            }
        } else {
            redirects.push(text.to_string());
        }
    }
}

fn contains_expansion(node: Node<'_>) -> bool {
    if matches!(
        node.kind(),
        "simple_expansion" | "expansion" | "command_substitution"
    ) {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(contains_expansion)
}

fn unquote_shell_token(raw: &str) -> String {
    if (raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2)
        || (raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2)
    {
        raw[1..raw.len() - 1].to_string()
    } else {
        raw.to_string()
    }
}

/// Builds narrow-to-broad command scope ladder options for an interactive approval prompt.
/// For example, `cargo test -p tekvra-core` produces:
/// 1. Exact: `cargo test -p tekvra-core`
/// 2. `cargo test -p *` (if >3 tokens)
/// 3. `cargo test *`
/// 4. `cargo *` (`broad_grant: true` -> requires secondary confirmation if persisted!)
#[must_use]
pub fn build_command_scope_ladder(
    exact_command: &str,
    tokens: &[String],
    once_only: bool,
    home_dir: Option<&Path>,
) -> Vec<ScopeLadderOption> {
    let exact_scope = ApprovalScope::ExactCommand {
        command: exact_command.to_string(),
    };
    let mut ladder = vec![ScopeLadderOption {
        label: format!("Exact: `{exact_command}`"),
        broad_grant: exact_scope.requires_broad_confirmation(home_dir),
        scope: exact_scope,
    }];

    if once_only || tokens.is_empty() {
        return ladder;
    }

    // Add full-token prefix rung (`cargo test -p tekvra-core *`), then progressively broader prefixes down to 1 token (`cargo *`).
    let max_prefix = tokens.len().min(4);
    for len in (1..=max_prefix).rev() {
        let prefix_tokens = tokens[..len].to_vec();
        let scope = ApprovalScope::CommandPrefix {
            tokens: prefix_tokens.clone(),
        };
        let label = format!("Prefix: `{} *`", prefix_tokens.join(" "));
        let broad_grant = scope.requires_broad_confirmation(home_dir);
        if !ladder.iter().any(|opt| opt.scope == scope) {
            ladder.push(ScopeLadderOption {
                label,
                scope,
                broad_grant,
            });
        }
    }
    ladder
}

/// Candidate action submitted to [`PermissionEvaluator`].
#[derive(Debug, Clone)]
pub enum ToolAction {
    /// Read-only workspace inspection (`file_read`, `file_glob`, `file_grep`, `file_index`, `view_image`, `tool_output`).
    ReadWorkspace {
        /// Tool name.
        tool_name: String,
        /// Target path inside workspace.
        path: PathBuf,
    },
    /// Updating the session Markdown `plan` document (allowed in `Plan` mode).
    UpdatePlan,
    /// Mutating workspace files (`apply_patch`, `file_write`, `file_edit`).
    WriteWorkspace {
        /// Tool name.
        tool_name: String,
        /// Target paths.
        paths: Vec<PathBuf>,
    },
    /// Accessing a path outside the workspace root.
    ExternalPath {
        /// Tool name.
        tool_name: String,
        /// Canonical or lexical external path.
        path: PathBuf,
    },
    /// Executing a shell command via PTY (`exec_command`).
    Shell {
        /// Raw command string.
        command: String,
        /// Working directory.
        cwd: PathBuf,
    },
    /// Fetching an outbound HTTP/HTTPS URL (`webfetch`).
    WebFetch {
        /// Target URL.
        url: String,
    },
    /// Calling an external MCP server tool (directly or from Monty `code`).
    McpCall {
        /// MCP server id.
        server: String,
        /// MCP tool name.
        tool: String,
    },
}

/// Outcome of evaluating a [`ToolAction`] against modes, rules, and remembered grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Immediately permitted without prompting.
    Allow,
    /// Rejected immediately (explicit `deny` rule or disallowed in `Plan`/`ReadOnly` mode).
    Deny {
        /// Human/model-facing explanation.
        reason: String,
    },
    /// Requires interactive user approval via [`ApprovalPrompt`].
    Prompt(ApprovalPrompt),
}

/// Stateful session permission evaluator enforcing `Plan`/`ReadOnly`/`Build`/`Auto`/`Yolo` modes,
/// `permissions.toml` rules (`deny` always wins!), Bash AST decomposition, and remembered grants.
#[derive(Debug, Clone)]
pub struct PermissionEvaluator {
    mode: PermissionMode,
    workspace_root: PathBuf,
    home_dir: Option<PathBuf>,
    rules: Vec<PermissionRule>,
    conversation_grants: Vec<ApprovalScope>,
    persisted_grants: Vec<ApprovalScope>,
    syntax: ShellSyntax,
}

impl PermissionEvaluator {
    /// Creates a new [`PermissionEvaluator`].
    #[must_use]
    pub fn new(
        mode: PermissionMode,
        workspace_root: PathBuf,
        home_dir: Option<PathBuf>,
        rules: Vec<PermissionRule>,
        persisted_grants: Vec<ApprovalScope>,
    ) -> Self {
        Self {
            mode,
            workspace_root,
            home_dir,
            rules,
            conversation_grants: Vec::new(),
            persisted_grants,
            syntax: active_shell_syntax(),
        }
    }

    /// Overrides the shell syntax used for command decomposition (platform default is
    /// [`active_shell_syntax`]; primarily for tests and explicit host overrides).
    #[must_use]
    pub fn with_shell_syntax(mut self, syntax: ShellSyntax) -> Self {
        self.syntax = syntax;
        self
    }

    /// Returns the current [`PermissionMode`].
    #[must_use]
    pub const fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Transitions the session's active [`PermissionMode`] without altering conversation history.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// Evaluates `action` and returns `Allow`, `Deny`, or `Prompt(ApprovalPrompt)`.
    #[must_use]
    pub fn evaluate(&self, item_id: &str, action: &ToolAction) -> PermissionDecision {
        // Step 1: Explicit `deny` rules in `permissions.toml` ALWAYS win in EVERY mode (including Auto & YOLO)!
        if let Some(deny_reason) = self.check_explicit_deny(action) {
            return PermissionDecision::Deny {
                reason: deny_reason,
            };
        }

        // Step 2: Mode-level hard invariants (`Plan` and `ReadOnly`)
        match action {
            ToolAction::UpdatePlan => {
                if self.mode == PermissionMode::ReadOnly {
                    PermissionDecision::Deny {
                        reason: "Plan updates are disabled during read-only review sessions."
                            .to_string(),
                    }
                } else {
                    PermissionDecision::Allow
                }
            }
            ToolAction::ReadWorkspace { .. } => PermissionDecision::Allow,
            ToolAction::WriteWorkspace { tool_name, .. } => {
                if !self.mode.allows_workspace_writes() {
                    PermissionDecision::Deny {
                        reason: format!(
                            "`{tool_name}` is blocked in `{:?}` mode; record proposed changes using the `plan` tool instead.",
                            self.mode
                        ),
                    }
                } else if self.check_explicit_ask_for_write(tool_name, action)
                    && self.mode != PermissionMode::Yolo
                {
                    PermissionDecision::Prompt(self.build_path_prompt(
                        item_id,
                        tool_name,
                        &self.workspace_root,
                    ))
                } else {
                    PermissionDecision::Allow
                }
            }
            ToolAction::ExternalPath { tool_name, path } => {
                if !self.mode.allows_workspace_writes() {
                    PermissionDecision::Deny {
                        reason: format!(
                            "External path access via `{tool_name}` is blocked in `{:?}` mode.",
                            self.mode
                        ),
                    }
                } else if self.is_scope_granted_for_path(path) || self.mode == PermissionMode::Yolo
                {
                    PermissionDecision::Allow
                } else {
                    // Even `Auto` mode enforces workspace path boundaries and prompts for external paths!
                    PermissionDecision::Prompt(self.build_path_prompt(item_id, tool_name, path))
                }
            }
            ToolAction::Shell { command, cwd } => self.evaluate_shell(item_id, command, cwd),
            ToolAction::WebFetch { url } => {
                if self.matches_rule("webfetch", url, RuleAction::Allow)
                    || self.is_scope_granted_for_url(url)
                {
                    PermissionDecision::Allow
                } else if self.matches_rule("webfetch", url, RuleAction::Ask) {
                    if self.mode == PermissionMode::Yolo {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Prompt(self.build_url_prompt(item_id, url))
                    }
                } else if matches!(self.mode, PermissionMode::Auto | PermissionMode::Yolo) {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Prompt(self.build_url_prompt(item_id, url))
                }
            }
            ToolAction::McpCall { server, tool } => {
                let target = format!("{server}/{tool}");
                if self.matches_rule("mcp", &target, RuleAction::Allow)
                    || self.is_scope_granted_for_mcp(server, tool)
                {
                    PermissionDecision::Allow
                } else if self.matches_rule("mcp", &target, RuleAction::Ask) {
                    if self.mode == PermissionMode::Yolo {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Prompt(self.build_mcp_prompt(item_id, server, tool))
                    }
                } else if matches!(self.mode, PermissionMode::Auto | PermissionMode::Yolo) {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Prompt(self.build_mcp_prompt(item_id, server, tool))
                }
            }
        }
    }

    /// Applies an interactive [`ApprovalResolution`] to a previously returned [`ApprovalPrompt`],
    /// enforcing `once_only` restrictions and `confirmed_broad` checks before persisting grants.
    ///
    /// # Errors
    /// Returns [`ToolsError::PermissionDenied`] if the user denied the prompt, attempted a reusable grant
    /// on a `once_only` command, or omitted `confirmed_broad` on a persistent broad grant.
    pub fn apply_resolution(
        &mut self,
        prompt: &ApprovalPrompt,
        resolution: ApprovalResolution,
        state_index: Option<&StateIndex>,
    ) -> Result<(), ToolsError> {
        match resolution {
            ApprovalResolution::Deny { reason } => Err(ToolsError::PermissionDenied {
                message: reason.unwrap_or_else(|| {
                    format!("user denied `{}` ({})", prompt.tool_name, prompt.summary)
                }),
            }),
            ApprovalResolution::Approve {
                scope,
                lifetime,
                confirmed_broad,
            } => {
                if prompt.once_only && lifetime != GrantLifetime::Once {
                    return Err(ToolsError::PermissionDenied {
                        message: "dynamic or unparseable shell commands only allow single-invocation (`Once`) approval".to_string(),
                    });
                }
                let chosen_scope = scope.or_else(|| prompt.ladder.first().map(|r| r.scope.clone()));
                if lifetime != GrantLifetime::Once
                    && let Some(sc) = chosen_scope
                {
                    if prompt.once_only && !matches!(sc, ApprovalScope::ExactCommand { .. }) {
                        return Err(ToolsError::PermissionDenied {
                            message:
                                "reusable pattern scopes are disallowed for dynamic shell commands"
                                    .to_string(),
                        });
                    }
                    if sc.requires_broad_confirmation(self.home_dir.as_deref()) && !confirmed_broad
                    {
                        return Err(ToolsError::PermissionDenied {
                            message: "broad permission grant requires explicit secondary confirmation (`confirmed_broad = true`)".to_string(),
                        });
                    }
                    match lifetime {
                        GrantLifetime::Conversation => {
                            self.conversation_grants.push(sc);
                        }
                        GrantLifetime::Project => {
                            if let Some(idx) = state_index {
                                idx.save_permission_grant(
                                    Some(&self.workspace_root),
                                    GrantLifetime::Project,
                                    &sc,
                                    0,
                                )?;
                            }
                            self.persisted_grants.push(sc);
                        }
                        GrantLifetime::Global => {
                            if let Some(idx) = state_index {
                                idx.save_permission_grant(None, GrantLifetime::Global, &sc, 0)?;
                            }
                            self.persisted_grants.push(sc);
                        }
                        GrantLifetime::Once => {}
                    }
                }
                Ok(())
            }
        }
    }

    fn evaluate_shell(&self, item_id: &str, command: &str, cwd: &Path) -> PermissionDecision {
        let decomposed = decompose_shell_command_for(self.syntax, command);

        // In `Plan` or `ReadOnly` mode, ONLY statically proven read-only commands without write redirects are allowed!
        if matches!(self.mode, PermissionMode::Plan | PermissionMode::ReadOnly) {
            let all_readonly = !decomposed.once_only
                && !decomposed.subcommands.is_empty()
                && decomposed
                    .subcommands
                    .iter()
                    .all(|sub| is_readonly_subcommand(sub, self.syntax));
            if all_readonly {
                return PermissionDecision::Allow;
            }
            return PermissionDecision::Deny {
                reason: format!(
                    "Non-read-only shell command `{command}` is blocked in `{:?}` mode; record proposed commands in the `plan` instead.",
                    self.mode
                ),
            };
        }

        // Check if cwd escapes workspace_root
        let cwd_outside = !cwd.starts_with(&self.workspace_root);

        if decomposed.once_only {
            if self.mode == PermissionMode::Yolo {
                return PermissionDecision::Allow;
            }
            // Even `Auto` mode prompts `Once` for dynamic/unparseable inline scripts (`eval`, `bash -c "$DYN"`)!
            return PermissionDecision::Prompt(ApprovalPrompt {
                approval_id: format!("appr_{}", Uuid::now_v7().simple()),
                item_id: item_id.to_string(),
                tool_name: "exec_command".to_string(),
                summary: command.to_string(),
                cwd: Some(cwd.to_path_buf()),
                once_only: true,
                ladder: build_command_scope_ladder(command, &[], true, self.home_dir.as_deref()),
            });
        }

        // Evaluate EVERY constituent subcommand independently!
        for sub in &decomposed.subcommands {
            // Check write redirections escaping workspace
            for redir in &sub.write_redirects {
                let redir_path = cwd.join(redir);
                if !redir_path.starts_with(&self.workspace_root)
                    && !self.is_scope_granted_for_path(&redir_path)
                    && self.mode != PermissionMode::Yolo
                {
                    return PermissionDecision::Prompt(self.build_path_prompt(
                        item_id,
                        "exec_command",
                        &redir_path,
                    ));
                }
            }

            let sub_text = sub.tokens.join(" ");
            let explicitly_asked = self.matches_rule("exec_command", &sub_text, RuleAction::Ask)
                || self.matches_rule("bash", &sub_text, RuleAction::Ask);
            if explicitly_asked && self.mode != PermissionMode::Yolo {
                return PermissionDecision::Prompt(ApprovalPrompt {
                    approval_id: format!("appr_{}", Uuid::now_v7().simple()),
                    item_id: item_id.to_string(),
                    tool_name: "exec_command".to_string(),
                    summary: sub.raw.clone(),
                    cwd: Some(cwd.to_path_buf()),
                    once_only: false,
                    ladder: build_command_scope_ladder(
                        &sub.raw,
                        &sub.tokens,
                        false,
                        self.home_dir.as_deref(),
                    ),
                });
            }

            let covered = is_readonly_subcommand(sub, self.syntax)
                || self.matches_rule("exec_command", &sub_text, RuleAction::Allow)
                || self.matches_rule("bash", &sub_text, RuleAction::Allow)
                || self.is_scope_granted_for_subcommand(&sub.raw, &sub.tokens);

            if !covered {
                if self.mode == PermissionMode::Yolo
                    || (self.mode == PermissionMode::Auto && !cwd_outside)
                {
                    continue;
                }
                // Return prompt scoped specifically to the uncovered subcommand (e.g. `rm -rf build/` in `git status && rm -rf build/`)!
                return PermissionDecision::Prompt(ApprovalPrompt {
                    approval_id: format!("appr_{}", Uuid::now_v7().simple()),
                    item_id: item_id.to_string(),
                    tool_name: "exec_command".to_string(),
                    summary: sub.raw.clone(),
                    cwd: Some(cwd.to_path_buf()),
                    once_only: false,
                    ladder: build_command_scope_ladder(
                        &sub.raw,
                        &sub.tokens,
                        false,
                        self.home_dir.as_deref(),
                    ),
                });
            }
        }

        if cwd_outside && !self.is_scope_granted_for_path(cwd) && self.mode != PermissionMode::Yolo
        {
            return PermissionDecision::Prompt(self.build_path_prompt(
                item_id,
                "exec_command",
                cwd,
            ));
        }

        PermissionDecision::Allow
    }

    fn check_explicit_deny(&self, action: &ToolAction) -> Option<String> {
        match action {
            ToolAction::Shell { command, .. } => {
                if self.matches_rule("exec_command", command, RuleAction::Deny)
                    || self.matches_rule("bash", command, RuleAction::Deny)
                {
                    return Some(format!(
                        "command `{command}` blocked by explicit `deny` rule"
                    ));
                }
                let decomposed = decompose_shell_command_for(self.syntax, command);
                for sub in &decomposed.subcommands {
                    let sub_joined = sub.tokens.join(" ");
                    if self.matches_rule("exec_command", &sub_joined, RuleAction::Deny)
                        || self.matches_rule("bash", &sub_joined, RuleAction::Deny)
                        || self.matches_rule("exec_command", &sub.raw, RuleAction::Deny)
                    {
                        return Some(format!(
                            "subcommand `{}` blocked by explicit `deny` rule",
                            sub.raw
                        ));
                    }
                }
                None
            }
            ToolAction::ReadWorkspace { tool_name, path }
            | ToolAction::ExternalPath { tool_name, path } => {
                let p = path.to_string_lossy();
                self.matches_rule(tool_name, &p, RuleAction::Deny)
                    .then(|| format!("`{tool_name}` on `{p}` blocked by explicit `deny` rule"))
            }
            ToolAction::WriteWorkspace { tool_name, paths } => {
                for p in paths {
                    let s = p.to_string_lossy();
                    if self.matches_rule(tool_name, &s, RuleAction::Deny)
                        || self.matches_rule("file_write", &s, RuleAction::Deny)
                    {
                        return Some(format!(
                            "`{tool_name}` on `{s}` blocked by explicit `deny` rule"
                        ));
                    }
                }
                None
            }
            ToolAction::WebFetch { url } => self
                .matches_rule("webfetch", url, RuleAction::Deny)
                .then(|| format!("`webfetch` to `{url}` blocked by explicit `deny` rule")),
            ToolAction::McpCall { server, tool } => {
                let target = format!("{server}/{tool}");
                self.matches_rule("mcp", &target, RuleAction::Deny)
                    .then(|| format!("MCP call `{target}` blocked by explicit `deny` rule"))
            }
            ToolAction::UpdatePlan => None,
        }
    }

    fn check_explicit_ask_for_write(&self, tool_name: &str, action: &ToolAction) -> bool {
        if let ToolAction::WriteWorkspace { paths, .. } = action {
            paths.iter().any(|p| {
                let s = p.to_string_lossy();
                self.matches_rule(tool_name, &s, RuleAction::Ask)
                    || self.matches_rule("file_write", &s, RuleAction::Ask)
            })
        } else {
            false
        }
    }

    fn matches_rule(&self, tool_name: &str, candidate: &str, expected_action: RuleAction) -> bool {
        self.rules.iter().any(|rule| {
            rule.action == expected_action
                && (rule.tool == "*" || rule.tool == tool_name)
                && pattern_matches(&rule.pattern, candidate)
        })
    }

    fn all_grants(&self) -> impl Iterator<Item = &ApprovalScope> {
        self.conversation_grants
            .iter()
            .chain(self.persisted_grants.iter())
    }

    fn is_scope_granted_for_subcommand(&self, raw: &str, tokens: &[String]) -> bool {
        self.all_grants().any(|scope| match scope {
            ApprovalScope::ExactCommand { command } => command == raw,
            ApprovalScope::CommandPrefix {
                tokens: prefix_tokens,
            } => !prefix_tokens.is_empty() && tokens.starts_with(prefix_tokens),
            _ => false,
        })
    }

    fn is_scope_granted_for_path(&self, target: &Path) -> bool {
        self.all_grants().any(|scope| match scope {
            ApprovalScope::PathPrefix { path } => target.starts_with(path),
            _ => false,
        })
    }

    fn is_scope_granted_for_url(&self, url: &str) -> bool {
        self.all_grants().any(|scope| match scope {
            ApprovalScope::UrlPrefix { prefix } => pattern_matches(prefix, url),
            _ => false,
        })
    }

    fn is_scope_granted_for_mcp(&self, server: &str, tool: &str) -> bool {
        self.all_grants().any(|scope| match scope {
            ApprovalScope::McpTool { server: s, tool: t } => s == server && (t == "*" || t == tool),
            _ => false,
        })
    }

    fn build_path_prompt(&self, item_id: &str, tool_name: &str, path: &Path) -> ApprovalPrompt {
        let mut ladder = Vec::new();
        let exact_scope = ApprovalScope::PathPrefix {
            path: path.to_path_buf(),
        };
        ladder.push(ScopeLadderOption {
            label: format!("Path: `{}`", path.display()),
            broad_grant: exact_scope.requires_broad_confirmation(self.home_dir.as_deref()),
            scope: exact_scope,
        });
        if let Some(parent) = path.parent() {
            let parent_scope = ApprovalScope::PathPrefix {
                path: parent.to_path_buf(),
            };
            ladder.push(ScopeLadderOption {
                label: format!("Directory: `{}`", parent.display()),
                broad_grant: parent_scope.requires_broad_confirmation(self.home_dir.as_deref()),
                scope: parent_scope,
            });
        }
        ApprovalPrompt {
            approval_id: format!("appr_{}", Uuid::now_v7().simple()),
            item_id: item_id.to_string(),
            tool_name: tool_name.to_string(),
            summary: format!("{tool_name} {}", path.display()),
            cwd: None,
            once_only: false,
            ladder,
        }
    }

    fn build_url_prompt(&self, item_id: &str, url: &str) -> ApprovalPrompt {
        let mut ladder = Vec::new();
        let exact = ApprovalScope::UrlPrefix {
            prefix: url.to_string(),
        };
        ladder.push(ScopeLadderOption {
            label: format!("URL: `{url}`"),
            broad_grant: false,
            scope: exact,
        });
        if let Ok(parsed) = url::Url::parse(url)
            && let Some(host) = parsed.host_str()
        {
            let host_prefix = format!("{}://{host}/*", parsed.scheme());
            let host_scope = ApprovalScope::UrlPrefix {
                prefix: host_prefix.clone(),
            };
            ladder.push(ScopeLadderOption {
                label: format!("Origin: `{host_prefix}`"),
                broad_grant: false,
                scope: host_scope,
            });
        }
        ApprovalPrompt {
            approval_id: format!("appr_{}", Uuid::now_v7().simple()),
            item_id: item_id.to_string(),
            tool_name: "webfetch".to_string(),
            summary: format!("GET {url}"),
            cwd: None,
            once_only: false,
            ladder,
        }
    }

    fn build_mcp_prompt(&self, item_id: &str, server: &str, tool: &str) -> ApprovalPrompt {
        let exact = ApprovalScope::McpTool {
            server: server.to_string(),
            tool: tool.to_string(),
        };
        let all_on_server = ApprovalScope::McpTool {
            server: server.to_string(),
            tool: "*".to_string(),
        };
        ApprovalPrompt {
            approval_id: format!("appr_{}", Uuid::now_v7().simple()),
            item_id: item_id.to_string(),
            tool_name: "mcp".to_string(),
            summary: format!("{server}/{tool}"),
            cwd: None,
            once_only: false,
            ladder: vec![
                ScopeLadderOption {
                    label: format!("Tool: `{server}/{tool}`"),
                    scope: exact,
                    broad_grant: false,
                },
                ScopeLadderOption {
                    label: format!("Server: `{server}/*`"),
                    scope: all_on_server,
                    broad_grant: true,
                },
            ],
        }
    }
}

/// PowerShell cmdlets and (program, subcommand) pairs that never mutate state, matched
/// case-insensitively on the program token (PowerShell is case-insensitive).
const SAFE_READONLY_POWERSHELL_CMDS: &[&str] = &[
    "get-childitem",
    "get-content",
    "get-item",
    "get-itemproperty",
    "get-location",
    "get-process",
    "get-service",
    "get-help",
    "get-command",
    "get-member",
    "select-string",
    "select-object",
    "where-object",
    "foreach-object",
    "measure-object",
    "sort-object",
    "group-object",
    "compare-object",
    "test-path",
    "resolve-path",
    "out-string",
    "out-host",
    "format-table",
    "format-list",
    "format-wide",
];

/// `true` for a numeric PS range literal (`1..300`): a value generator, never a writer.
fn is_numeric_range(token: &str) -> bool {
    match token.split_once("..") {
        Some((low, high)) => {
            !low.is_empty()
                && !high.is_empty()
                && low.chars().all(|c| c.is_ascii_digit())
                && high.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

fn is_readonly_subcommand(sub: &BashSubcommand, syntax: ShellSyntax) -> bool {
    if sub.dynamic || !sub.write_redirects.is_empty() || sub.tokens.is_empty() {
        return false;
    }
    let prog = Path::new(&sub.tokens[0])
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&sub.tokens[0]);
    match syntax {
        ShellSyntax::Bash => SAFE_READONLY_COMMANDS.iter().any(|allowed| {
            if allowed.len() == 1 {
                prog == allowed[0]
            } else {
                prog == allowed[0] && sub.tokens.get(1).is_some_and(|second| second == allowed[1])
            }
        }),
        ShellSyntax::PowerShell => {
            let lowered = prog.to_ascii_lowercase();
            let is_git_readonly = lowered == "git"
                && matches!(
                    sub.tokens.get(1).map(String::as_str),
                    Some("status" | "diff" | "log" | "show" | "branch" | "rev-parse")
                );
            SAFE_READONLY_POWERSHELL_CMDS.contains(&lowered.as_str())
                || is_numeric_range(&sub.tokens[0])
                || is_git_readonly
        }
    }
}

fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(" *") {
        return candidate == prefix || candidate.starts_with(&format!("{prefix} "));
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return candidate.starts_with(prefix);
    }
    if let Ok(glob) = globset::Glob::new(pattern) {
        return glob.compile_matcher().is_match(candidate);
    }
    pattern == candidate
}
