#![cfg(not(target_os = "windows"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![expect(
    clippy::await_holding_invalid_type,
    reason = "tests intentionally hold the MiniMax environment lock across async work to serialize process-wide environment mutations"
)]

use anyhow::Context;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::ProviderCredentials;
use codex_login::save_provider_credentials;
use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;
use tokio::sync::Mutex;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_string_contains;
use wiremock::matchers::method;
use wiremock::matchers::path;

static MINIMAX_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const DELEGATE_CALL_ID: &str = "delegate-call";
const DELEGATE_RETRY_CALL_ID: &str = "delegate-call-retry";
const APPLY_CALL_ID: &str = "apply-call";
const SHELL_WRITE_CALL_ID: &str = "shell-write-call";
const FIX_ADD_PATCH: &str = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch\n";
const ALT_ADD_PATCH: &str = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a * b }\n*** End Patch\n";
const STRICT_BLOCK_MESSAGE: &str = "blocked: strict delegation mode requires applying a completed patch candidate returned by delegate_to_minimax. No matching candidate is available.";
const STRICT_SHELL_BLOCK_MESSAGE: &str = "blocked: strict delegation mode forbids manual file modifications via shell. Apply a completed patch candidate returned by delegate_to_minimax instead.";
const SHELL_REWRITE_LIB_RS_COMMAND: &str =
    "cat <<'EOF' > src/lib.rs\npub fn add(a: i32, b: i32) -> i32 { a + b }\nEOF\n";

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

fn save_minimax_credentials(codex_home: &Path) {
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

async fn lock_minimax_env() -> tokio::sync::MutexGuard<'static, ()> {
    MINIMAX_ENV_LOCK.lock().await
}

fn minimax_stream_body(content: &str) -> String {
    let content = serde_json::to_string(content).expect("serialize minimax content");
    format!(
        "data: {{\"id\":\"resp-test\",\"object\":\"chat.completion.chunk\",\
         \"choices\":[{{\"index\":0,\"delta\":{{\"content\":{content}}}}}]}}\n\n\
         data: {{\"id\":\"resp-test\",\"choices\":[{{\"index\":0,\"finish_reason\":\"stop\",\
         \"delta\":{{}}}}],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":3,\
         \"total_tokens\":8}}}}\n\n\
         data: [DONE]\n\n"
    )
}

fn init_add_repo(root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join("tests"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"delegate-add\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )?;
    fs::write(
        root.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a - b }\n",
    )?;
    fs::write(
        root.join("tests/add.rs"),
        "use delegate_add::add;\n\n#[test]\nfn adds_numbers() {\n    assert_eq!(add(1, 2), 3);\n}\n",
    )?;

    let status = Command::new("git")
        .arg("init")
        .current_dir(root)
        .status()
        .context("git init")?;
    assert!(status.success(), "git init failed with {status}");
    Ok(())
}

fn run_cargo_test(root: &Path) -> anyhow::Result<()> {
    let status = Command::new("cargo")
        .arg("test")
        .current_dir(root)
        .status()
        .context("cargo test")?;
    assert!(status.success(), "cargo test failed with {status}");
    Ok(())
}

fn delegate_arguments(
    task_description: &str,
    acceptance_criteria: &[&str],
    context_files: &[&str],
) -> String {
    serde_json::to_string(&serde_json::json!({
        "task_description": task_description,
        "acceptance_criteria": acceptance_criteria,
        "context_files": context_files,
    }))
    .expect("serialize delegate arguments")
}

fn request_json(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).expect("request body should be valid JSON")
}

fn request_has_function_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    request_json(request)
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|input| {
            input.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
        })
}

fn apply_output(
    request: &responses::ResponsesRequest,
    call_id: &str,
) -> (Option<String>, Option<bool>) {
    request
        .function_call_output_content_and_success(call_id)
        .expect("tool output should be present")
}

fn run_exec(
    test: &core_test_support::test_codex_exec::TestCodexExecBuilder,
    supervisor_server: &wiremock::MockServer,
    strict_delegation: bool,
    prompt: &str,
) -> anyhow::Result<std::process::Output> {
    let mut cmd = test.cmd_with_server(supervisor_server);
    cmd.arg("--skip-git-repo-check");
    if strict_delegation {
        cmd.arg("--strict-delegation");
    }
    cmd.arg("-s")
        .arg("danger-full-access")
        .arg(prompt)
        .output()
        .context("run codex-exec")
}

fn assert_file_unchanged(test: &core_test_support::test_codex_exec::TestCodexExecBuilder) {
    let final_source =
        fs::read_to_string(test.cwd_path().join("src/lib.rs")).expect("read final source");
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a - b }\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_off_allows_manual_apply_patch() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| !request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_assistant_message("msg-1", "Applied patch manually."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ false,
        "fix add manually",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let request = after_apply.single_request();
    let (_apply_output, apply_success) = apply_output(&request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(false));

    let final_source = fs::read_to_string(test.cwd_path().join("src/lib.rs"))?;
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );
    run_cargo_test(test.cwd_path())?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_off_allows_manual_shell_write() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| !request_has_function_call_output(req, SHELL_WRITE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_shell_command_call(SHELL_WRITE_CALL_ID, SHELL_REWRITE_LIB_RS_COMMAND),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_shell = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, SHELL_WRITE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_assistant_message("msg-1", "Edited src/lib.rs via shell."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ false,
        "rewrite add via shell",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let shell_request = after_shell.single_request();
    let (_shell_output, shell_success) = apply_output(&shell_request, SHELL_WRITE_CALL_ID);
    assert_ne!(shell_success, Some(false));

    let final_source = fs::read_to_string(test.cwd_path().join("src/lib.rs"))?;
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );
    run_cargo_test(test.cwd_path())?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_blocks_apply_patch_without_delegate_call() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| !request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_assistant_message("msg-1", "Strict delegation blocked the manual patch."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "fix add manually",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let request = after_apply.single_request();
    let (apply_output, apply_success) = apply_output(&request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(true));
    assert_eq!(apply_output.as_deref(), Some(STRICT_BLOCK_MESSAGE));
    assert_file_unchanged(&test);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_blocks_manual_shell_write() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| !request_has_function_call_output(req, SHELL_WRITE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_shell_command_call(SHELL_WRITE_CALL_ID, SHELL_REWRITE_LIB_RS_COMMAND),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_shell = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, SHELL_WRITE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_assistant_message("msg-1", "Strict delegation blocked shell write."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "rewrite add via shell",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let shell_request = after_shell.single_request();
    let (shell_output, shell_success) = apply_output(&shell_request, SHELL_WRITE_CALL_ID);
    assert_ne!(shell_success, Some(true));
    assert_eq!(shell_output.as_deref(), Some(STRICT_SHELL_BLOCK_MESSAGE));
    assert_file_unchanged(&test);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_blocks_apply_patch_after_delegate_infra_error() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;
    save_minimax_credentials(test.home_path());

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            !request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "Fix add so it returns the sum of both inputs.",
                    &["cargo test must pass"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message(
                "msg-1",
                "MiniMax failed and strict delegation blocked fallback.",
            ),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("server exploded"))
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "fix add with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let delegate_request = after_delegate.single_request();
    let (delegate_output, delegate_success) = apply_output(&delegate_request, DELEGATE_CALL_ID);
    assert_ne!(delegate_success, Some(true));
    assert!(
        delegate_output
            .as_deref()
            .is_some_and(|text| text.starts_with("MiniMax delegation failed:")),
        "unexpected delegate output: {delegate_output:?}"
    );

    let apply_request = after_apply.single_request();
    let (apply_output, apply_success) = apply_output(&apply_request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(true));
    assert_eq!(apply_output.as_deref(), Some(STRICT_BLOCK_MESSAGE));
    assert_file_unchanged(&test);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_blocks_apply_patch_after_invalid_delegate_result() -> anyhow::Result<()>
{
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;
    save_minimax_credentials(test.home_path());

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            !request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "Try an underspecified edit.",
                    &["Return invalid if the worker answers with prose"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message(
                "msg-1",
                "Invalid worker output blocked manual fallback.",
            ),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(minimax_stream_body("I updated the helper and added tests.")),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "fix add with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let delegate_request = after_delegate.single_request();
    let (delegate_output, delegate_success) = apply_output(&delegate_request, DELEGATE_CALL_ID);
    assert_ne!(delegate_success, Some(false));
    let delegate_json: Value =
        serde_json::from_str(delegate_output.as_deref().expect("delegate output text"))?;
    assert_eq!(delegate_json["status"], "invalid");

    let apply_request = after_apply.single_request();
    let (apply_output, apply_success) = apply_output(&apply_request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(true));
    assert_eq!(apply_output.as_deref(), Some(STRICT_BLOCK_MESSAGE));
    assert_file_unchanged(&test);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_blocks_apply_patch_after_clarify_delegate_result() -> anyhow::Result<()>
{
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;
    save_minimax_credentials(test.home_path());

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            !request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "Clarify parse_value requirements.",
                    &["Ask a question instead of guessing"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message("msg-1", "Clarify response blocked manual fallback."),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(minimax_stream_body(
                    r#"{"status":"clarify","question":"What input shape should parse_value accept?","diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "clarify with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let delegate_request = after_delegate.single_request();
    let (delegate_output, delegate_success) = apply_output(&delegate_request, DELEGATE_CALL_ID);
    assert_ne!(delegate_success, Some(false));
    let delegate_json: Value =
        serde_json::from_str(delegate_output.as_deref().expect("delegate output text"))?;
    assert_eq!(delegate_json["status"], "clarify");

    let apply_request = after_apply.single_request();
    let (apply_output, apply_success) = apply_output(&apply_request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(true));
    assert_eq!(apply_output.as_deref(), Some(STRICT_BLOCK_MESSAGE));
    assert_file_unchanged(&test);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_allows_exact_completed_patch_candidate() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;
    save_minimax_credentials(test.home_path());

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            !request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "Fix add so it returns the sum of both inputs.",
                    &["cargo test must pass"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message("msg-1", "Applied the delegated patch."),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(minimax_stream_body(
                    r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch","diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "fix add with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let delegate_request = after_delegate.single_request();
    let (delegate_output, delegate_success) = apply_output(&delegate_request, DELEGATE_CALL_ID);
    assert_ne!(delegate_success, Some(false));
    let delegate_json: Value =
        serde_json::from_str(delegate_output.as_deref().expect("delegate output text"))?;
    assert_eq!(delegate_json["status"], "completed");
    assert_eq!(
        delegate_json["patch"],
        Value::String(FIX_ADD_PATCH.trim_end().to_string())
    );

    let apply_request = after_apply.single_request();
    let (_apply_output, apply_success) = apply_output(&apply_request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(false));

    let final_source = fs::read_to_string(test.cwd_path().join("src/lib.rs"))?;
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );
    run_cargo_test(test.cwd_path())?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_allows_normalized_heredoc_candidate() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;
    save_minimax_credentials(test.home_path());

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            !request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "Fix add so it returns the sum of both inputs.",
                    &["cargo test must pass"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message("msg-1", "Applied the normalized delegated patch."),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(minimax_stream_body(
                    r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"apply_patch <<'PATCH'\n*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch\nPATCH","diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "fix add with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let delegate_request = after_delegate.single_request();
    let (delegate_output, delegate_success) = apply_output(&delegate_request, DELEGATE_CALL_ID);
    assert_ne!(delegate_success, Some(false));
    let delegate_json: Value =
        serde_json::from_str(delegate_output.as_deref().expect("delegate output text"))?;
    assert_eq!(delegate_json["status"], "completed");
    assert_eq!(
        delegate_json["patch"],
        Value::String(FIX_ADD_PATCH.trim_end().to_string())
    );
    assert!(
        delegate_json["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics.iter().any(|item| {
                item.as_str() == Some("normalized worker patch: extracted apply_patch heredoc")
            }))
    );

    let apply_request = after_apply.single_request();
    let (_apply_output, apply_success) = apply_output(&apply_request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(false));

    let final_source = fs::read_to_string(test.cwd_path().join("src/lib.rs"))?;
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );
    run_cargo_test(test.cwd_path())?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_blocks_manual_apply_after_non_applicable_delegate_result()
-> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;
    save_minimax_credentials(test.home_path());

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            !request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "Fix add so it returns the sum of both inputs.",
                    &["cargo test must pass"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message(
                "msg-1",
                "Strict delegation blocked manual apply after a non-applicable candidate.",
            ),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(minimax_stream_body(
                    r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@ -1,5 +1,5 @@\n-pub fn add(a: i32, b: i32) -> i32 {\n-    a - b\n-}\n+pub fn add(a: i32, b: i32) -> i32 {\n+    a + b\n+}\n*** End Patch","diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "fix add with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let delegate_request = after_delegate.single_request();
    let (delegate_output, delegate_success) = apply_output(&delegate_request, DELEGATE_CALL_ID);
    assert_ne!(delegate_success, Some(false));
    let delegate_json: Value =
        serde_json::from_str(delegate_output.as_deref().expect("delegate output text"))?;
    assert_eq!(delegate_json["status"], "invalid");
    assert_eq!(
        delegate_json["error"],
        "patch_not_applicable: context did not match src/lib.rs"
    );

    let apply_request = after_apply.single_request();
    let (apply_output, apply_success) = apply_output(&apply_request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(true));
    assert_eq!(apply_output.as_deref(), Some(STRICT_BLOCK_MESSAGE));
    assert_file_unchanged(&test);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_blocks_shell_write_even_with_completed_candidate() -> anyhow::Result<()>
{
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;
    save_minimax_credentials(test.home_path());

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            !request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, SHELL_WRITE_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "Fix add so it returns the sum of both inputs.",
                    &["cargo test must pass"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, SHELL_WRITE_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_shell_command_call(SHELL_WRITE_CALL_ID, SHELL_REWRITE_LIB_RS_COMMAND),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_shell = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, SHELL_WRITE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message(
                "msg-1",
                "Strict delegation refused the shell bypass and kept the repo unchanged.",
            ),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(minimax_stream_body(
                    r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch","diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "fix add with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let delegate_request = after_delegate.single_request();
    let (delegate_output, delegate_success) = apply_output(&delegate_request, DELEGATE_CALL_ID);
    assert_ne!(delegate_success, Some(false));
    let delegate_json: Value =
        serde_json::from_str(delegate_output.as_deref().expect("delegate output text"))?;
    assert_eq!(delegate_json["status"], "completed");

    let shell_request = after_shell.single_request();
    let (shell_output, shell_success) = apply_output(&shell_request, SHELL_WRITE_CALL_ID);
    assert_ne!(shell_success, Some(true));
    assert_eq!(shell_output.as_deref(), Some(STRICT_SHELL_BLOCK_MESSAGE));
    assert_file_unchanged(&test);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_blocks_different_patch_than_completed_candidate() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;
    save_minimax_credentials(test.home_path());

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            !request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "Fix add so it returns the sum of both inputs.",
                    &["cargo test must pass"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, ALT_ADD_PATCH),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message(
                "msg-1",
                "Strict delegation blocked the mismatched patch.",
            ),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(minimax_stream_body(
                    r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch","diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "fix add with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let delegate_request = after_delegate.single_request();
    let (delegate_output, delegate_success) = apply_output(&delegate_request, DELEGATE_CALL_ID);
    assert_ne!(delegate_success, Some(false));
    let delegate_json: Value =
        serde_json::from_str(delegate_output.as_deref().expect("delegate output text"))?;
    assert_eq!(delegate_json["status"], "completed");

    let apply_request = after_apply.single_request();
    let (apply_output, apply_success) = apply_output(&apply_request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(true));
    assert_eq!(apply_output.as_deref(), Some(STRICT_BLOCK_MESSAGE));
    assert_file_unchanged(&test);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_allows_retry_after_invalid_then_completed_delegate_result()
-> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;
    save_minimax_credentials(test.home_path());

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            !request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, DELEGATE_RETRY_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "first pass: try an underspecified edit",
                    &["Return invalid if the worker answers with prose"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_first_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_CALL_ID)
                && !request_has_function_call_output(req, DELEGATE_RETRY_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_function_call(
                DELEGATE_RETRY_CALL_ID,
                "delegate_to_minimax",
                &delegate_arguments(
                    "second pass: fix add so it returns the sum of both inputs",
                    &["cargo test must pass"],
                    &["src/lib.rs"],
                ),
            ),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_retry_delegate = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DELEGATE_RETRY_CALL_ID)
                && !request_has_function_call_output(req, APPLY_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;
    let after_apply = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-4"),
            responses::ev_assistant_message(
                "msg-1",
                "Retried delegate_to_minimax and applied the returned patch.",
            ),
            responses::ev_completed("resp-4"),
        ]),
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(
            "first pass: try an underspecified edit",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(minimax_stream_body("I updated the helper and added tests.")),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(
            "second pass: fix add so it returns the sum of both inputs",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(minimax_stream_body(
                    r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch","diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec(
        &test,
        &supervisor_server,
        /*strict_delegation*/ true,
        "fix add with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let first_delegate_request = after_first_delegate.single_request();
    let (first_delegate_output, first_delegate_success) =
        apply_output(&first_delegate_request, DELEGATE_CALL_ID);
    assert_ne!(first_delegate_success, Some(false));
    let first_delegate_json: Value = serde_json::from_str(
        first_delegate_output
            .as_deref()
            .expect("delegate output text"),
    )?;
    assert_eq!(first_delegate_json["status"], "invalid");

    let retry_delegate_request = after_retry_delegate.single_request();
    let (retry_delegate_output, retry_delegate_success) =
        apply_output(&retry_delegate_request, DELEGATE_RETRY_CALL_ID);
    assert_ne!(retry_delegate_success, Some(false));
    let retry_delegate_json: Value = serde_json::from_str(
        retry_delegate_output
            .as_deref()
            .expect("delegate output text"),
    )?;
    assert_eq!(retry_delegate_json["status"], "completed");

    let apply_request = after_apply.single_request();
    let (_apply_output, apply_success) = apply_output(&apply_request, APPLY_CALL_ID);
    assert_ne!(apply_success, Some(false));

    let final_source = fs::read_to_string(test.cwd_path().join("src/lib.rs"))?;
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );
    run_cargo_test(test.cwd_path())?;

    Ok(())
}

#[test]
fn strict_delegation_block_message_stays_secret_free() {
    for forbidden in [
        FIX_ADD_PATCH,
        ALT_ADD_PATCH,
        "OPENAI_API_KEY",
        ".env",
        SHELL_REWRITE_LIB_RS_COMMAND,
    ] {
        assert!(
            !STRICT_BLOCK_MESSAGE.contains(forbidden),
            "strict delegation block message should stay safe"
        );
        assert!(
            !STRICT_SHELL_BLOCK_MESSAGE.contains(forbidden),
            "strict delegation shell block message should stay safe"
        );
    }
}
