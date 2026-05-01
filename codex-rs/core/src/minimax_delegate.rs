use crate::client_common::Prompt;
use codex_api::ResponseEvent;
use codex_config::types::AuthCredentialsStoreMode;
use codex_minimax::AuthPreference;
use codex_minimax::resolve_auth_from_env;
use codex_model_provider_info::MINIMAX_PROVIDER_ID;
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
- Writing unit tests for code you've already analyzed
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
   - context_files: optional file paths whose content provides context
4. The worker returns text output
5. Evaluate that output before applying it:
   - If satisfactory: use apply_patch to commit it
   - If unclear or wrong: refine the criteria and call the tool again, or do it yourself
   - If the worker responded with CLARIFY:, refine the task and retry

The user may see the worker output in tool history. You are responsible for evaluating quality before applying it."#;

const MINIMAX_DELEGATE_SYSTEM_PROMPT: &str = r#"You are MiniMax-M2.7, a delegated coding worker for Codrex.

You are only handling bounded mechanical implementation work delegated by a primary model.
Complete the requested implementation directly when the task is clear.

If the task is ambiguous or missing critical information needed to proceed safely, respond with `CLARIFY:` followed by exactly one concrete question.
Do not invent missing requirements.
Do not describe approval gates, terminal commands, or filesystem mutations.
Return the requested code or patch-ready text directly. Keep commentary brief unless it materially helps the caller evaluate the result."#;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DelegateToMinimaxRequest {
    pub task_description: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub context_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateToMinimaxResponse {
    Completed {
        output: String,
        context_truncated: bool,
    },
    Clarify {
        question: String,
        context_truncated: bool,
    },
}

pub fn delegate_to_minimax_dynamic_tool() -> DynamicToolSpec {
    DynamicToolSpec {
        namespace: None,
        name: DELEGATE_TO_MINIMAX_TOOL_NAME.to_string(),
        description:
            "Delegate a mechanical implementation task to MiniMax-M2.7, a faster cheaper worker model."
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
                    "description": "Optional file paths whose content provides context"
                }
            },
            "required": ["task_description"]
        }),
        defer_loading: false,
    }
}

pub fn is_delegate_to_minimax_available(codex_home: &Path) -> bool {
    resolve_auth_from_env(AuthPreference::default()).is_ok()
        || codex_login::load_provider_credentials(
            codex_home,
            AuthCredentialsStoreMode::Auto,
            MINIMAX_PROVIDER_ID,
        )
        .ok()
        .flatten()
        .is_some()
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
    let mut stream = crate::minimax_adapter::stream_chat_completions(
        &provider,
        &prompt,
        MINIMAX_DELEGATE_MODEL,
        reqwest::Client::new(),
    )
    .await?;

    let output = collect_stream_text(&mut stream).await?;
    let output = output.trim_end().to_string();
    if output.is_empty() {
        return Err(CodexErr::UnsupportedOperation(
            "MiniMax returned no text output for delegate_to_minimax".to_string(),
        ));
    }

    let trimmed = output.trim_start();
    if let Some(question) = trimmed.strip_prefix(CLARIFY_PREFIX) {
        return Ok(DelegateToMinimaxResponse::Clarify {
            question: question.trim().to_string(),
            context_truncated,
        });
    }

    Ok(DelegateToMinimaxResponse::Completed {
        output,
        context_truncated,
    })
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
    let _ = writeln!(
        text,
        "If you need clarification before proceeding, respond with `CLARIFY:` followed by one question."
    );
    text
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
    use tempfile::TempDir;
    use wiremock::MockServer;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::CONTEXT_FILES_MAX_BYTES;
    use super::DelegateToMinimaxRequest;
    use super::DelegateToMinimaxResponse;
    use super::delegate_to_minimax;
    use super::delegate_to_minimax_developer_instructions;
    use super::delegate_to_minimax_dynamic_tool;
    use super::is_delegate_to_minimax_available;

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
        unsafe {
            std::env::set_var("CODEX_HOME", temp_dir.path());
            std::env::set_var("MINIMAX_BASE_URL", format!("{}/v1", server.uri()));
        }

        let result = delegate_to_minimax(
            DelegateToMinimaxRequest {
                task_description: "Implement validate_email".to_string(),
                acceptance_criteria: vec!["Add 5 unit tests".to_string()],
                context_files: Vec::new(),
            },
            temp_dir.path(),
        )
        .await
        .expect("delegate call should succeed");

        assert_eq!(
            result,
            DelegateToMinimaxResponse::Clarify {
                question: "should I add property-based tests?".to_string(),
                context_truncated: false,
            }
        );

        unsafe {
            std::env::remove_var("CODEX_HOME");
            std::env::remove_var("MINIMAX_BASE_URL");
        }
    }

    #[tokio::test]
    #[serial(env_minimax_delegate)]
    async fn delegate_to_minimax_reports_context_truncation() {
        let server = MockServer::start().await;
        let body = nonstreaming_chat_response_body("def fizzbuzz(n):\n    return []");
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
        unsafe {
            std::env::set_var("CODEX_HOME", temp_dir.path());
            std::env::set_var("MINIMAX_BASE_URL", format!("{}/v1", server.uri()));
        }

        let result = delegate_to_minimax(
            DelegateToMinimaxRequest {
                task_description: "Implement fizzbuzz".to_string(),
                acceptance_criteria: vec!["Write 3 unit tests".to_string()],
                context_files: vec!["big_context.py".to_string()],
            },
            temp_dir.path(),
        )
        .await
        .expect("delegate call should succeed");

        assert_eq!(
            result,
            DelegateToMinimaxResponse::Completed {
                output: "def fizzbuzz(n):\n    return []".to_string(),
                context_truncated: true,
            }
        );

        unsafe {
            std::env::remove_var("CODEX_HOME");
            std::env::remove_var("MINIMAX_BASE_URL");
        }
    }

    #[test]
    fn delegate_to_minimax_is_available_when_chatgpt_and_minimax_auth_coexist() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        write_chatgpt_auth_json(&temp_dir);
        save_minimax_credentials(&temp_dir);

        assert!(is_delegate_to_minimax_available(temp_dir.path()));
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
