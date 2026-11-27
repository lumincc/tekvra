//! Native PowerShell statement decomposition for the Windows shell backend: a lexical
//! tokenizer and statement splitter that mirrors the bash decomposition contract
//! (`raw` / `tokens` / `write_redirects` / `dynamic` / `once_only`) without a grammar crate.

use crate::permissions::{BashSubcommand, DecomposedBashCommand};

/// PS constructs that evaluate arbitrary text at runtime (`Once`-only grants).
const DYNAMIC_PS_BUILTINS: &[&str] = &["invoke-expression", "iex"];
/// Nested `powershell -Command` payloads are treated as dynamic (out-of-scope decomposition).
const PS_SHELL_WRAPPERS: &[&str] = &[
    "pwsh",
    "pwsh.exe",
    "powershell",
    "powershell.exe",
    "powershell_ise",
    "powershell_ise.exe",
];
/// Cmdlets that make an otherwise read-looking script block destructive (design D5).
const PS_DYNAMIC_BLOCK_PATTERNS: &[&str] = &[
    "remove-item",
    "set-content",
    "add-content",
    "start-process",
    "invoke-expression",
    "invoke-command",
    "start-job",
    "iex",
];

/// Parses `command` with a PowerShell-aware lexical splitter and extracts all constituent
/// subcommands, redirections, and dynamic constructs into the shared decomposition contract.
#[must_use]
pub fn decompose_powershell_command(command: &str) -> DecomposedBashCommand {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return DecomposedBashCommand {
            original: command.to_string(),
            subcommands: Vec::new(),
            once_only: true,
        };
    }

    let (raw_statements, split_error) = split_statements(trimmed);
    let mut subcommands = Vec::new();
    let mut has_unparseable = split_error;
    for raw in &raw_statements {
        let (tokens, redirects, dynamic, token_error) = tokenize_statement(raw);
        has_unparseable |= token_error;
        if tokens.is_empty() && redirects.is_empty() {
            // Statement produced nothing provable (e.g. lone grouping): keep the
            // once-only signal but do not emit an empty subcommand.
            has_unparseable = true;
            continue;
        }
        let dynamic = dynamic
            || redirects
                .iter()
                .any(|r| r.contains('$') || r.contains("$("));
        subcommands.push(BashSubcommand {
            raw: raw.clone(),
            tokens,
            write_redirects: redirects,
            dynamic,
        });
    }

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

/// Splits PS source into top-level statements on `;`, newline, `|`, `&&`, and `||`,
/// honoring quotes, backtick continuations/escapes, comments, `(...)` grouping (split
/// inside), and `{...}` script blocks (opaque). Returns `(statements, had_error)`.
fn split_statements(source: &str) -> (Vec<String>, bool) {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut chars = source.chars().peekable();
    let mut paren_depth: usize = 0;
    let mut brace_depth: usize = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut has_error = false;

    while let Some(c) = chars.next() {
        if in_single {
            current.push(c);
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    current.push('\'');
                    chars.next();
                } else {
                    in_single = false;
                }
            }
            continue;
        }
        if in_double {
            current.push(c);
            if c == '`' {
                if let Some(&next) = chars.peek() {
                    current.push(next);
                    chars.next();
                } else {
                    has_error = true;
                }
            } else if c == '"' {
                in_double = false;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                in_single = c == '\'';
                in_double = c == '"';
                current.push(c);
            }
            '`' => match chars.peek() {
                Some('\n') => {
                    chars.next();
                    current.push(' ');
                }
                Some('\r') => current.push('`'),
                Some(&next) => {
                    current.push('`');
                    current.push(next);
                    chars.next();
                }
                None => has_error = true,
            },
            '<' if chars.peek() == Some(&'#') => {
                // Block comment `<# ... #>`: skipped entirely.
                chars.next();
                let mut closed = false;
                while let Some(comment_char) = chars.next() {
                    if comment_char == '#' && chars.peek() == Some(&'>') {
                        chars.next();
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    has_error = true;
                }
                current.push(' ');
            }
            '#' if current.trim().is_empty() || current.ends_with(char::is_whitespace) => {
                // Line comment (only when `#` starts a token).
                while let Some(&next) = chars.peek() {
                    if next == '\n' {
                        break;
                    }
                    chars.next();
                }
                current.push(' ');
            }
            '(' => {
                paren_depth += 1;
                current.push(c);
            }
            ')' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                    current.push(c);
                } else {
                    has_error = true;
                }
            }
            '{' => {
                brace_depth += 1;
                current.push(c);
            }
            '}' => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                    current.push(c);
                } else {
                    has_error = true;
                }
            }
            ';' | '\n' => {
                if brace_depth == 0 {
                    push_statement(&mut statements, &mut current);
                } else {
                    current.push(c);
                }
            }
            '\r' => {}
            '|' | '&' => {
                // `&` inside redirection merges (`2>&1`, `&>file`) is syntax, not a separator.
                let merge_ampersand =
                    c == '&' && (chars.peek() == Some(&'>') || current.trim_end().ends_with('>'));
                if brace_depth == 0 && !merge_ampersand {
                    // Consume the second char of `||` and `&&`.
                    if chars.peek() == Some(&c) {
                        chars.next();
                    }
                    push_statement(&mut statements, &mut current);
                } else {
                    current.push(c);
                }
            }
            _ => current.push(c),
        }
    }
    if in_single || in_double {
        has_error = true;
    }
    if paren_depth > 0 || brace_depth > 0 {
        has_error = true;
    }
    push_statement(&mut statements, &mut current);
    (statements, has_error)
}

fn push_statement(statements: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        statements.push(trimmed.to_string());
    }
    current.clear();
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RedirectPiece {
    /// Bare operator (`>`, `>>`, `2>`, `*>`): the next piece is the target.
    Operator,
    /// Operator and target written together (`>out.txt`, `2>err.log`).
    WithTarget(String),
    /// Stream merge (`2>&1`, `*>&1`): not a write target.
    Merge,
    /// Operator whose target is a variable/subexpression/script block (`> $f`).
    DynamicTarget,
}

/// Classifies a raw piece as a PowerShell redirection form, if it is one.
fn classify_redirect(piece: &str) -> Option<RedirectPiece> {
    let gt = piece.find('>')?;
    let (prefix, rest) = piece.split_at(gt);
    if prefix.len() > 1 || !prefix.chars().all(|c| c.is_ascii_digit() || c == '*') {
        return None;
    }
    let after_gt = &rest[1..];
    let after_gt = after_gt.strip_prefix('>').unwrap_or(after_gt);
    if let Some(merged) = after_gt.strip_prefix('&') {
        if !merged.is_empty() && merged.chars().all(|c| c.is_ascii_digit() || c == '*') {
            return Some(RedirectPiece::Merge);
        }
        return None;
    }
    if after_gt.is_empty() {
        return Some(RedirectPiece::Operator);
    }
    if after_gt.starts_with('$') || after_gt.starts_with('(') || after_gt.starts_with('{') {
        return Some(RedirectPiece::DynamicTarget);
    }
    Some(RedirectPiece::WithTarget(after_gt.to_string()))
}

/// Tokenizes one statement into argv pieces, output redirection targets, and a dynamic
/// flag. Call operators with non-literal targets, command-position variables/splatting
/// and subexpressions, nested shell payloads, and destructive script blocks are dynamic.
fn tokenize_statement(statement: &str) -> (Vec<String>, Vec<String>, bool, bool) {
    let mut pieces: Vec<String> = Vec::new();
    let mut dynamic = false;
    let mut error = false;

    let mut chars = statement.chars().peekable();
    let mut cur = String::new();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // Literal string: strip quotes; `''` is an escaped quote.
                let mut literal = String::new();
                loop {
                    match chars.next() {
                        Some('\'') => {
                            if chars.peek() == Some(&'\'') {
                                literal.push('\'');
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        Some(inner) => literal.push(inner),
                        None => {
                            error = true;
                            break;
                        }
                    }
                }
                cur.push_str(&literal);
            }
            '"' => {
                // Expandable string: backtick escapes; contents kept (may contain `$`).
                let mut literal = String::new();
                loop {
                    match chars.next() {
                        Some('`') => match chars.next() {
                            Some(next) => literal.push(next),
                            None => {
                                error = true;
                                break;
                            }
                        },
                        Some('"') => break,
                        Some(inner) => literal.push(inner),
                        None => {
                            error = true;
                            break;
                        }
                    }
                }
                cur.push_str(&literal);
            }
            '`' => match chars.next() {
                Some(next) => cur.push(next),
                None => error = true,
            },
            '{' => {
                // Script block: consume the balanced body into one opaque token.
                let mut depth = 1usize;
                let mut block = String::from("{");
                while let Some(block_char) = chars.next() {
                    block.push(block_char);
                    match block_char {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        '\'' | '"' => {
                            // Quotes delimit inside blocks; don't count braces in strings.
                            let quote = block_char;
                            for inner in chars.by_ref() {
                                block.push(inner);
                                if inner == quote {
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if depth != 0 {
                    error = true;
                }
                pieces.push(std::mem::take(&mut cur));
                pieces.push(block);
            }
            '>' => {
                // Redirection: fold an attached stream prefix (`2>`, `*>`) into the piece.
                let stream =
                    if !cur.is_empty() && cur.chars().all(|ch| ch.is_ascii_digit() || ch == '*') {
                        std::mem::take(&mut cur)
                    } else {
                        pieces.push(std::mem::take(&mut cur));
                        String::new()
                    };
                let mut piece = stream;
                piece.push('>');
                if chars.peek() == Some(&'>') {
                    chars.next();
                    piece.push('>');
                }
                if chars.peek() == Some(&'&') {
                    chars.next();
                    piece.push('&');
                    match chars.next() {
                        Some(target_stream) => piece.push(target_stream),
                        None => error = true,
                    }
                }
                pieces.push(piece);
            }
            c if c.is_whitespace() => pieces.push(std::mem::take(&mut cur)),
            '(' | ')' => {
                // Grouping delimiters: structural, not part of argv.
                pieces.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    pieces.push(std::mem::take(&mut cur));

    // Pass 2: split redirection pieces from argv tokens, resolving targets.
    let mut tokens: Vec<String> = Vec::new();
    let mut redirects: Vec<String> = Vec::new();
    let mut pending_redirect = false;
    for piece in pieces {
        if piece.is_empty() {
            continue;
        }
        if let Some(kind) = classify_redirect(&piece) {
            match kind {
                RedirectPiece::Merge => {}
                RedirectPiece::Operator => pending_redirect = true,
                RedirectPiece::WithTarget(target) => redirects.push(target),
                RedirectPiece::DynamicTarget => {
                    dynamic = true;
                    pending_redirect = false;
                }
            }
            continue;
        }
        if pending_redirect {
            // The piece after a bare operator is the redirect target.
            pending_redirect = false;
            if piece.starts_with('$') || piece.starts_with('{') || piece.starts_with('(') {
                dynamic = true;
                error = true;
            } else {
                redirects.push(piece);
            }
            continue;
        }
        tokens.push(piece);
    }
    if pending_redirect {
        // `command >` with no target cannot be statically authorized.
        dynamic = true;
        error = true;
    }

    // Leading call operator `&`: literal target stays static (program becomes tokens[0]);
    // variable/subexpression target is dynamic.
    if tokens.first().map(String::as_str) == Some("&") {
        match tokens.get(1) {
            Some(next) if next.starts_with('$') || next.starts_with('(') => dynamic = true,
            Some(_) => {}
            None => dynamic = true,
        }
        tokens.remove(0);
    }

    if let Some(first) = tokens.first().map(String::as_str) {
        let lowered = first.to_ascii_lowercase();
        let wrapper_payload = PS_SHELL_WRAPPERS.contains(&lowered.as_str())
            && tokens.iter().any(|t| {
                let low = t.to_ascii_lowercase();
                low == "-command" || low == "-c" || low == "-encodedcommand" || low == "-file"
            });
        if DYNAMIC_PS_BUILTINS.contains(&lowered.as_str())
            || lowered.starts_with('$')
            || lowered.starts_with('@')
            || wrapper_payload
        {
            dynamic = true;
        }
    }

    // Script blocks: destructive keywords inside an otherwise innocent pipeline are dynamic.
    if tokens.iter().filter(|t| t.starts_with('{')).any(|block| {
        let low = block.to_ascii_lowercase();
        PS_DYNAMIC_BLOCK_PATTERNS
            .iter()
            .any(|pattern| low.contains(pattern))
            || low.contains("& ")
    }) {
        dynamic = true;
    }

    (tokens, redirects, dynamic, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens_of(command: &str) -> Vec<Vec<String>> {
        decompose_powershell_command(command)
            .subcommands
            .iter()
            .map(|s| s.tokens.clone())
            .collect()
    }

    #[test]
    fn simple_cmdlet_and_pipeline_split() {
        let d = decompose_powershell_command("Get-ChildItem src -Recurse | Select-String pattern");
        assert!(!d.once_only);
        assert_eq!(
            tokens_of("Get-ChildItem src -Recurse | Select-String pattern"),
            vec![
                vec!["Get-ChildItem", "src", "-Recurse"],
                vec!["Select-String", "pattern"],
            ]
        );
    }

    #[test]
    fn semicolon_and_newline_split() {
        let d = decompose_powershell_command("git status\ngit log");
        assert_eq!(d.subcommands.len(), 2);
        let d = decompose_powershell_command("Write-Output a; Write-Output b");
        assert_eq!(d.subcommands.len(), 2);
    }

    #[test]
    fn backtick_continuation_joins_lines() {
        let d = decompose_powershell_command("Get-ChildItem -Recurse `\n  -Path src | Out-String");
        assert!(!d.once_only);
        assert_eq!(d.subcommands.len(), 2);
        assert_eq!(
            d.subcommands[0].tokens,
            vec!["Get-ChildItem", "-Recurse", "-Path", "src"]
        );
    }

    #[test]
    fn quoted_args_keep_spaces() {
        let d = decompose_powershell_command("Write-Output 'PS QUOTED OK'");
        assert_eq!(
            d.subcommands[0].tokens,
            vec!["Write-Output", "PS QUOTED OK"]
        );
        let d = decompose_powershell_command("Write-Output \"GOT:$tok\"");
        assert!(!d.once_only);
        assert_eq!(d.subcommands[0].tokens, vec!["Write-Output", "GOT:$tok"]);
    }

    #[test]
    fn redirections_captured_including_streams() {
        let d = decompose_powershell_command("Get-Content log.txt *> C:\\out\\all.txt");
        assert!(!d.once_only);
        assert_eq!(d.subcommands[0].write_redirects, vec!["C:\\out\\all.txt"]);

        let d = decompose_powershell_command("python run.py 2>> err.log > out.txt");
        assert_eq!(d.subcommands[0].write_redirects, vec!["err.log", "out.txt"]);

        // Attached targets (`>file`) and merge forms (`2>&1`).
        let d = decompose_powershell_command("python run.py >out.txt");
        assert_eq!(d.subcommands[0].write_redirects, vec!["out.txt"]);
        let d = decompose_powershell_command("python run.py 2>&1");
        assert!(!d.once_only);
        assert!(d.subcommands[0].write_redirects.is_empty());
    }

    #[test]
    fn dynamic_target_redirect_is_once_only() {
        let d = decompose_powershell_command("Get-Content a.txt > $outPath");
        assert!(d.once_only);
    }

    #[test]
    fn dynamic_constructs_force_once_only() {
        for command in [
            "Invoke-Expression \"$user_input\"",
            "iex $payload",
            "& $scriptPath -Force",
            "pwsh -Command \"Write-Output nested\"",
            "$cmd = 'x'",
        ] {
            let d = decompose_powershell_command(command);
            assert!(d.once_only, "expected once_only for `{command}`");
        }
    }

    #[test]
    fn literal_call_operator_stays_static() {
        let d = decompose_powershell_command("& 'C:\\Program Files\\Git\\bin\\git.exe' status");
        assert!(!d.once_only);
        assert_eq!(
            d.subcommands[0].tokens,
            vec!["C:\\Program Files\\Git\\bin\\git.exe", "status"]
        );
    }

    #[test]
    fn splatting_at_command_position_is_dynamic() {
        let d = decompose_powershell_command("@params");
        assert!(d.once_only);
    }

    #[test]
    fn destructive_script_blocks_are_dynamic() {
        let d =
            decompose_powershell_command("Get-ChildItem src | ForEach-Object { Remove-Item $_ }");
        assert!(d.once_only);
        let d = decompose_powershell_command("Get-ChildItem src | ForEach-Object { $_.Name }");
        assert!(!d.once_only);
    }

    #[test]
    fn unterminated_quote_is_unparseable() {
        let d = decompose_powershell_command("Write-Output 'open");
        assert!(d.once_only);
    }

    #[test]
    fn comments_are_stripped() {
        let d = decompose_powershell_command("# header\nWrite-Output ok");
        assert!(!d.once_only);
        assert_eq!(
            tokens_of("# header\nWrite-Output ok"),
            vec![vec!["Write-Output", "ok"]]
        );
    }
}
