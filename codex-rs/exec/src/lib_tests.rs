use super::*;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::ProviderCredentials;
use codex_login::save_provider_credentials;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::trace::TraceId;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::sync::LazyLock;
use std::sync::Mutex;
use tempfile::tempdir;
use tracing_opentelemetry::OpenTelemetrySpanExt;

static MINIMAX_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn test_tracing_subscriber() -> impl tracing::Subscriber + Send + Sync {
    let provider = SdkTracerProvider::builder().build();
    let tracer = provider.tracer("codex-exec-tests");
    tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer))
}

fn save_minimax_credentials(codex_home: &std::path::Path) {
    save_provider_credentials(
        codex_home,
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
fn exec_defaults_analytics_to_enabled() {
    assert_eq!(DEFAULT_ANALYTICS_ENABLED, true);
}

#[test]
fn exec_root_span_can_be_parented_from_trace_context() {
    let subscriber = test_tracing_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let parent = codex_protocol::protocol::W3cTraceContext {
        traceparent: Some("00-00000000000000000000000000000077-0000000000000088-01".into()),
        tracestate: Some("vendor=value".into()),
    };
    let exec_span = exec_root_span();
    assert!(set_parent_from_w3c_trace_context(&exec_span, &parent));

    let trace_id = exec_span.context().span().span_context().trace_id();
    assert_eq!(
        trace_id,
        TraceId::from_hex("00000000000000000000000000000077").expect("trace id")
    );
}

#[test]
fn builds_uncommitted_review_request() {
    let args = ReviewArgs {
        uncommitted: true,
        base: None,
        commit: None,
        commit_title: None,
        prompt: None,
    };
    let request = build_review_request(&args).expect("builds uncommitted review request");

    let expected = ReviewRequest {
        target: ReviewTarget::UncommittedChanges,
        user_facing_hint: None,
    };

    assert_eq!(request, expected);
}

#[test]
fn builds_commit_review_request_with_title() {
    let args = ReviewArgs {
        uncommitted: false,
        base: None,
        commit: Some("123456789".to_string()),
        commit_title: Some("Add review command".to_string()),
        prompt: None,
    };
    let request = build_review_request(&args).expect("builds commit review request");

    let expected = ReviewRequest {
        target: ReviewTarget::Commit {
            sha: "123456789".to_string(),
            title: Some("Add review command".to_string()),
        },
        user_facing_hint: None,
    };

    assert_eq!(request, expected);
}

#[test]
fn builds_custom_review_request_trims_prompt() {
    let args = ReviewArgs {
        uncommitted: false,
        base: None,
        commit: None,
        commit_title: None,
        prompt: Some("  custom review instructions  ".to_string()),
    };
    let request = build_review_request(&args).expect("builds custom review request");

    let expected = ReviewRequest {
        target: ReviewTarget::Custom {
            instructions: "custom review instructions".to_string(),
        },
        user_facing_hint: None,
    };

    assert_eq!(request, expected);
}

#[test]
fn decode_prompt_bytes_strips_utf8_bom() {
    let input = [0xEF, 0xBB, 0xBF, b'h', b'i', b'\n'];

    let out = decode_prompt_bytes(&input).expect("decode utf-8 with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_decodes_utf16le_bom() {
    // UTF-16LE BOM + "hi\n"
    let input = [0xFF, 0xFE, b'h', 0x00, b'i', 0x00, b'\n', 0x00];

    let out = decode_prompt_bytes(&input).expect("decode utf-16le with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_decodes_utf16be_bom() {
    // UTF-16BE BOM + "hi\n"
    let input = [0xFE, 0xFF, 0x00, b'h', 0x00, b'i', 0x00, b'\n'];

    let out = decode_prompt_bytes(&input).expect("decode utf-16be with BOM");

    assert_eq!(out, "hi\n");
}

#[test]
fn decode_prompt_bytes_rejects_utf32le_bom() {
    // UTF-32LE BOM + "hi\n"
    let input = [
        0xFF, 0xFE, 0x00, 0x00, b'h', 0x00, 0x00, 0x00, b'i', 0x00, 0x00, 0x00, b'\n', 0x00, 0x00,
        0x00,
    ];

    let err = decode_prompt_bytes(&input).expect_err("utf-32le should be rejected");

    assert_eq!(
        err,
        PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32LE"
        }
    );
}

#[test]
fn decode_prompt_bytes_rejects_utf32be_bom() {
    // UTF-32BE BOM + "hi\n"
    let input = [
        0x00, 0x00, 0xFE, 0xFF, 0x00, 0x00, 0x00, b'h', 0x00, 0x00, 0x00, b'i', 0x00, 0x00, 0x00,
        b'\n',
    ];

    let err = decode_prompt_bytes(&input).expect_err("utf-32be should be rejected");

    assert_eq!(
        err,
        PromptDecodeError::UnsupportedBom {
            encoding: "UTF-32BE"
        }
    );
}

#[test]
fn decode_prompt_bytes_rejects_invalid_utf8() {
    // Invalid UTF-8 sequence: 0xC3 0x28
    let input = [0xC3, 0x28];

    let err = decode_prompt_bytes(&input).expect_err("invalid utf-8 should fail");

    assert_eq!(err, PromptDecodeError::InvalidUtf8 { valid_up_to: 0 });
}

#[test]
fn prompt_with_stdin_context_wraps_stdin_block() {
    let combined = prompt_with_stdin_context("Summarize this concisely", "my output");

    assert_eq!(
        combined,
        "Summarize this concisely\n\n<stdin>\nmy output\n</stdin>"
    );
}

#[test]
fn prompt_with_stdin_context_preserves_trailing_newline() {
    let combined = prompt_with_stdin_context("Summarize this concisely", "my output\n");

    assert_eq!(
        combined,
        "Summarize this concisely\n\n<stdin>\nmy output\n</stdin>"
    );
}

#[test]
fn lagged_event_warning_message_is_explicit() {
    assert_eq!(
        lagged_event_warning_message(/*skipped*/ 7),
        "in-process app-server event stream lagged; dropped 7 events".to_string()
    );
}

#[tokio::test]
async fn resume_lookup_model_providers_filters_only_last_lookup() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build default config");
    config.model_provider_id = "test-provider".to_string();

    let last_args = crate::cli::ResumeArgs {
        session_id: None,
        last: true,
        all: false,
        images: vec![],
        prompt: None,
    };
    let named_args = crate::cli::ResumeArgs {
        session_id: Some("named-session".to_string()),
        last: false,
        all: false,
        images: vec![],
        prompt: None,
    };

    assert_eq!(
        resume_lookup_model_providers(&config, &last_args),
        Some(vec!["test-provider".to_string()])
    );
    assert_eq!(resume_lookup_model_providers(&config, &named_args), None);
}

#[test]
fn turn_items_for_thread_returns_matching_turn_items() {
    let thread = AppServerThread {
        id: "thread-1".to_string(),
        forked_from_id: None,
        preview: String::new(),
        ephemeral: false,
        model_provider: "openai".to_string(),
        created_at: 0,
        updated_at: 0,
        status: codex_app_server_protocol::ThreadStatus::Idle,
        path: None,
        cwd: test_path_buf("/tmp/project").abs(),
        cli_version: "0.0.0-test".to_string(),
        source: codex_app_server_protocol::SessionSource::Exec,
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        turns: vec![
            codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items: vec![AppServerThreadItem::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "hello".to_string(),
                    phase: None,
                    memory_citation: None,
                }],
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
            codex_app_server_protocol::Turn {
                id: "turn-2".to_string(),
                items: vec![AppServerThreadItem::Plan {
                    id: "plan-1".to_string(),
                    text: "ship it".to_string(),
                }],
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        ],
    };

    assert_eq!(
        turn_items_for_thread(&thread, "turn-1"),
        Some(vec![AppServerThreadItem::AgentMessage {
            id: "msg-1".to_string(),
            text: "hello".to_string(),
            phase: None,
            memory_citation: None,
        }])
    );
    assert_eq!(turn_items_for_thread(&thread, "missing-turn"), None);
}

#[test]
fn should_backfill_turn_completed_items_skips_ephemeral_threads() {
    let notification =
        ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items: Vec::new(),
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        });

    assert!(!should_backfill_turn_completed_items(
        /*thread_ephemeral*/ true,
        &notification
    ));
}

#[test]
fn canceled_mcp_server_elicitation_response_uses_cancel_action() {
    let value = canceled_mcp_server_elicitation_response()
        .expect("mcp elicitation cancel response should serialize");
    let response: McpServerElicitationRequestResponse =
        serde_json::from_value(value).expect("cancel response should deserialize");

    assert_eq!(
        response,
        McpServerElicitationRequestResponse {
            action: McpServerElicitationAction::Cancel,
            content: None,
            meta: None,
        }
    );
}

#[tokio::test]
async fn thread_start_params_include_review_policy_when_review_policy_is_manual_only() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            approvals_reviewer: Some(ApprovalsReviewer::User),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with manual-only review policy");

    let params = thread_start_params_from_config(&config, /*strict_delegation*/ false);

    assert_eq!(
        params.approvals_reviewer,
        Some(codex_app_server_protocol::ApprovalsReviewer::User)
    );
    assert_eq!(params.sandbox, None);
    assert_eq!(
        params.permission_profile,
        Some(config.permissions.permission_profile().into())
    );
}

#[tokio::test]
async fn thread_start_params_include_review_policy_when_auto_review_is_enabled() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            approvals_reviewer: Some(ApprovalsReviewer::AutoReview),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with guardian review policy");

    let params = thread_start_params_from_config(&config, /*strict_delegation*/ false);

    assert_eq!(
        params.approvals_reviewer,
        Some(codex_app_server_protocol::ApprovalsReviewer::AutoReview)
    );
}

#[tokio::test]
async fn thread_start_params_include_delegate_tool_when_minimax_credentials_exist() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    save_minimax_credentials(codex_home.path());
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with minimax credentials");

    let params = thread_start_params_from_config(&config, /*strict_delegation*/ false);
    let dynamic_tools = params
        .dynamic_tools
        .expect("delegate_to_minimax should be registered");

    assert_eq!(dynamic_tools.len(), 1);
    assert_eq!(dynamic_tools[0].name, "delegate_to_minimax");
}

#[test]
fn thread_start_params_include_delegate_tool_when_minimax_api_key_env_exists() {
    let _lock = MINIMAX_ENV_LOCK.lock().expect("lock minimax env");
    let _minimax_api_key = EnvVarGuard::set("MINIMAX_API_KEY", OsStr::new("test-key"));
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");
    let codex_home = tempdir().expect("create temp codex home");
    let dynamic_tools = delegate_dynamic_tools(codex_home.path())
        .expect("delegate_to_minimax should be registered");

    assert_eq!(dynamic_tools.len(), 1);
    assert_eq!(dynamic_tools[0].name, "delegate_to_minimax");
}

#[test]
fn thread_start_params_include_delegate_tool_when_minimax_coding_plan_env_exists() {
    let _lock = MINIMAX_ENV_LOCK.lock().expect("lock minimax env");
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key =
        EnvVarGuard::set("MINIMAX_CODING_PLAN_KEY", OsStr::new("test-plan-key"));
    let codex_home = tempdir().expect("create temp codex home");
    let dynamic_tools = delegate_dynamic_tools(codex_home.path())
        .expect("delegate_to_minimax should be registered");

    assert_eq!(dynamic_tools.len(), 1);
    assert_eq!(dynamic_tools[0].name, "delegate_to_minimax");
}

#[test]
fn thread_start_params_omit_delegate_tool_without_minimax_credentials() {
    let _lock = MINIMAX_ENV_LOCK.lock().expect("lock minimax env");
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");
    let codex_home = tempdir().expect("create temp codex home");

    assert_eq!(delegate_dynamic_tools(codex_home.path()), None);
}

#[tokio::test]
async fn thread_start_params_include_strict_delegation_instructions_when_enabled() {
    let codex_home = tempdir().expect("create temp codex home");
    let cwd = tempdir().expect("create temp cwd");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            developer_instructions: Some("Stay inside the repo.".to_string()),
            ..Default::default()
        })
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("build config with developer instructions");

    let params = thread_start_params_from_config(&config, /*strict_delegation*/ true);
    let developer_instructions = params
        .developer_instructions
        .expect("strict delegation should inject developer instructions");

    assert!(developer_instructions.contains("Stay inside the repo."));
    assert!(developer_instructions.contains(STRICT_DELEGATION_MARKER));
    assert!(developer_instructions.contains("Strict delegation mode is active"));
    assert!(developer_instructions.contains("Do not use shell/unified_exec"));
    assert!(
        developer_instructions.contains("only apply that exact patch candidate with `apply_patch`")
    );
    assert!(developer_instructions.contains("patch_lines"));
    assert!(developer_instructions.contains("patch_not_applicable"));
}

#[tokio::test]
async fn exec_dynamic_tool_call_response_reports_unregistered_tool() {
    let temp_dir = tempdir().expect("create temp dir");
    let context = ExecDynamicToolContext {
        available_tools: std::collections::HashSet::new(),
        cwd: temp_dir.path().to_path_buf(),
        codex_home: temp_dir.path().to_path_buf(),
    };

    let response = exec_dynamic_tool_call_response(
        codex_app_server_protocol::DynamicToolCallParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            namespace: None,
            tool: "unknown_dynamic_tool".to_string(),
            arguments: serde_json::json!({}),
        },
        &context,
    )
    .await;

    assert_eq!(response.success, false);
    assert_eq!(
        response.content_items,
        vec![
            codex_app_server_protocol::DynamicToolCallOutputContentItem::InputText {
                text:
                    "dynamic tool `unknown_dynamic_tool` is not registered for this exec session."
                        .to_string(),
            },
        ]
    );
}

#[tokio::test]
async fn exec_dynamic_tool_call_response_reports_invalid_arguments() {
    let temp_dir = tempdir().expect("create temp dir");
    let context = ExecDynamicToolContext {
        available_tools: std::collections::HashSet::from([
            codex_core::DELEGATE_TO_MINIMAX_TOOL_NAME.to_string(),
        ]),
        cwd: temp_dir.path().to_path_buf(),
        codex_home: temp_dir.path().to_path_buf(),
    };

    let response = exec_dynamic_tool_call_response(
        codex_app_server_protocol::DynamicToolCallParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            namespace: None,
            tool: codex_core::DELEGATE_TO_MINIMAX_TOOL_NAME.to_string(),
            arguments: serde_json::json!({}),
        },
        &context,
    )
    .await;

    assert_eq!(response.success, false);
    assert_eq!(response.content_items.len(), 1);
    let [codex_app_server_protocol::DynamicToolCallOutputContentItem::InputText { text }] =
        response.content_items.as_slice()
    else {
        panic!("expected single text output item");
    };
    assert!(text.starts_with("delegate_to_minimax received invalid arguments:"));
}

#[test]
fn session_configured_from_thread_response_uses_review_policy_from_response() {
    let response = ThreadStartResponse {
        thread: codex_app_server_protocol::Thread {
            id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
            forked_from_id: None,
            preview: String::new(),
            ephemeral: false,
            model_provider: "openai".to_string(),
            created_at: 0,
            updated_at: 0,
            status: codex_app_server_protocol::ThreadStatus::Idle,
            path: Some(PathBuf::from("/tmp/rollout.jsonl")),
            cwd: test_path_buf("/tmp").abs(),
            cli_version: "0.0.0".to_string(),
            source: codex_app_server_protocol::SessionSource::Cli,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: Some("thread".to_string()),
            turns: vec![],
        },
        model: "gpt-5.4".to_string(),
        model_provider: "openai".to_string(),
        service_tier: None,
        cwd: test_path_buf("/tmp").abs(),
        instruction_sources: Vec::new(),
        approval_policy: codex_app_server_protocol::AskForApproval::OnRequest,
        approvals_reviewer: codex_app_server_protocol::ApprovalsReviewer::AutoReview,
        sandbox: codex_app_server_protocol::SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        },
        permission_profile: Some(
            codex_protocol::models::PermissionProfile::from_legacy_sandbox_policy(
                &codex_protocol::protocol::SandboxPolicy::new_workspace_write_policy(),
            )
            .into(),
        ),
        reasoning_effort: None,
    };

    let event = session_configured_from_thread_start_response(&response)
        .expect("build bootstrap session configured event");

    assert_eq!(event.approvals_reviewer, ApprovalsReviewer::AutoReview);
}
