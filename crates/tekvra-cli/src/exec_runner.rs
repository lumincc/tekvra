//! Headless non-interactive `tekvra exec` / `--print` runner supporting plain text, JSONL events,
//! `stream-json` bidirectional framing, and `--output-schema <PATH>` JSON Schema validation.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tekvra_core::SessionHandle;
use tekvra_protocol::{
    ApprovalResolution, GrantLifetime, InputDeliveryMode, InputImage, PermissionMode,
    SessionCommand, SessionEvent,
};

/// Options for headless `tekvra exec` / `--print`.
#[derive(Debug, Clone)]
pub struct ExecRunOptions {
    /// Optional prompt string from CLI arguments (reads stdin if `None`).
    pub prompt: Option<String>,
    /// Emit newline-delimited JSONL `SessionEvent` frames (`--json`).
    pub json_events: bool,
    /// Optional path to a JSON Schema file (`--output-schema <PATH>`) that the final output must satisfy.
    pub output_schema: Option<PathBuf>,
    /// Optional path to write the final assistant message (`-o` / `--output-last-message <PATH>`).
    pub output_file: Option<PathBuf>,
    /// Input stream format (`"text"` or `"stream-json"`).
    pub input_format: String,
    /// Output stream format (`"text"`, `"json"`, or `"stream-json"`).
    pub output_format: String,
    /// Active permission mode.
    pub mode: PermissionMode,
    /// Pre-validated `-i/--image` attachments sent with the first turn.
    pub images: Vec<InputImage>,
}

/// Exit code distinction for CLI callers (`2` = schema validation failure).
#[derive(Debug)]
pub struct SchemaValidationFailed(pub String);

impl std::fmt::Display for SchemaValidationFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "output failed JSON Schema validation: {}", self.0)
    }
}

impl std::error::Error for SchemaValidationFailed {}

/// Runs headless execution on `handle` according to `opts`.
pub async fn run_headless_exec(handle: SessionHandle, opts: ExecRunOptions) -> Result<()> {
    let schema_bundle = if let Some(schema_path) = &opts.output_schema {
        let raw_schema = std::fs::read_to_string(schema_path).with_context(|| {
            format!("failed reading --output-schema `{}`", schema_path.display())
        })?;
        let schema_json: Value = serde_json::from_str(&raw_schema)
            .with_context(|| format!("invalid JSON in `{}`", schema_path.display()))?;
        let validator = jsonschema::validator_for(&schema_json).map_err(|e| {
            anyhow::anyhow!("invalid JSON Schema in `{}`: {e}", schema_path.display())
        })?;
        Some((raw_schema, validator))
    } else {
        None
    };

    let prompts = collect_input_prompts(opts.prompt.as_deref(), &opts.input_format)?;
    if prompts.is_empty() {
        bail!("no prompt provided to `tekvra exec`");
    }

    let emit_jsonl = opts.json_events
        || opts.output_format.eq_ignore_ascii_case("json")
        || opts.output_format.eq_ignore_ascii_case("stream-json");

    let mut final_assistant_text = String::new();

    for base_prompt in prompts {
        let effective_prompt = match &schema_bundle {
            Some((raw_schema, _)) => format!(
                "{base_prompt}\n\nIMPORTANT: Respond ONLY with raw valid JSON conforming strictly to this JSON Schema:\n{raw_schema}"
            ),
            None => base_prompt,
        };

        let mut rx = handle.subscribe();
        handle
            .send(SessionCommand::SubmitInput {
                prompt: effective_prompt,
                delivery: InputDeliveryMode::Next,
                images: opts.images.clone(),
            })
            .await?;

        let mut turn_text = String::new();
        while let Ok(ev) = rx.recv().await {
            if emit_jsonl && let Ok(line) = serde_json::to_string(&ev) {
                println!("{line}");
                let _ = io::stdout().flush();
            }

            match ev {
                SessionEvent::ItemDelta { channel, delta, .. } if channel == "text" => {
                    turn_text.push_str(&delta);
                }
                SessionEvent::ItemCompleted {
                    text,
                    tool_output: None,
                    ..
                } => {
                    if !text.is_empty() {
                        turn_text = text;
                    }
                }
                SessionEvent::UserInputRequested { item_id, .. } => {
                    // Headless exec has no interactive host: resolve immediately with
                    // no answers so the `question` turn completes instead of hanging.
                    let _ = handle
                        .send(SessionCommand::ResolveUserInput {
                            item_id,
                            answers: BTreeMap::new(),
                        })
                        .await;
                }
                SessionEvent::ApprovalRequested { prompt, .. } => {
                    let resolution = if matches!(
                        opts.mode,
                        PermissionMode::Auto | PermissionMode::Yolo
                    ) {
                        ApprovalResolution::Approve {
                            scope: None,
                            lifetime: GrantLifetime::Once,
                            confirmed_broad: false,
                        }
                    } else {
                        ApprovalResolution::Deny {
                            reason: Some(
                                "headless `tekvra exec` denied interactive approval prompt; pass `--mode auto` or configure allow rules"
                                    .to_string(),
                            ),
                        }
                    };
                    let _ = handle
                        .send(SessionCommand::ResolveApproval {
                            approval_id: prompt.approval_id,
                            resolution,
                        })
                        .await;
                }
                SessionEvent::TurnCompleted { status, error, .. } => {
                    if status == "failed" {
                        bail!(
                            "turn failed: {}",
                            error.unwrap_or_else(|| "unknown error".to_string())
                        );
                    }
                    break;
                }
                _ => {}
            }
        }
        final_assistant_text = turn_text;
    }

    if let Some((_, validator)) = &schema_bundle {
        let candidate = strip_markdown_json_fence(final_assistant_text.trim());
        let parsed: Value = serde_json::from_str(candidate).map_err(|e| {
            SchemaValidationFailed(format!(
                "assistant output is not valid JSON ({e}): {candidate}"
            ))
        })?;
        if let Err(err) = validator.validate(&parsed) {
            return Err(SchemaValidationFailed(format!("{err}")).into());
        }
        let pretty = serde_json::to_string_pretty(&parsed)?;
        write_optional_output_file(opts.output_file.as_deref(), &pretty)?;
        if !emit_jsonl {
            println!("{pretty}");
        }
    } else {
        write_optional_output_file(opts.output_file.as_deref(), &final_assistant_text)?;
        if !emit_jsonl {
            if opts.output_format.eq_ignore_ascii_case("stream-json") {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "type": "result",
                        "text": final_assistant_text,
                    }))?
                );
            } else {
                println!("{final_assistant_text}");
            }
        }
    }

    Ok(())
}

fn write_optional_output_file(path: Option<&std::path::Path>, content: &str) -> Result<()> {
    let Some(target) = path else {
        return Ok(());
    };
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating parent directory for `-o {}`",
                target.display()
            )
        })?;
    }
    std::fs::write(target, content)
        .with_context(|| format!("failed writing output file `{}`", target.display()))
}

fn collect_input_prompts(cli_prompt: Option<&str>, input_format: &str) -> Result<Vec<String>> {
    if let Some(p) = cli_prompt {
        return Ok(vec![p.to_string()]);
    }
    let stdin = io::stdin();
    if input_format.eq_ignore_ascii_case("stream-json") {
        let mut out = Vec::new();
        for line in stdin.lock().lines() {
            let l = line?;
            let trimmed = l.trim();
            if trimmed.is_empty() {
                continue;
            }
            let val: Value = serde_json::from_str(trimmed)
                .with_context(|| format!("invalid stream-json line: {trimmed}"))?;
            if let Some(txt) = val
                .get("prompt")
                .and_then(Value::as_str)
                .or_else(|| val.get("text").and_then(Value::as_str))
            {
                out.push(txt.to_string());
            }
        }
        Ok(out)
    } else {
        let mut buf = String::new();
        for line in stdin.lock().lines() {
            buf.push_str(&line?);
            buf.push('\n');
        }
        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(vec![trimmed])
        }
    }
}

fn strip_markdown_json_fence(raw: &str) -> &str {
    if let Some(rest) = raw.strip_prefix("```json")
        && let Some(inner) = rest.strip_suffix("```")
    {
        return inner.trim();
    }
    if let Some(rest) = raw.strip_prefix("```")
        && let Some(inner) = rest.strip_suffix("```")
    {
        return inner.trim();
    }
    raw
}
