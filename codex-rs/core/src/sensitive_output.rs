use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_shell_command::bash::parse_shell_lc_plain_commands;
use codex_shell_command::parse_command::extract_shell_command;
use regex_lite::Regex;
use std::path::Component;
use std::path::Path;
use std::sync::OnceLock;

const REDACTED_SECRET_PLACEHOLDER: &str = "[REDACTED_SECRET]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SensitiveCommandBlock {
    pub(crate) path: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SanitizedOutput {
    pub(crate) text: String,
    pub(crate) changed: bool,
}

pub(crate) fn is_sensitive_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(value) => {
            is_sensitive_component_name(value.to_string_lossy().to_lowercase().as_str())
        }
        _ => false,
    })
}

pub(crate) fn first_sensitive_path<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) -> Option<String> {
    paths.into_iter().find_map(sensitive_path_display)
}

pub(crate) fn sensitive_command_block(command: &[String]) -> Option<SensitiveCommandBlock> {
    sensitive_path_in_command(command, /*depth*/ 0).map(|path| SensitiveCommandBlock {
        message: sensitive_command_block_message(path.as_str()),
        path,
    })
}

pub(crate) fn sensitive_command_block_from_text(command: &str) -> Option<SensitiveCommandBlock> {
    tokenize_shell_script(command).and_then(|tokens| {
        sensitive_path_in_command(&tokens, /*depth*/ 0).map(|path| SensitiveCommandBlock {
            message: sensitive_command_block_message(path.as_str()),
            path,
        })
    })
}

pub(crate) fn sensitive_command_block_message(path: &str) -> String {
    format!(
        "blocked: command would expose sensitive file contents ({path}). Use non-sensitive context or ask the user."
    )
}

pub(crate) fn sensitive_patch_block_message(path: &str) -> String {
    format!("blocked: patch modifies sensitive path {path}.")
}

pub(crate) fn sanitize_output_text(text: &str) -> SanitizedOutput {
    let diff_redacted = redact_sensitive_diff_blocks(text);
    let secret_redacted = redact_secret_patterns(diff_redacted.text.as_str());
    SanitizedOutput {
        text: secret_redacted.text,
        changed: diff_redacted.changed || secret_redacted.changed,
    }
}

pub(crate) fn sanitize_exec_output(exec_output: &ExecToolCallOutput) -> ExecToolCallOutput {
    let stdout = sanitize_output_text(exec_output.stdout.text.as_str());
    let stderr = sanitize_output_text(exec_output.stderr.text.as_str());
    let aggregated_output = sanitize_output_text(exec_output.aggregated_output.text.as_str());

    ExecToolCallOutput {
        exit_code: exec_output.exit_code,
        stdout: StreamOutput {
            text: stdout.text,
            truncated_after_lines: exec_output.stdout.truncated_after_lines,
        },
        stderr: StreamOutput {
            text: stderr.text,
            truncated_after_lines: exec_output.stderr.truncated_after_lines,
        },
        aggregated_output: StreamOutput {
            text: aggregated_output.text,
            truncated_after_lines: exec_output.aggregated_output.truncated_after_lines,
        },
        duration: exec_output.duration,
        timed_out: exec_output.timed_out,
    }
}

pub(crate) fn sanitize_utf8_lossy_bytes(bytes: &[u8]) -> Vec<u8> {
    sanitize_output_text(String::from_utf8_lossy(bytes).as_ref())
        .text
        .into_bytes()
}

fn sensitive_path_in_command(command: &[String], depth: usize) -> Option<String> {
    const MAX_SHELL_WRAPPER_DEPTH: usize = 4;

    if let Some(path) = sensitive_path_in_tokens(command) {
        return Some(path);
    }
    if depth >= MAX_SHELL_WRAPPER_DEPTH {
        return None;
    }

    if let Some(inner_commands) = parse_shell_lc_plain_commands(command)
        && let Some(path) = inner_commands
            .into_iter()
            .find_map(|inner| sensitive_path_in_command(&inner, depth + 1))
    {
        return Some(path);
    }

    if let Some((_shell, script)) = extract_shell_command(command)
        && let Some(tokens) = tokenize_shell_script(script)
    {
        return sensitive_path_in_command(&tokens, depth + 1);
    }

    None
}

fn tokenize_shell_script(script: &str) -> Option<Vec<String>> {
    shlex::split(script).or_else(|| {
        let tokens = script
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        (!tokens.is_empty()).then_some(tokens)
    })
}

fn sensitive_path_in_tokens(tokens: &[String]) -> Option<String> {
    tokens
        .iter()
        .flat_map(|token| token_path_candidates(token))
        .find_map(|candidate| sensitive_path_display(Path::new(candidate.as_str())))
}

fn token_path_candidates(token: &str) -> Vec<String> {
    let trimmed = trim_shell_punctuation(token);
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut candidates = Vec::with_capacity(3);

    if let Some((_, suffix)) = trimmed.rsplit_once(':')
        && !suffix.is_empty()
    {
        candidates.push(trim_shell_punctuation(suffix).to_string());
    }

    if let Some((_, suffix)) = trimmed.rsplit_once('=')
        && !suffix.is_empty()
    {
        candidates.push(trim_shell_punctuation(suffix).to_string());
    }

    candidates.push(trimmed.to_string());

    candidates
}

fn trim_shell_punctuation(token: &str) -> &str {
    token.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ';' | ',' | '|' | '&' | '<' | '>'
        )
    })
}

fn sensitive_path_display(path: &Path) -> Option<String> {
    if !is_sensitive_path(path) {
        return None;
    }

    if !path.is_absolute() {
        return Some(path.to_string_lossy().replace('\\', "/"));
    }

    let mut suffix = Vec::new();
    let mut capturing = false;
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_string_lossy().to_string();
                if capturing || is_sensitive_component_name(value.to_lowercase().as_str()) {
                    capturing = true;
                    suffix.push(value);
                }
            }
            Component::CurDir if capturing => suffix.push(".".to_string()),
            Component::ParentDir if capturing => suffix.push("..".to_string()),
            _ => {}
        }
    }

    Some(if suffix.is_empty() {
        path.to_string_lossy().replace('\\', "/")
    } else {
        suffix.join("/")
    })
}

fn is_sensitive_component_name(component: &str) -> bool {
    matches!(component, ".git" | "target" | "node_modules" | "vendor")
        || component == ".env"
        || component.starts_with(".env.")
        || component == "auth.json"
        || component.contains("credentials")
        || component == "id_rsa"
        || component == "id_ed25519"
        || component.ends_with(".pem")
        || component.ends_with(".key")
        || component.contains("private_key")
        || component.contains("private-key")
}

fn redact_sensitive_diff_blocks(input: &str) -> SanitizedOutput {
    let mut changed = false;
    let mut output = String::with_capacity(input.len());
    let mut lines = input.split_inclusive('\n').peekable();

    while let Some(line) = lines.next() {
        if let Some(path) = sensitive_diff_block_path(line) {
            changed = true;
            output.push_str(format!("[REDACTED_SENSITIVE_FILE_DIFF path=\"{path}\"]").as_str());
            if line.ends_with('\n') || lines.peek().is_some() {
                output.push('\n');
            }
            while let Some(next) = lines.peek() {
                if next.starts_with("diff --git ") {
                    break;
                }
                let _ = lines.next();
            }
            continue;
        }

        output.push_str(line);
    }

    SanitizedOutput {
        text: output,
        changed,
    }
}

fn sensitive_diff_block_path(line: &str) -> Option<String> {
    if !line.starts_with("diff --git ") {
        return None;
    }

    let mut parts = line.split_whitespace();
    let _ = parts.next();
    let _ = parts.next();
    let lhs = parts.next()?;
    let rhs = parts.next()?;

    [lhs, rhs]
        .into_iter()
        .filter_map(diff_path_token_to_display_path)
        .find(|path| is_sensitive_path(Path::new(path)))
}

fn diff_path_token_to_display_path(token: &str) -> Option<String> {
    let trimmed = token.trim_matches('"');
    let trimmed = trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .unwrap_or(trimmed);
    (!trimmed.is_empty()).then(|| trimmed.replace('\\', "/"))
}

fn redact_secret_patterns(input: &str) -> SanitizedOutput {
    let mut changed = false;
    let mut updated = codex_secrets::redact_secrets(input.to_string());
    if updated != input {
        changed = true;
    }

    for (regex, replacement) in redaction_patterns() {
        let next = regex
            .replace_all(updated.as_str(), *replacement)
            .into_owned();
        if next != updated {
            changed = true;
            updated = next;
        }
    }

    SanitizedOutput {
        text: updated,
        changed,
    }
}

fn redaction_patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            (
                compile_regex(
                    r#"\b(OPENAI_API_KEY|MINIMAX_API_KEY|MINIMAX_CODING_PLAN_KEY)\s*=\s*[^\s"']+"#,
                ),
                "$1=[REDACTED_SECRET]",
            ),
            (
                compile_regex(r"(Authorization:\s*Bearer\s+)[A-Za-z0-9._-]+"),
                "${1}[REDACTED_SECRET]",
            ),
            (
                compile_regex(
                    r#"(\b(?:api[_-]?key|token|secret|password)\b\s*[:=]\s*)(?:"[^"]+"|'[^']+'|[^\s"']+)"#,
                ),
                "${1}[REDACTED_SECRET]",
            ),
            (
                compile_regex(r"\bsk-[A-Za-z0-9_-]{8,}\b"),
                REDACTED_SECRET_PLACEHOLDER,
            ),
        ]
    })
}

fn compile_regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => panic!("invalid sensitive-output regex `{pattern}`: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_output_text;
    use super::sensitive_command_block;
    use super::sensitive_command_block_from_text;
    use pretty_assertions::assert_eq;

    const FAKE_SECRET: &str = "sk-test-secret-should-not-leak";

    #[test]
    fn redacts_secret_patterns() {
        let output = sanitize_output_text(format!("OPENAI_API_KEY={FAKE_SECRET}").as_str());

        assert!(!output.text.contains(FAKE_SECRET));
        assert!(output.text.contains("[REDACTED_SECRET]"));
        assert!(output.changed);
    }

    #[test]
    fn redacts_sensitive_diff_blocks() {
        let input = format!(
            "diff --git a/.env b/.env\n--- a/.env\n+++ b/.env\n@@\n-OPENAI_API_KEY={FAKE_SECRET}\n+OPENAI_API_KEY=new-value\n"
        );

        let output = sanitize_output_text(input.as_str());

        assert!(!output.text.contains(FAKE_SECRET));
        assert!(
            output
                .text
                .contains("[REDACTED_SENSITIVE_FILE_DIFF path=\".env\"]")
        );
        assert!(output.changed);
    }

    #[test]
    fn preserves_non_sensitive_diff_blocks_in_mixed_output() {
        let input = format!(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 {{ a - b }}\n+pub fn add(a: i32, b: i32) -> i32 {{ a + b }}\n\
diff --git a/.env b/.env\n--- a/.env\n+++ b/.env\n@@\n-OPENAI_API_KEY={FAKE_SECRET}\n+OPENAI_API_KEY=changed\n"
        );

        let output = sanitize_output_text(input.as_str());

        assert!(output.text.contains("diff --git a/src/lib.rs b/src/lib.rs"));
        assert!(output.text.contains("a + b"));
        assert!(
            output
                .text
                .contains("[REDACTED_SENSITIVE_FILE_DIFF path=\".env\"]")
        );
        assert!(!output.text.contains(FAKE_SECRET));
    }

    #[test]
    fn leaves_normal_output_unchanged() {
        let cargo_output = "test result: ok. 3 passed; 0 failed;";
        let diff_output = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n-1\n+2\n";
        let build_output = "Compiling codex-core v0.0.0 (target/debug/deps/codex_core)\n";
        let summary_output = r#"{"context_summary":{"included_files":[{"path":"src/lib.rs"}]},"diagnostics":["kept target/debug output visible"]}"#;

        assert_eq!(
            sanitize_output_text(cargo_output),
            super::SanitizedOutput {
                text: cargo_output.to_string(),
                changed: false,
            }
        );
        assert_eq!(
            sanitize_output_text(diff_output),
            super::SanitizedOutput {
                text: diff_output.to_string(),
                changed: false,
            }
        );
        assert_eq!(
            sanitize_output_text(build_output),
            super::SanitizedOutput {
                text: build_output.to_string(),
                changed: false,
            }
        );
        assert_eq!(
            sanitize_output_text(summary_output),
            super::SanitizedOutput {
                text: summary_output.to_string(),
                changed: false,
            }
        );
    }

    #[test]
    fn blocks_explicit_sensitive_commands() {
        let blocked = sensitive_command_block(&["cat".to_string(), ".env".to_string()])
            .expect("cat .env should be blocked");
        assert_eq!(
            blocked.message,
            "blocked: command would expose sensitive file contents (.env). Use non-sensitive context or ask the user."
        );

        assert_eq!(
            sensitive_command_block_from_text("cat .env")
                .expect("stdin command should be blocked")
                .path,
            ".env"
        );

        assert_eq!(
            sensitive_command_block(&[
                "git".to_string(),
                "status".to_string(),
                "--short".to_string()
            ]),
            None
        );
        assert_eq!(
            sensitive_command_block(&["cargo".to_string(), "test".to_string()]),
            None
        );
    }

    #[test]
    fn blocks_common_sensitive_file_dump_commands() {
        let cases = [
            (vec!["cat", "\".env\""], ".env"),
            (vec!["cat", "./.env"], "./.env"),
            (vec!["head", ".env"], ".env"),
            (vec!["tail", ".env"], ".env"),
            (vec!["sed", "-n", "1,20p", ".env"], ".env"),
            (vec!["awk", "{print}", ".env"], ".env"),
            (vec!["grep", ".", ".env"], ".env"),
            (vec!["rg", ".", ".env"], ".env"),
            (vec!["xxd", ".env"], ".env"),
            (vec!["base64", ".env"], ".env"),
            (vec!["od", "-c", ".env"], ".env"),
            (vec!["strings", ".env"], ".env"),
            (vec!["git", "diff", "--", ".env"], ".env"),
            (vec!["git", "log", "-p", "--", ".env"], ".env"),
            (vec!["git", "show", "HEAD:.env"], ".env"),
            (vec!["git", "show", "HEAD:./.env"], "./.env"),
            (vec!["git", "show", "HEAD:path/to/.env"], "path/to/.env"),
            (vec!["git", "show", "main:.env"], ".env"),
            (vec!["git", "show", "abc123:.env"], ".env"),
            (vec!["git", "show", "HEAD:auth.json"], "auth.json"),
            (vec!["git", "show", "HEAD:secrets/id_rsa"], "secrets/id_rsa"),
            (vec!["bash", "-lc", "cat .env"], ".env"),
            (vec!["bash", "-lc", "git show HEAD:.env"], ".env"),
            (vec!["sh", "-c", "git diff -- .env"], ".env"),
            (vec!["bash", "-lc", "bash -lc 'cat .env'"], ".env"),
            (vec!["bash", "-lc", "sh -c 'git diff -- .env'"], ".env"),
        ];

        for (command, expected_path) in cases {
            let command = command
                .into_iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>();
            let blocked = sensitive_command_block(&command)
                .unwrap_or_else(|| panic!("command should be blocked: {command:?}"));
            assert_eq!(blocked.path, expected_path);
            assert_eq!(
                blocked.message,
                format!(
                    "blocked: command would expose sensitive file contents ({expected_path}). Use non-sensitive context or ask the user."
                )
            );
        }
    }

    #[test]
    fn redacts_common_non_unified_secret_patterns() {
        let input = format!(
            "OPENAI_API_KEY={FAKE_SECRET}\nAuthorization: Bearer {FAKE_SECRET}\ntoken = \"{FAKE_SECRET}\"\nsecret: {FAKE_SECRET}\n"
        );

        let output = sanitize_output_text(input.as_str());

        assert!(!output.text.contains(FAKE_SECRET));
        assert!(output.text.contains("OPENAI_API_KEY=[REDACTED_SECRET]"));
        assert!(
            output
                .text
                .contains("Authorization: Bearer [REDACTED_SECRET]")
        );
        assert!(output.text.contains("token = [REDACTED_SECRET]"));
        assert!(output.text.contains("secret: [REDACTED_SECRET]"));
        assert!(output.changed);
    }
}
