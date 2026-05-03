use crate::client_common::Prompt;
use codex_api::ResponseEvent;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::fmt::Write;
use std::path::Path;

mod context_packer;
mod git_status;
mod response_parser;
#[cfg(test)]
#[path = "minimax_delegate/response_parser_tests.rs"]
mod response_parser_tests;
use context_packer::ContextPack;
use context_packer::ContextPackRequest;
use context_packer::ContextPacker;
use context_packer::DEFAULT_CONTEXT_FILE_MAX_BYTES;

pub const DELEGATE_TO_MINIMAX_TOOL_NAME: &str = "delegate_to_minimax";
pub const CONTEXT_FILES_MAX_BYTES: usize = 32 * 1024;

const MINIMAX_DELEGATE_MODEL: &str = "MiniMax-M2.7";
const CLARIFY_PREFIX: &str = "CLARIFY:";

const DELEGATE_TO_MINIMAX_DEVELOPER_INSTRUCTIONS: &str = r#"You have access to delegate_to_minimax, a tool that delegates mechanical implementation tasks to MiniMax-M2.7, a faster cheaper worker model.

USE IT WHEN:
- Implementing a clearly-specified function (you know signature, inputs, outputs, expected behavior)
- Writing or updating unit tests for code you've already analyzed
- Mechanical refactoring (rename, extract method, format conversion)
- Code translation between languages with clear mapping
- Repetitive boilerplate (CRUD endpoints, similar test cases)

DO NOT USE IT WHEN:
- The task requires architectural decisions
- Debugging complex issues without clear root cause
- Security/auth implementation
- Integration with new external services or SDKs
- The user's request is ambiguous or open-ended
- You need to read and understand existing code before designing the change

WORKFLOW:
1. Analyze the user's request and your existing context
2. Decide if the task fits the USE IT WHEN criteria
3. If yes: call delegate_to_minimax with:
   - task_description: a clear, complete description
   - acceptance_criteria: specific requirements the output must satisfy
   - context_files: the most relevant files when you already know which code the worker should edit or test against; the tool will safely inline, redact, and truncate them as needed
   - include_modified_files: true only when the current worktree is directly relevant and you want the tool to safely inline a small bounded set of git-modified files using the same denylist, redaction, dedupe, and budget rules
     - Good uses: continuing, fixing, or reviewing code that is likely already modified locally, especially when you do not yet know the exact context files to attach
     - Do not use it for secrets, credentials, auth, tokens, or other sensitive local data; global architecture or repo-wide reasoning; when the user does not want to share local context with MiniMax; or when extra local context is unnecessary
4. The worker returns structured JSON:
   - completed: `{"status":"completed","format":"apply_patch"|"unified_diff","summary":"...","patch":"...","diagnostics":["..."]}`
   - clarify: `{"status":"clarify","question":"...","diagnostics":["..."]}`
   - invalid: `{"status":"invalid","error":"...","diagnostics":["..."]}`
   - The tool may also attach optional `context_summary` metadata describing which local context files were actually included and how many candidate files were omitted
5. Evaluate that output before applying it:
   - When `include_modified_files=true`, review `context_summary` and `diagnostics` before applying anything
   - If `status=completed`: review the patch before applying it, then apply it yourself with the normal Codex tools only if it looks correct
   - If `status=clarify`: do not apply anything; ask the user or gather the missing file/context, then retry only after you have new information
   - If `status=invalid`: do not apply anything; if the error is invalid format you may retry once with stricter JSON-only instructions and explicitly request `patch_lines` when JSON escaping is the likely failure mode
   - If `status=invalid` reports `patch_not_applicable`, retry only after refreshing `context_files`, `include_modified_files`, or the task description with exact current file context
   - Do not read, diff, cat, print, or otherwise expose sensitive files such as `.env`, `auth.json`, private keys, or credential files
   - If required files were omitted, attach them explicitly on retry; if sensitive files were denied, do not try to force them into MiniMax
   - If a command was blocked for a sensitive path, do not try to evade the guardrail with another command; rely on `context_summary`/`diagnostics` or ask the user for abstract guidance
   - Avoid indefinite retry loops; if one retry does not fix the issue, either gather better context or do the work yourself

The user may see the worker output in tool history. You are responsible for evaluating quality before applying it."#;

const MINIMAX_DELEGATE_SYSTEM_PROMPT: &str = r#"You are MiniMax-M2.7, a delegated coding worker for Codrex.

You are only handling bounded mechanical implementation work delegated by a primary model.
Complete the requested implementation directly when the task is clear.

Return exactly one JSON object and nothing else.

Allowed responses:
{"status":"completed","format":"apply_patch","summary":"<brief summary>","patch":"*** Begin Patch\\n...\\n*** End Patch","diagnostics":["optional diagnostics"]}
{"status":"completed","format":"apply_patch","summary":"<brief summary>","patch_lines":["*** Begin Patch","...","*** End Patch"],"diagnostics":["optional diagnostics"]}
{"status":"clarify","question":"<question>","diagnostics":["optional diagnostics"]}
{"status":"invalid","error":"<brief reason>","diagnostics":["optional diagnostics"]}

Rules:
- Use only the provided context.
- Some context files may have been included because they are currently modified in git; treat them as local-state hints, not proof that you saw the whole repository.
- Do not assume you saw the entire repository.
- Return only one JSON object and nothing else.
- Do not use markdown fences.
- Do not use shell commands.
- Do not emit `apply_patch <<EOF`, `apply_patch <<'PATCH'`, or other heredoc wrappers.
- Do not explain anything outside the JSON object.
- Do not invent files that are not shown, unless creating a new file is explicitly necessary to complete the requested change.
- If critical context is missing, truncated, or ambiguous, respond with `status":"clarify"` instead of guessing.
- Use `status":"invalid"` when the request or available context is not actionable as given and needs the supervisor to retry differently.
- For `status":"completed"`, return `format":"apply_patch"` with an exact apply_patch patch.
- The patch must start with `*** Begin Patch` and end with `*** End Patch`.
- Use `*** Update File: path` when modifying an existing file.
- Copy old lines exactly from the provided context when writing `-` lines.
- Do not use `...`, placeholders, or line numbers inside hunks.
- If JSON escaping is awkward, use `patch_lines` instead of an invalid multiline JSON string.
- If you cannot build an applicable patch from the provided context, return `status":"clarify"` or `status":"invalid"` instead of inventing a patch.
- The summary must be brief and factual.
- Do not invent missing requirements, APIs, or symbols.
- Do not describe approval gates, terminal commands, or filesystem mutations.
- Example valid response:
{"status":"completed","format":"apply_patch","summary":"Fix add implementation.","patch":"*** Begin Patch\\n*** Update File: src/lib.rs\\n@@\\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\\n*** End Patch","diagnostics":[]}"#;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DelegateToMinimaxRequest {
    pub task_description: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub context_files: Vec<String>,
    #[serde(default)]
    pub include_modified_files: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MiniMaxDelegationStatus {
    Completed,
    Clarify,
    Invalid,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPatchFormat {
    ApplyPatch,
    UnifiedDiff,
}

impl WorkerPatchFormat {
    fn as_str(&self) -> &'static str {
        match self {
            Self::ApplyPatch => "apply_patch",
            Self::UnifiedDiff => "unified_diff",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MiniMaxContextFileSource {
    ExplicitFile,
    ExplicitSnippet,
    TaskMention,
    GitModified,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct MiniMaxContextFile {
    pub path: String,
    pub source: MiniMaxContextFileSource,
    pub truncated: bool,
    pub redacted: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct MiniMaxContextSummary {
    pub included_files: Vec<MiniMaxContextFile>,
    pub omitted_count: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct MiniMaxDelegationResult {
    pub status: MiniMaxDelegationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<WorkerPatchFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_summary: Option<MiniMaxContextSummary>,
}

pub type DelegateToMinimaxResponse = MiniMaxDelegationResult;

impl MiniMaxDelegationResult {
    fn completed(
        format: WorkerPatchFormat,
        summary: String,
        patch: String,
        diagnostics: Vec<String>,
        context_summary: Option<MiniMaxContextSummary>,
    ) -> Self {
        Self {
            status: MiniMaxDelegationStatus::Completed,
            format: Some(format),
            summary: Some(summary),
            patch: Some(patch),
            question: None,
            error: None,
            diagnostics,
            context_summary,
        }
    }

    fn clarify(
        question: String,
        diagnostics: Vec<String>,
        context_summary: Option<MiniMaxContextSummary>,
    ) -> Self {
        Self {
            status: MiniMaxDelegationStatus::Clarify,
            format: None,
            summary: None,
            patch: None,
            question: Some(question),
            error: None,
            diagnostics,
            context_summary,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::invalid_with_context_summary(message.into(), Vec::new(), None)
    }

    pub fn invalid_with_diagnostics(message: String, diagnostics: Vec<String>) -> Self {
        Self::invalid_with_context_summary(message, diagnostics, None)
    }

    fn invalid_with_context_summary(
        message: String,
        diagnostics: Vec<String>,
        context_summary: Option<MiniMaxContextSummary>,
    ) -> Self {
        Self {
            status: MiniMaxDelegationStatus::Invalid,
            format: None,
            summary: None,
            patch: None,
            question: None,
            error: Some(message),
            diagnostics,
            context_summary,
        }
    }
}

pub fn delegate_to_minimax_dynamic_tool() -> DynamicToolSpec {
    DynamicToolSpec {
        namespace: None,
        name: DELEGATE_TO_MINIMAX_TOOL_NAME.to_string(),
        description: "Delegate a bounded implementation task to MiniMax-M2.7. Returns JSON with status plus a reviewable patch candidate."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_description": {
                    "type": "string",
                    "description": "Clear description of what to implement"
                },
                "acceptance_criteria": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Specific criteria the output must meet (e.g. 'must use only stdlib', 'function signature: parse(s: str) -> datetime')"
                },
                "context_files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file paths whose content gives the worker the exact code context it should edit or test against"
                },
                "include_modified_files": {
                    "type": "boolean",
                    "description": "Optional opt-in to also attach a small bounded set of modified tracked files from git status using the same denylist, redaction, dedupe, and budget rules"
                }
            },
            "required": ["task_description"]
        }),
        defer_loading: false,
    }
}

pub fn is_delegate_to_minimax_available(codex_home: &Path) -> bool {
    crate::minimax_adapter::has_minimax_credentials(codex_home)
}

pub(crate) fn delegate_to_minimax_developer_instructions(
    dynamic_tools: &[DynamicToolSpec],
) -> Option<String> {
    dynamic_tools
        .iter()
        .any(|tool| tool.name == DELEGATE_TO_MINIMAX_TOOL_NAME)
        .then(|| DELEGATE_TO_MINIMAX_DEVELOPER_INSTRUCTIONS.to_string())
}

pub async fn delegate_to_minimax(
    request: DelegateToMinimaxRequest,
    cwd: &Path,
    codex_home: &Path,
) -> CodexResult<DelegateToMinimaxResponse> {
    let context_packer =
        ContextPacker::new(cwd, CONTEXT_FILES_MAX_BYTES, DEFAULT_CONTEXT_FILE_MAX_BYTES);
    let context_pack = if request.include_modified_files {
        context_packer.pack_with_request(ContextPackRequest {
            explicit_files: &request.context_files,
            explicit_snippets: &[],
            task_text: request.task_description.as_str(),
            include_modified_files: true,
        })
    } else {
        context_packer.pack(
            &request.context_files,
            &[],
            request.task_description.as_str(),
        )
    };

    let mut prompt = Prompt {
        base_instructions: BaseInstructions {
            text: MINIMAX_DELEGATE_SYSTEM_PROMPT.to_string(),
        },
        ..Prompt::default()
    };
    prompt.input.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: render_delegate_request(&request, &context_pack),
        }],
        phase: None,
    });

    let mut provider = ModelProviderInfo::create_minimax_provider();
    provider.base_url = None;
    let mut stream = crate::minimax_adapter::stream_chat_completions_with_codex_home(
        &provider,
        &prompt,
        MINIMAX_DELEGATE_MODEL,
        reqwest::Client::new(),
        codex_home,
    )
    .await?;

    let output = collect_stream_text(&mut stream).await?;
    let context_summary = build_context_summary(&context_pack);
    let result = parse_delegate_output(
        output.as_str(),
        &context_pack.diagnostics.messages,
        context_summary,
    );
    Ok(validate_delegate_result_against_worktree(result, cwd).await)
}

fn render_delegate_request(
    request: &DelegateToMinimaxRequest,
    context_pack: &ContextPack,
) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "Task description:");
    let _ = writeln!(text, "{}", request.task_description);

    if !request.acceptance_criteria.is_empty() {
        let _ = writeln!(text);
        let _ = writeln!(text, "Acceptance criteria:");
        for criterion in &request.acceptance_criteria {
            let _ = writeln!(text, "- {criterion}");
        }
    }

    if !context_pack.git_modified_paths.is_empty() {
        let _ = writeln!(text);
        let _ = writeln!(text, "Git-modified context:");
        let _ = writeln!(
            text,
            "- The following files were included because they are currently modified in git: {}.",
            context_pack.git_modified_paths.join(", ")
        );
        let _ = writeln!(
            text,
            "- Do not assume this is a complete view of every changed file in the repository."
        );
    }

    let _ = writeln!(text);
    let _ = writeln!(text, "<context>");
    for context_file in &context_pack.files {
        let truncated = if context_file.truncated {
            "true"
        } else {
            "false"
        };
        let _ = writeln!(
            text,
            r#"<file path="{}" truncated="{truncated}">"#,
            context_file.path
        );
        let _ = write!(text, "{}", context_file.content);
        if !context_file.content.ends_with('\n') {
            let _ = writeln!(text);
        }
        let _ = writeln!(text, "</file>");
        let _ = writeln!(text);
    }
    let _ = writeln!(text, "</context>");

    if context_pack.truncated || !context_pack.diagnostics.messages.is_empty() {
        let _ = writeln!(text);
        let _ = writeln!(text, "Context note:");
        let _ = writeln!(
            text,
            "- Some requested context was omitted, truncated, or redacted by safety and budget rules."
        );
        let _ = writeln!(
            text,
            "- If essential context is missing, respond with `status\":\"clarify\"` instead of guessing."
        );
    }

    let _ = writeln!(text);
    let _ = writeln!(text, "Return contract:");
    let _ = writeln!(text, "- Return exactly one JSON object and nothing else.");
    let _ = writeln!(
        text,
        "- If the task is clear, return completed JSON with `format: \"apply_patch\"` plus either `patch` with escaped `\\n` newlines or `patch_lines` as an array of lines."
    );
    let _ = writeln!(
        text,
        "- If essential context is missing or ambiguous, return: {{\"status\":\"clarify\",\"question\":\"...\",\"diagnostics\":[\"...\"]}}."
    );
    let _ = writeln!(
        text,
        "- If the request is not actionable as given, return: {{\"status\":\"invalid\",\"error\":\"...\",\"diagnostics\":[\"...\"]}}."
    );
    let _ = writeln!(text, "- Use only the context provided above.");
    let _ = writeln!(
        text,
        "- Do not invent files that are not shown above unless creating a new file is explicitly necessary."
    );
    let _ = writeln!(
        text,
        "- Do not include markdown fences, prose, or explanations outside the JSON object."
    );
    let _ = writeln!(
        text,
        "- Do not use shell commands or heredoc wrappers such as `apply_patch <<'PATCH' ... PATCH`."
    );
    let _ = writeln!(
        text,
        "- For existing files, use `*** Update File: path`, copy `-` lines exactly from the context above, and do not use `...`, placeholders, or line numbers inside hunks."
    );
    let _ = writeln!(
        text,
        "- If the provided context is not enough to build an applicable patch, return `status\":\"clarify\"` or `status\":\"invalid\"` instead of guessing."
    );
    text
}

fn build_context_summary(context_pack: &ContextPack) -> Option<MiniMaxContextSummary> {
    let included_files = context_pack
        .files
        .iter()
        .map(|file| MiniMaxContextFile {
            path: file.path.clone(),
            source: file.source.clone(),
            truncated: file.truncated,
            redacted: file.redacted,
        })
        .collect::<Vec<_>>();
    let omitted_count = context_pack
        .diagnostics
        .messages
        .iter()
        .filter(|message| message.starts_with("omitted "))
        .count();

    if included_files.is_empty() && omitted_count == 0 {
        None
    } else {
        Some(MiniMaxContextSummary {
            included_files,
            omitted_count,
        })
    }
}

fn parse_delegate_output(
    output: &str,
    context_diagnostics: &[String],
    context_summary: Option<MiniMaxContextSummary>,
) -> DelegateToMinimaxResponse {
    response_parser::parse_delegate_output(output, context_diagnostics, context_summary)
}

async fn validate_delegate_result_against_worktree(
    result: DelegateToMinimaxResponse,
    cwd: &Path,
) -> DelegateToMinimaxResponse {
    response_parser::validate_delegate_result_against_worktree(result, cwd).await
}

async fn collect_stream_text(
    stream: &mut crate::client_common::ResponseStream,
) -> CodexResult<String> {
    let mut deltas = String::new();
    let mut final_message_text = String::new();

    while let Some(item) = stream.next().await {
        match item? {
            ResponseEvent::OutputTextDelta(delta) => deltas.push_str(&delta),
            ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }) => {
                for item in content {
                    if let ContentItem::OutputText { text } = item {
                        final_message_text.push_str(&text);
                    }
                }
            }
            _ => {}
        }
    }

    if deltas.trim().is_empty() {
        Ok(final_message_text)
    } else {
        Ok(deltas)
    }
}

#[cfg(test)]
mod tests {
    use codex_config::types::AuthCredentialsStoreMode;
    use codex_login::ProviderCredentials;
    use codex_login::save_provider_credentials;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use serial_test::serial;
    use std::ffi::OsStr;
    use std::ffi::OsString;
    use tempfile::TempDir;
    use wiremock::MockServer;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::CONTEXT_FILES_MAX_BYTES;
    use super::DEFAULT_CONTEXT_FILE_MAX_BYTES;
    use super::DelegateToMinimaxRequest;
    use super::MINIMAX_DELEGATE_SYSTEM_PROMPT;
    use super::MiniMaxContextFile;
    use super::MiniMaxContextFileSource;
    use super::MiniMaxContextSummary;
    use super::MiniMaxDelegationResult;
    use super::MiniMaxDelegationStatus;
    use super::WorkerPatchFormat;
    use super::delegate_to_minimax;
    use super::delegate_to_minimax_developer_instructions;
    use super::delegate_to_minimax_dynamic_tool;
    use super::is_delegate_to_minimax_available;
    use super::parse_delegate_output;

    fn nonstreaming_chat_response_body(content: &str) -> String {
        let content = serde_json::to_string(content).expect("serialize content");
        format!(
            "data: {{\"id\":\"resp-test\",\"object\":\"chat.completion.chunk\",\
             \"choices\":[{{\"index\":0,\"delta\":{{\"content\":{content}}}}}]}}\n\n\
             data: {{\"id\":\"resp-test\",\"choices\":[{{\"index\":0,\"finish_reason\":\"stop\",\
             \"delta\":{{}}}}],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":3,\
             \"total_tokens\":8}}}}\n\n\
             data: [DONE]\n\n"
        )
    }

    fn write_chatgpt_auth_json(codex_home: &TempDir) {
        codex_login::save_auth(
            codex_home.path(),
            &codex_login::AuthDotJson {
                auth_mode: Some(codex_login::AuthMode::Chatgpt),
                openai_api_key: None,
                tokens: Some(codex_login::TokenData {
                    id_token: codex_login::token_data::IdTokenInfo {
                        raw_jwt: "e30.e30.e30".to_string(),
                        ..codex_login::token_data::IdTokenInfo::default()
                    },
                    access_token: "chatgpt-access-token".to_string(),
                    refresh_token: "chatgpt-refresh-token".to_string(),
                    account_id: None,
                }),
                last_refresh: None,
                agent_identity: None,
            },
            AuthCredentialsStoreMode::File,
        )
        .expect("write auth.json");
    }

    fn save_minimax_credentials(codex_home: &TempDir) {
        save_provider_credentials(
            codex_home.path(),
            AuthCredentialsStoreMode::File,
            "minimax",
            ProviderCredentials {
                api_key: "minimax-file-token".to_string(),
                kind: Some("coding_plan".to_string()),
                last_verified: None,
            },
        )
        .expect("save minimax credentials");
    }

    fn sample_context_summary() -> MiniMaxContextSummary {
        MiniMaxContextSummary {
            included_files: vec![MiniMaxContextFile {
                path: "src/lib.rs".to_string(),
                source: MiniMaxContextFileSource::GitModified,
                truncated: false,
                redacted: false,
            }],
            omitted_count: 1,
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &OsStr) -> Self {
            let original = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }

        fn clear(key: &'static str) -> Self {
            let original = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.original {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn parse_delegate_output_returns_completed_patch_candidate() {
        let context_summary = sample_context_summary();
        let result = parse_delegate_output(
            r#"{"status":"completed","format":"apply_patch","summary":"Implement validate_email","patch":"*** Begin Patch\n*** Add File: validate_email.py\n+def validate_email(value: str) -> bool:\n+    return \"@\" in value\n*** End Patch","diagnostics":["worker checked existing helper signature"]}"#,
            &[],
            Some(context_summary.clone()),
        );

        assert_eq!(
            result,
            MiniMaxDelegationResult {
                status: MiniMaxDelegationStatus::Completed,
                format: Some(WorkerPatchFormat::ApplyPatch),
                summary: Some("Implement validate_email".to_string()),
                patch: Some(
                    "*** Begin Patch\n*** Add File: validate_email.py\n+def validate_email(value: str) -> bool:\n+    return \"@\" in value\n*** End Patch".to_string()
                ),
                question: None,
                error: None,
                diagnostics: vec!["worker checked existing helper signature".to_string()],
                context_summary: Some(context_summary),
            }
        );
    }

    #[test]
    fn parse_delegate_output_returns_json_clarify_response() {
        let context_summary = sample_context_summary();
        let result = parse_delegate_output(
            r#"{"status":"clarify","question":"should I preserve the helper signature?","diagnostics":["need existing helper file"]}"#,
            &["omitted .env: denied path".to_string()],
            Some(context_summary.clone()),
        );

        assert_eq!(
            result,
            MiniMaxDelegationResult {
                status: MiniMaxDelegationStatus::Clarify,
                format: None,
                summary: None,
                patch: None,
                question: Some("should I preserve the helper signature?".to_string()),
                error: None,
                diagnostics: vec![
                    "omitted .env: denied path".to_string(),
                    "need existing helper file".to_string(),
                ],
                context_summary: Some(context_summary),
            }
        );
    }

    #[test]
    fn parse_delegate_output_keeps_legacy_clarify_compatibility() {
        let result = parse_delegate_output(
            "CLARIFY: should I preserve the helper signature?",
            &[],
            None,
        );

        assert_eq!(
            result,
            MiniMaxDelegationResult {
                status: MiniMaxDelegationStatus::Clarify,
                format: None,
                summary: None,
                patch: None,
                question: Some("should I preserve the helper signature?".to_string()),
                error: None,
                diagnostics: Vec::new(),
                context_summary: None,
            }
        );
    }

    #[test]
    fn parse_delegate_output_returns_invalid_response() {
        let context_summary = sample_context_summary();
        let result = parse_delegate_output(
            r#"{"status":"invalid","error":"missing src/lib.rs context","diagnostics":["attach src/lib.rs"]}"#,
            &["context truncated: exceeded total budget".to_string()],
            Some(context_summary.clone()),
        );

        assert_eq!(
            result,
            MiniMaxDelegationResult {
                status: MiniMaxDelegationStatus::Invalid,
                format: None,
                summary: None,
                patch: None,
                question: None,
                error: Some("missing src/lib.rs context".to_string()),
                diagnostics: vec![
                    "context truncated: exceeded total budget".to_string(),
                    "attach src/lib.rs".to_string(),
                ],
                context_summary: Some(context_summary),
            }
        );
    }

    #[test]
    fn parse_delegate_output_rejects_prose_without_json() {
        let result = parse_delegate_output("I updated the helper and added tests.", &[], None);

        assert_eq!(
            result,
            MiniMaxDelegationResult {
                status: MiniMaxDelegationStatus::Invalid,
                format: None,
                summary: None,
                patch: None,
                question: None,
                error: Some(
                    "worker_response_not_json: expected JSON object with status completed/clarify/invalid"
                        .to_string(),
                ),
                diagnostics: Vec::new(),
                context_summary: None,
            }
        );
    }

    #[test]
    fn parse_delegate_output_without_context_summary_keeps_legacy_shape() {
        let result = parse_delegate_output(
            r#"{"status":"invalid","error":"missing context"}"#,
            &[],
            None,
        );

        assert_eq!(result.context_summary, None);
        assert!(
            serde_json::to_value(&result)
                .expect("serialize result")
                .get("context_summary")
                .is_none()
        );
    }

    #[tokio::test]
    #[serial(env_minimax_delegate)]
    async fn delegate_to_minimax_returns_clarify_response() {
        let server = MockServer::start().await;
        let body = nonstreaming_chat_response_body(
            r#"{"status":"clarify","question":"should I add property-based tests?","diagnostics":["need target test file"]}"#,
        );
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        save_minimax_credentials(&temp_dir);
        let minimax_base_url = format!("{}/v1", server.uri());
        let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

        let result = delegate_to_minimax(
            DelegateToMinimaxRequest {
                task_description: "Implement validate_email".to_string(),
                acceptance_criteria: vec!["Add 5 unit tests".to_string()],
                context_files: Vec::new(),
                include_modified_files: false,
            },
            temp_dir.path(),
            temp_dir.path(),
        )
        .await
        .expect("delegate call should succeed");

        assert_eq!(
            result,
            MiniMaxDelegationResult {
                status: MiniMaxDelegationStatus::Clarify,
                format: None,
                summary: None,
                patch: None,
                question: Some("should I add property-based tests?".to_string()),
                error: None,
                diagnostics: vec!["need target test file".to_string()],
                context_summary: None,
            }
        );
    }

    #[tokio::test]
    #[serial(env_minimax_delegate)]
    async fn delegate_to_minimax_returns_patch_candidate_with_truncation_diagnostic() {
        let server = MockServer::start().await;
        let body = nonstreaming_chat_response_body(
            r#"{"status":"completed","format":"apply_patch","summary":"Implement fizzbuzz","patch":"*** Begin Patch\n*** Add File: fizzbuzz.py\n+def fizzbuzz(n: int) -> list[str]:\n+    return []\n*** End Patch"}"#,
        );
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        save_minimax_credentials(&temp_dir);
        let context_path = temp_dir.path().join("big_context.py");
        std::fs::write(&context_path, "x".repeat(CONTEXT_FILES_MAX_BYTES + 128))
            .expect("write context file");
        let minimax_base_url = format!("{}/v1", server.uri());
        let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

        let result = delegate_to_minimax(
            DelegateToMinimaxRequest {
                task_description: "Implement fizzbuzz".to_string(),
                acceptance_criteria: vec!["Write 3 unit tests".to_string()],
                context_files: vec!["big_context.py".to_string()],
                include_modified_files: false,
            },
            temp_dir.path(),
            temp_dir.path(),
        )
        .await
        .expect("delegate call should succeed");

        assert_eq!(
            result,
            MiniMaxDelegationResult {
                status: MiniMaxDelegationStatus::Completed,
                format: Some(WorkerPatchFormat::ApplyPatch),
                summary: Some("Implement fizzbuzz".to_string()),
                patch: Some(
                    "*** Begin Patch\n*** Add File: fizzbuzz.py\n+def fizzbuzz(n: int) -> list[str]:\n+    return []\n*** End Patch".to_string()
                ),
                question: None,
                error: None,
                diagnostics: vec![format!(
                    "context file big_context.py truncated at {DEFAULT_CONTEXT_FILE_MAX_BYTES} bytes"
                )],
                context_summary: Some(MiniMaxContextSummary {
                    included_files: vec![MiniMaxContextFile {
                        path: "big_context.py".to_string(),
                        source: MiniMaxContextFileSource::ExplicitFile,
                        truncated: true,
                        redacted: false,
                    }],
                    omitted_count: 0,
                }),
            }
        );
    }

    #[test]
    fn delegate_to_minimax_is_available_when_chatgpt_and_minimax_auth_coexist() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        write_chatgpt_auth_json(&temp_dir);
        save_minimax_credentials(&temp_dir);

        assert!(is_delegate_to_minimax_available(temp_dir.path()));
    }

    #[test]
    #[serial(env_minimax_delegate)]
    fn delegate_to_minimax_is_available_with_minimax_api_key_env() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let _minimax_api_key = EnvVarGuard::set("MINIMAX_API_KEY", OsStr::new("test-key"));
        let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

        assert!(is_delegate_to_minimax_available(temp_dir.path()));
    }

    #[test]
    #[serial(env_minimax_delegate)]
    fn delegate_to_minimax_is_available_with_minimax_coding_plan_key_env() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
        let _minimax_coding_plan_key = EnvVarGuard::set(
            "MINIMAX_CODING_PLAN_KEY",
            OsStr::new("test-coding-plan-key"),
        );

        assert!(is_delegate_to_minimax_available(temp_dir.path()));
    }

    #[test]
    #[serial(env_minimax_delegate)]
    fn delegate_to_minimax_is_unavailable_without_credentials() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
        let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

        assert!(!is_delegate_to_minimax_available(temp_dir.path()));
    }

    #[test]
    fn delegate_to_minimax_dynamic_tool_is_visible_to_the_model() {
        let tool = delegate_to_minimax_dynamic_tool();

        assert_eq!(tool.name, "delegate_to_minimax");
        assert!(!tool.defer_loading);
        assert_eq!(tool.namespace, None);
        assert_eq!(tool.input_schema["required"], json!(["task_description"]));
        assert!(tool.input_schema["properties"]["context_files"].is_object());
        assert_eq!(
            tool.input_schema["properties"]["include_modified_files"]["type"],
            json!("boolean")
        );
        assert!(
            tool.input_schema["required"]
                .as_array()
                .is_some_and(|required| {
                    !required
                        .iter()
                        .any(|entry| entry == "include_modified_files")
                })
        );
    }

    #[test]
    fn delegate_to_minimax_prompt_is_only_added_when_tool_is_registered() {
        assert!(delegate_to_minimax_developer_instructions(&[]).is_none());
        let prompt =
            delegate_to_minimax_developer_instructions(&[delegate_to_minimax_dynamic_tool()])
                .expect("delegate tool should inject developer instructions");
        assert!(prompt.contains(r#""status":"clarify""#));
        assert!(prompt.contains(r#""status":"invalid""#));
        assert!(prompt.contains("Avoid indefinite retry loops"));
        assert!(prompt.contains("include_modified_files"));
        assert!(prompt.contains("secrets, credentials, auth, tokens"));
        assert!(prompt.contains("Do not read, diff, cat, print"));
        assert!(prompt.contains("patch_lines"));
    }

    #[test]
    fn delegate_to_minimax_request_defaults_include_modified_files_to_false() {
        let request: DelegateToMinimaxRequest = serde_json::from_value(json!({
            "task_description": "Implement validate_email",
            "acceptance_criteria": ["Add 5 unit tests"],
            "context_files": ["src/lib.rs"]
        }))
        .expect("deserialize delegate request");

        assert_eq!(
            request,
            DelegateToMinimaxRequest {
                task_description: "Implement validate_email".to_string(),
                acceptance_criteria: vec!["Add 5 unit tests".to_string()],
                context_files: vec!["src/lib.rs".to_string()],
                include_modified_files: false,
            }
        );
    }

    #[test]
    fn delegate_to_minimax_worker_prompt_mentions_git_modified_context() {
        assert!(MINIMAX_DELEGATE_SYSTEM_PROMPT.contains("modified in git"));
        assert!(
            MINIMAX_DELEGATE_SYSTEM_PROMPT.contains("Do not assume you saw the entire repository")
        );
        assert!(MINIMAX_DELEGATE_SYSTEM_PROMPT.contains("patch_lines"));
        assert!(MINIMAX_DELEGATE_SYSTEM_PROMPT.contains("Do not emit `apply_patch <<EOF`"));
    }
}
