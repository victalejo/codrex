use crate::client_common::Prompt;
use codex_api::ResponseEvent;
use codex_apply_patch::parse_patch;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::error::CodexErr;
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
   - context_files: the most relevant files when you already know which code the worker should edit or test against
4. The worker returns structured JSON:
   - completed: `{"status":"completed","format":"apply_patch"|"unified_diff","summary":"...","patch":"..."}`
   - clarify: `CLARIFY: <question>`
   - invalid worker output is returned to you as a structured error result
5. Evaluate that output before applying it:
   - Review the patch before applying it
   - If satisfactory: apply it yourself with the normal Codex tools
   - If unclear or wrong: refine the criteria and call the tool again, or do it yourself
   - If the worker responded with CLARIFY:, refine the task and retry

The user may see the worker output in tool history. You are responsible for evaluating quality before applying it."#;

const MINIMAX_DELEGATE_SYSTEM_PROMPT: &str = r#"You are MiniMax-M2.7, a delegated coding worker for Codrex.

You are only handling bounded mechanical implementation work delegated by a primary model.
Complete the requested implementation directly when the task is clear.

If the task is ambiguous or missing critical information needed to proceed safely, respond exactly with `CLARIFY: <question>`.
If the task is clear, respond with exactly one JSON object and nothing else:
{"status":"completed","format":"apply_patch","summary":"<brief summary>","patch":"*** Begin Patch\n...\n*** End Patch"}

Use `apply_patch` whenever possible. Only use `unified_diff` when `apply_patch` is not practical.
The summary must be brief and factual.
The patch must be valid for the declared format.
Do not invent missing requirements.
Do not invent files, APIs, or symbols when the provided context is insufficient.
Do not describe approval gates, terminal commands, or filesystem mutations.
Do not include markdown fences, prose, or explanations outside the required JSON object."#;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DelegateToMinimaxRequest {
    pub task_description: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub context_files: Vec<String>,
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
}

pub type DelegateToMinimaxResponse = MiniMaxDelegationResult;

impl MiniMaxDelegationResult {
    fn completed(
        format: WorkerPatchFormat,
        summary: String,
        patch: String,
        diagnostics: Vec<String>,
    ) -> Self {
        Self {
            status: MiniMaxDelegationStatus::Completed,
            format: Some(format),
            summary: Some(summary),
            patch: Some(patch),
            question: None,
            error: None,
            diagnostics,
        }
    }

    fn clarify(question: String, diagnostics: Vec<String>) -> Self {
        Self {
            status: MiniMaxDelegationStatus::Clarify,
            format: None,
            summary: None,
            patch: None,
            question: Some(question),
            error: None,
            diagnostics,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::invalid_with_diagnostics(message.into(), Vec::new())
    }

    pub fn invalid_with_diagnostics(message: String, diagnostics: Vec<String>) -> Self {
        Self {
            status: MiniMaxDelegationStatus::Invalid,
            format: None,
            summary: None,
            patch: None,
            question: None,
            error: Some(message),
            diagnostics,
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
    let (context_files, context_truncated) = load_context_files(cwd, &request.context_files)?;

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
            text: render_delegate_request(&request, &context_files, context_truncated),
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
    Ok(parse_delegate_output(output.as_str(), context_truncated))
}

struct ContextFileSnippet {
    path: String,
    content: String,
}

fn load_context_files(
    cwd: &Path,
    context_files: &[String],
) -> CodexResult<(Vec<ContextFileSnippet>, bool)> {
    let mut remaining = CONTEXT_FILES_MAX_BYTES;
    let mut truncated = false;
    let mut snippets = Vec::with_capacity(context_files.len());

    for (index, context_file) in context_files.iter().enumerate() {
        if remaining == 0 {
            truncated = true;
            break;
        }

        let resolved_path = resolve_context_path(cwd, context_file);
        let bytes = std::fs::read(&resolved_path).map_err(|err| {
            CodexErr::UnsupportedOperation(format!(
                "failed to read delegate context file `{context_file}`: {err}"
            ))
        })?;
        let content = String::from_utf8_lossy(&bytes);
        let truncated_content =
            codex_utils_string::take_bytes_at_char_boundary(content.as_ref(), remaining);
        if truncated_content.len() < content.len() {
            truncated = true;
        }
        remaining = remaining.saturating_sub(truncated_content.len());
        snippets.push(ContextFileSnippet {
            path: context_file.clone(),
            content: truncated_content.to_string(),
        });

        if remaining == 0 && index + 1 < context_files.len() {
            truncated = true;
            break;
        }
    }

    Ok((snippets, truncated))
}

fn resolve_context_path(cwd: &Path, context_file: &str) -> std::path::PathBuf {
    let path = Path::new(context_file);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn render_delegate_request(
    request: &DelegateToMinimaxRequest,
    context_files: &[ContextFileSnippet],
    context_truncated: bool,
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

    if !context_files.is_empty() {
        let _ = writeln!(text);
        let _ = writeln!(text, "Context files:");
        for snippet in context_files {
            let _ = writeln!(text, "=== {} ===", snippet.path);
            let _ = writeln!(text, "{}", snippet.content);
            if !snippet.content.ends_with('\n') {
                let _ = writeln!(text);
            }
        }
    }

    if context_truncated {
        let _ = writeln!(text);
        let _ = writeln!(
            text,
            "Note: context files were truncated to {CONTEXT_FILES_MAX_BYTES} bytes total."
        );
    }

    let _ = writeln!(text);
    let _ = writeln!(text, "Return contract:");
    let _ = writeln!(
        text,
        "- If the task is clear, return exactly one JSON object with fields `status`, `format`, `summary`, and `patch`."
    );
    let _ = writeln!(
        text,
        "- Use `format: \"apply_patch\"` whenever possible. Only use `\"unified_diff\"` when needed."
    );
    let _ = writeln!(
        text,
        "- Do not include markdown fences or any prose outside the JSON object."
    );
    let _ = writeln!(
        text,
        "- If you need clarification before proceeding, respond exactly with `CLARIFY: <question>`."
    );
    text
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkerPatchCandidate {
    status: MiniMaxDelegationStatus,
    format: WorkerPatchFormat,
    summary: String,
    patch: String,
}

fn parse_delegate_output(output: &str, context_truncated: bool) -> DelegateToMinimaxResponse {
    let trimmed = output.trim();
    let diagnostics = context_truncation_diagnostics(context_truncated);

    if trimmed.is_empty() {
        return MiniMaxDelegationResult::invalid_with_diagnostics(
            "MiniMax returned no output for delegate_to_minimax.".to_string(),
            diagnostics,
        );
    }

    if let Some(question) = trimmed.strip_prefix(CLARIFY_PREFIX) {
        return MiniMaxDelegationResult::clarify(question.trim().to_string(), diagnostics);
    }

    let candidate = match serde_json::from_str::<RawWorkerPatchCandidate>(trimmed) {
        Ok(candidate) => candidate,
        Err(_) => {
            if let Some(format) = detect_patch_format(trimmed) {
                return MiniMaxDelegationResult::invalid_with_diagnostics(
                    format!(
                        "MiniMax returned a raw {} patch instead of the required JSON object.",
                        format.as_str()
                    ),
                    diagnostics,
                );
            }

            return MiniMaxDelegationResult::invalid_with_diagnostics(
                "MiniMax returned invalid output: expected JSON patch candidate or `CLARIFY: <question>`."
                    .to_string(),
                diagnostics,
            );
        }
    };

    if candidate.status != MiniMaxDelegationStatus::Completed {
        return MiniMaxDelegationResult::invalid_with_diagnostics(
            format!(
                "MiniMax returned status `{}` in JSON, but only `completed` is valid for patch candidates.",
                candidate_status_name(&candidate.status)
            ),
            diagnostics,
        );
    }

    let summary = candidate.summary.trim().to_string();
    if summary.is_empty() {
        return MiniMaxDelegationResult::invalid_with_diagnostics(
            "MiniMax returned a patch candidate without a summary.".to_string(),
            diagnostics,
        );
    }

    let patch = candidate.patch.trim().to_string();
    if patch.is_empty() {
        return MiniMaxDelegationResult::invalid_with_diagnostics(
            "MiniMax returned a patch candidate without patch content.".to_string(),
            diagnostics,
        );
    }

    if let Err(error) = validate_patch_candidate(&candidate.format, patch.as_str()) {
        let mut diagnostics = diagnostics;
        diagnostics.push(error);
        return MiniMaxDelegationResult::invalid_with_diagnostics(
            format!(
                "MiniMax returned an invalid {} patch candidate.",
                candidate.format.as_str()
            ),
            diagnostics,
        );
    }

    MiniMaxDelegationResult::completed(candidate.format, summary, patch, diagnostics)
}

fn validate_patch_candidate(format: &WorkerPatchFormat, patch: &str) -> Result<(), String> {
    match format {
        WorkerPatchFormat::ApplyPatch => {
            let parsed = parse_patch(patch)
                .map_err(|err| format!("apply_patch validation failed: {err}"))?;
            if parsed.hunks.is_empty() {
                return Err(
                    "apply_patch validation failed: patch contained no file operations."
                        .to_string(),
                );
            }
            Ok(())
        }
        WorkerPatchFormat::UnifiedDiff => {
            let has_diff_header = patch.contains("diff --git");
            let has_file_headers = patch.contains("--- ") && patch.contains("+++ ");
            let has_hunk = patch.contains("@@");
            if has_diff_header || has_file_headers || has_hunk {
                Ok(())
            } else {
                Err("unified diff validation failed: expected `diff --git`, `---`/`+++`, or `@@` markers.".to_string())
            }
        }
    }
}

fn detect_patch_format(output: &str) -> Option<WorkerPatchFormat> {
    if let Ok(parsed) = parse_patch(output)
        && !parsed.hunks.is_empty()
    {
        return Some(WorkerPatchFormat::ApplyPatch);
    }

    let has_diff_header = output.contains("diff --git");
    let has_file_headers = output.contains("--- ") && output.contains("+++ ");
    let has_hunk = output.contains("@@");
    if has_diff_header || has_file_headers || has_hunk {
        Some(WorkerPatchFormat::UnifiedDiff)
    } else {
        None
    }
}

fn context_truncation_diagnostics(context_truncated: bool) -> Vec<String> {
    if context_truncated {
        vec![format!(
            "Context files were truncated to {CONTEXT_FILES_MAX_BYTES} bytes before delegation."
        )]
    } else {
        Vec::new()
    }
}

fn candidate_status_name(status: &MiniMaxDelegationStatus) -> &'static str {
    match status {
        MiniMaxDelegationStatus::Completed => "completed",
        MiniMaxDelegationStatus::Clarify => "clarify",
        MiniMaxDelegationStatus::Invalid => "invalid",
    }
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
    use serial_test::serial;
    use std::ffi::OsStr;
    use std::ffi::OsString;
    use tempfile::TempDir;
    use wiremock::MockServer;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::CONTEXT_FILES_MAX_BYTES;
    use super::DelegateToMinimaxRequest;
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
        let result = parse_delegate_output(
            r#"{"status":"completed","format":"apply_patch","summary":"Implement validate_email","patch":"*** Begin Patch\n*** Add File: validate_email.py\n+def validate_email(value: str) -> bool:\n+    return \"@\" in value\n*** End Patch"}"#,
            /*context_truncated*/ false,
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
                diagnostics: Vec::new(),
            }
        );
    }

    #[test]
    fn parse_delegate_output_returns_clarify_response() {
        let result = parse_delegate_output(
            "CLARIFY: should I preserve the helper signature?",
            /*context_truncated*/ false,
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
            }
        );
    }

    #[test]
    fn parse_delegate_output_rejects_prose_without_patch() {
        let result = parse_delegate_output(
            "I updated the helper and added tests.",
            /*context_truncated*/ false,
        );

        assert_eq!(
            result,
            MiniMaxDelegationResult {
                status: MiniMaxDelegationStatus::Invalid,
                format: None,
                summary: None,
                patch: None,
                question: None,
                error: Some(
                    "MiniMax returned invalid output: expected JSON patch candidate or `CLARIFY: <question>`."
                        .to_string()
                ),
                diagnostics: Vec::new(),
            }
        );
    }

    #[tokio::test]
    #[serial(env_minimax_delegate)]
    async fn delegate_to_minimax_returns_clarify_response() {
        let server = MockServer::start().await;
        let body = nonstreaming_chat_response_body("CLARIFY: should I add property-based tests?");
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
                diagnostics: Vec::new(),
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
                diagnostics: vec![
                    "Context files were truncated to 32768 bytes before delegation."
                        .to_string()
                ],
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
        let _minimax_coding_plan_key =
            EnvVarGuard::set("MINIMAX_CODING_PLAN_KEY", OsStr::new("test-coding-plan-key"));

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
    }

    #[test]
    fn delegate_to_minimax_prompt_is_only_added_when_tool_is_registered() {
        assert!(delegate_to_minimax_developer_instructions(&[]).is_none());
        assert!(
            delegate_to_minimax_developer_instructions(&[delegate_to_minimax_dynamic_tool()])
                .is_some()
        );
    }
}
