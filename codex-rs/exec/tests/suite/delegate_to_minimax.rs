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
use walkdir::WalkDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

static MINIMAX_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const DELEGATE_CALL_ID: &str = "delegate-call";
const APPLY_CALL_ID: &str = "apply-call";
const SHELL_CALL_ID: &str = "shell-call";
const UNKNOWN_CALL_ID: &str = "unknown-call";
const FAKE_SECRET: &str = "sk-test-secret-should-not-leak";
const FIX_ADD_PATCH: &str = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch\n";

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

fn run_git(root: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("git {}", args.join(" ")))?;
    assert!(status.success(), "git {args:?} failed with {status}");
    Ok(())
}

fn seed_committed_repo_with_sensitive_env(root: &Path) -> anyhow::Result<()> {
    seed_committed_repo_with_sensitive_files(root)
}

fn seed_committed_repo_with_sensitive_files(root: &Path) -> anyhow::Result<()> {
    init_add_repo(root)?;
    fs::write(root.join(".env"), "OPENAI_API_KEY=sk-initial-safe\n")?;
    fs::write(
        root.join("auth.json"),
        format!(r#"{{"token":"{FAKE_SECRET}"}}"#),
    )?;
    fs::create_dir_all(root.join("secrets"))?;
    fs::write(
        root.join("secrets/id_rsa"),
        format!("-----BEGIN PRIVATE KEY-----\n{FAKE_SECRET}\n-----END PRIVATE KEY-----\n"),
    )?;
    run_git(root, &["config", "user.email", "test@example.com"])?;
    run_git(root, &["config", "user.name", "Test User"])?;
    run_git(
        root,
        &[
            "add",
            "Cargo.toml",
            "src/lib.rs",
            "tests/add.rs",
            ".env",
            "auth.json",
            "secrets/id_rsa",
        ],
    )?;
    run_git(root, &["commit", "-m", "initial state"])?;
    Ok(())
}

fn read_rollout_text(home_path: &Path) -> anyhow::Result<String> {
    let sessions_dir = home_path.join("sessions");
    let mut contents = String::new();
    for entry in WalkDir::new(&sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() || !entry.file_name().to_string_lossy().ends_with(".jsonl")
        {
            continue;
        }
        contents.push_str(
            fs::read_to_string(entry.path())
                .with_context(|| format!("read rollout {}", entry.path().display()))?
                .as_str(),
        );
    }
    Ok(contents)
}

fn assert_secret_absent_from_tree(root: &Path, secret: &str) -> anyhow::Result<()> {
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }

        let contents = fs::read(entry.path())
            .with_context(|| format!("read artifact {}", entry.path().display()))?;
        let text = String::from_utf8_lossy(&contents);
        assert!(
            !text.contains(secret),
            "artifact leaked secret at {}:\n{text}",
            entry.path().display()
        );
    }

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_services_delegate_to_minimax_dynamic_tool_call() -> anyhow::Result<()> {
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

    let first_supervisor = responses::mount_sse_once_match(
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
    let second_supervisor = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, DELEGATE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let third_supervisor = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message("msg-1", "Patch applied."),
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

    let output = test
        .cmd_with_server(&supervisor_server)
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MINIMAX_CODING_PLAN_KEY")
        .arg("--skip-git-repo-check")
        .arg("-s")
        .arg("danger-full-access")
        .arg("use delegate_to_minimax to fix add")
        .output()
        .context("run codex-exec")?;

    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("dynamic tool calls are not supported in exec mode"),
        "stderr unexpectedly reported unsupported dynamic tools:\n{stderr}"
    );

    let first_request = first_supervisor.single_request();
    let first_body = first_request.body_json();
    let tools = first_body["tools"]
        .as_array()
        .expect("tools array should be present");
    assert!(
        tools.iter().any(|tool| {
            tool.get("name").and_then(Value::as_str) == Some("delegate_to_minimax")
        })
    );

    let second_request = second_supervisor.single_request();
    let (delegate_output, delegate_success) = second_request
        .function_call_output_content_and_success(DELEGATE_CALL_ID)
        .expect("delegate output should be present");
    assert_ne!(delegate_success, Some(false));
    let delegate_output = delegate_output.expect("delegate output should contain text");
    let delegate_json: Value =
        serde_json::from_str(&delegate_output).expect("delegate output should be valid JSON");
    assert_eq!(delegate_json["status"], "completed");
    assert_eq!(delegate_json["format"], "apply_patch");
    assert_eq!(delegate_json["summary"], "Fix add");
    assert_eq!(
        delegate_json["patch"],
        Value::String(FIX_ADD_PATCH.trim_end().to_string())
    );
    assert!(
        delegate_json.get("diagnostics").is_none()
            || delegate_json["diagnostics"] == serde_json::json!([])
    );
    assert_eq!(
        delegate_json["context_summary"]["included_files"][0]["path"],
        "src/lib.rs"
    );

    let third_request = third_supervisor.single_request();
    let (_apply_output, apply_success) = third_request
        .function_call_output_content_and_success(APPLY_CALL_ID)
        .expect("apply_patch output should be present");
    assert_ne!(apply_success, Some(false));

    let minimax_requests = minimax_server.received_requests().await.unwrap_or_default();
    assert_eq!(minimax_requests.len(), 1);
    let minimax_body =
        String::from_utf8(minimax_requests[0].body.clone()).expect("minimax request body text");
    assert!(minimax_body.contains("Fix add so it returns the sum of both inputs."));
    assert!(minimax_body.contains("src/lib.rs"));

    let final_source = fs::read_to_string(test.cwd_path().join("src/lib.rs"))?;
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );
    run_cargo_test(test.cwd_path())?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_services_delegate_to_minimax_normalizes_patch_lines() -> anyhow::Result<()> {
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
    let second_supervisor = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, DELEGATE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message("msg-1", "Patch applied."),
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
                    r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch_lines":["*** Begin Patch","*** Update File: src/lib.rs","@@","-pub fn add(a: i32, b: i32) -> i32 { a - b }","+pub fn add(a: i32, b: i32) -> i32 { a + b }","*** End Patch"],"diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    test.cmd_with_server(&supervisor_server)
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MINIMAX_CODING_PLAN_KEY")
        .arg("--skip-git-repo-check")
        .arg("-s")
        .arg("danger-full-access")
        .arg("use delegate_to_minimax to fix add")
        .assert()
        .success();

    let second_request = second_supervisor.single_request();
    let (delegate_output, delegate_success) = second_request
        .function_call_output_content_and_success(DELEGATE_CALL_ID)
        .expect("delegate output should be present");
    assert_ne!(delegate_success, Some(false));
    let delegate_output = delegate_output.expect("delegate output should contain text");
    let delegate_json: Value =
        serde_json::from_str(&delegate_output).expect("delegate output should be valid JSON");
    assert_eq!(delegate_json["status"], "completed");
    assert_eq!(
        delegate_json["patch"],
        Value::String(FIX_ADD_PATCH.trim_end().to_string())
    );
    assert!(
        delegate_json["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics.iter().any(|item| {
                item.as_str()
                    == Some("normalized worker patch: joined patch_lines into patch string")
            }))
    );

    let final_source = fs::read_to_string(test.cwd_path().join("src/lib.rs"))?;
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );
    run_cargo_test(test.cwd_path())?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_services_delegate_to_minimax_dynamic_tool_call_clarify() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));
    let _minimax_api_key = EnvVarGuard::set("MINIMAX_API_KEY", OsStr::new("minimax-env-token"));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| !request_has_function_call_output(req, DELEGATE_CALL_ID),
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
    let second_supervisor = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, DELEGATE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_assistant_message("msg-1", "What input shape should parse_value accept?"),
            responses::ev_completed("resp-2"),
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

    test.cmd_with_server(&supervisor_server)
        .env("MINIMAX_API_KEY", "minimax-env-token")
        .env_remove("MINIMAX_CODING_PLAN_KEY")
        .arg("--skip-git-repo-check")
        .arg("-s")
        .arg("danger-full-access")
        .arg("use delegate_to_minimax to clarify parse_value")
        .assert()
        .success();

    let second_request = second_supervisor.single_request();
    let (delegate_output, delegate_success) = second_request
        .function_call_output_content_and_success(DELEGATE_CALL_ID)
        .expect("delegate output should be present");
    assert_ne!(delegate_success, Some(false));
    let delegate_output = delegate_output.expect("delegate output text");
    let delegate_json: Value =
        serde_json::from_str(&delegate_output).expect("delegate output should be valid JSON");
    assert_eq!(delegate_json["status"], "clarify");
    assert_eq!(
        delegate_json["question"],
        "What input shape should parse_value accept?"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_services_delegate_to_minimax_dynamic_tool_call_invalid() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));
    let _minimax_api_key = EnvVarGuard::set("MINIMAX_API_KEY", OsStr::new("minimax-env-token"));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| !request_has_function_call_output(req, DELEGATE_CALL_ID),
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
    let second_supervisor = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, DELEGATE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_assistant_message("msg-1", "Need more context before applying changes."),
            responses::ev_completed("resp-2"),
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

    test.cmd_with_server(&supervisor_server)
        .env("MINIMAX_API_KEY", "minimax-env-token")
        .env_remove("MINIMAX_CODING_PLAN_KEY")
        .arg("--skip-git-repo-check")
        .arg("-s")
        .arg("danger-full-access")
        .arg("use delegate_to_minimax on an underspecified change")
        .assert()
        .success();

    let second_request = second_supervisor.single_request();
    let (delegate_output, delegate_success) = second_request
        .function_call_output_content_and_success(DELEGATE_CALL_ID)
        .expect("delegate output should be present");
    assert_ne!(delegate_success, Some(false));
    let delegate_output = delegate_output.expect("delegate output text");
    let delegate_json: Value =
        serde_json::from_str(&delegate_output).expect("delegate output should be valid JSON");
    assert_eq!(delegate_json["status"], "invalid");
    assert_eq!(
        delegate_json["error"],
        "worker_response_not_json: expected JSON object with status completed/clarify/invalid"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_blocks_sensitive_shell_output_before_delegate_and_rollout() -> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    seed_committed_repo_with_sensitive_env(test.cwd_path())?;
    save_minimax_credentials(test.home_path());
    fs::write(
        test.cwd_path().join(".env"),
        format!("OPENAI_API_KEY={FAKE_SECRET}\n"),
    )?;

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| !request_has_function_call_output(req, SHELL_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_shell_command_call(SHELL_CALL_ID, "git diff -- .env"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let second_supervisor = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, SHELL_CALL_ID)
                && !request_has_function_call_output(req, DELEGATE_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &serde_json::to_string(&serde_json::json!({
                    "task_description": "Fix add so it returns the sum of both inputs.",
                    "acceptance_criteria": ["cargo test must pass"],
                    "context_files": ["src/lib.rs"],
                    "include_modified_files": true
                }))?,
            ),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let third_supervisor = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, DELEGATE_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-4"),
            responses::ev_assistant_message(
                "msg-1",
                "Patch applied after safe guardrail handling.",
            ),
            responses::ev_completed("resp-4"),
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

    let output = test
        .cmd_with_server(&supervisor_server)
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MINIMAX_CODING_PLAN_KEY")
        .arg("--json")
        .arg("--skip-git-repo-check")
        .arg("-s")
        .arg("danger-full-access")
        .arg("fix add while respecting sensitive files")
        .output()
        .context("run codex-exec")?;

    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(FAKE_SECRET),
        "stdout leaked secret:\n{stdout}"
    );
    assert!(
        !stderr.contains(FAKE_SECRET),
        "stderr leaked secret:\n{stderr}"
    );

    let blocked_request = second_supervisor.single_request();
    let (blocked_output, blocked_success) = blocked_request
        .function_call_output_content_and_success(SHELL_CALL_ID)
        .expect("shell output should be present");
    assert_ne!(blocked_success, Some(true));
    let blocked_output = blocked_output.expect("blocked shell output");
    assert_eq!(
        blocked_output,
        "blocked: command would expose sensitive file contents (.env). Use non-sensitive context or ask the user."
    );
    assert!(!blocked_output.contains(FAKE_SECRET));

    let delegate_request = third_supervisor.single_request();
    let (delegate_output, delegate_success) = delegate_request
        .function_call_output_content_and_success(DELEGATE_CALL_ID)
        .expect("delegate output should be present");
    assert_ne!(delegate_success, Some(false));
    let delegate_output = delegate_output.expect("delegate output text");
    let delegate_json: Value =
        serde_json::from_str(&delegate_output).expect("delegate output should be valid JSON");
    assert_eq!(delegate_json["status"], "completed");
    assert_eq!(
        delegate_json["diagnostics"],
        serde_json::json!(["omitted git modified file .env: denied path"])
    );
    assert_eq!(
        delegate_json["context_summary"]["included_files"][0]["path"],
        "src/lib.rs"
    );
    assert!(!delegate_output.contains(FAKE_SECRET));

    let minimax_requests = minimax_server.received_requests().await.unwrap_or_default();
    assert_eq!(minimax_requests.len(), 1);
    let minimax_body =
        String::from_utf8(minimax_requests[0].body.clone()).expect("minimax request body text");
    assert!(!minimax_body.contains(FAKE_SECRET));
    assert!(
        !minimax_body.contains(".env"),
        "denylisted file should not be sent to MiniMax: {minimax_body}"
    );

    let rollout_text = read_rollout_text(test.home_path())?;
    assert!(!rollout_text.contains(FAKE_SECRET), "rollout leaked secret");
    assert_secret_absent_from_tree(test.home_path(), FAKE_SECRET)?;

    let final_source = fs::read_to_string(test.cwd_path().join("src/lib.rs"))?;
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    );
    run_cargo_test(test.cwd_path())?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_blocks_additional_sensitive_commands_and_redacts_mixed_diff_output()
-> anyhow::Result<()> {
    const SHOW_ENV_CALL_ID: &str = "shell-show-env";
    const BASE64_ENV_CALL_ID: &str = "shell-base64-env";
    const XXD_ENV_CALL_ID: &str = "shell-xxd-env";
    const CAT_ENV_CALL_ID: &str = "shell-cat-env";
    const DIFF_CALL_ID: &str = "shell-diff";

    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    seed_committed_repo_with_sensitive_files(test.cwd_path())?;
    save_minimax_credentials(test.home_path());
    fs::write(
        test.cwd_path().join(".env"),
        format!("OPENAI_API_KEY={FAKE_SECRET}\n"),
    )?;
    fs::write(
        test.cwd_path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a - b }\n\npub fn unchanged() -> i32 { 7 }\n",
    )?;

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let minimax_base_url = format!("{minimax_server}/v1", minimax_server = minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| !request_has_function_call_output(req, SHOW_ENV_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_shell_command_call(SHOW_ENV_CALL_ID, "git show HEAD:.env"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let after_show = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, SHOW_ENV_CALL_ID)
                && !request_has_function_call_output(req, BASE64_ENV_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_shell_command_call(BASE64_ENV_CALL_ID, "base64 .env"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let after_base64 = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, BASE64_ENV_CALL_ID)
                && !request_has_function_call_output(req, XXD_ENV_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_shell_command_call(XXD_ENV_CALL_ID, "xxd .env"),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;
    let after_xxd = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, XXD_ENV_CALL_ID)
                && !request_has_function_call_output(req, CAT_ENV_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-4"),
            responses::ev_shell_command_call(CAT_ENV_CALL_ID, "bash -lc 'cat .env'"),
            responses::ev_completed("resp-4"),
        ]),
    )
    .await;
    let after_cat = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, CAT_ENV_CALL_ID)
                && !request_has_function_call_output(req, DIFF_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-5"),
            responses::ev_shell_command_call(DIFF_CALL_ID, "git diff"),
            responses::ev_completed("resp-5"),
        ]),
    )
    .await;
    let after_diff = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| {
            request_has_function_call_output(req, DIFF_CALL_ID)
                && !request_has_function_call_output(req, DELEGATE_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("resp-6"),
            responses::ev_function_call(
                DELEGATE_CALL_ID,
                "delegate_to_minimax",
                &serde_json::to_string(&serde_json::json!({
                    "task_description": "Fix add so it returns the sum of both inputs.",
                    "acceptance_criteria": ["cargo test must pass"],
                    "context_files": ["src/lib.rs"],
                    "include_modified_files": true
                }))?,
            ),
            responses::ev_completed("resp-6"),
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
            responses::ev_response_created("resp-7"),
            responses::ev_apply_patch_function_call(APPLY_CALL_ID, FIX_ADD_PATCH),
            responses::ev_completed("resp-7"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-8"),
            responses::ev_assistant_message("msg-1", "Patch applied after hardened handling."),
            responses::ev_completed("resp-8"),
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

    let output = test
        .cmd_with_server(&supervisor_server)
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MINIMAX_CODING_PLAN_KEY")
        .arg("--json")
        .arg("--skip-git-repo-check")
        .arg("-s")
        .arg("danger-full-access")
        .arg("fix add while hardening sensitive command handling")
        .output()
        .context("run codex-exec")?;

    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(FAKE_SECRET),
        "stdout leaked secret:\n{stdout}"
    );
    assert!(
        !stderr.contains(FAKE_SECRET),
        "stderr leaked secret:\n{stderr}"
    );

    for (request, call_id) in [
        (&after_show, SHOW_ENV_CALL_ID),
        (&after_base64, BASE64_ENV_CALL_ID),
        (&after_xxd, XXD_ENV_CALL_ID),
        (&after_cat, CAT_ENV_CALL_ID),
    ] {
        let request = request.single_request();
        let (tool_output, tool_success) = request
            .function_call_output_content_and_success(call_id)
            .expect("shell output should be present");
        assert_ne!(tool_success, Some(true));
        assert_eq!(
            tool_output,
            Some(
                "blocked: command would expose sensitive file contents (.env). Use non-sensitive context or ask the user.".to_string()
            )
        );
    }

    let diff_request = after_diff.single_request();
    let (diff_output, diff_success) = diff_request
        .function_call_output_content_and_success(DIFF_CALL_ID)
        .expect("diff output should be present");
    assert_ne!(diff_success, Some(false));
    let diff_output = diff_output.expect("diff output text");
    assert!(diff_output.contains("diff --git a/src/lib.rs b/src/lib.rs"));
    assert!(diff_output.contains("pub fn unchanged() -> i32 { 7 }"));
    assert!(diff_output.contains("[REDACTED_SENSITIVE_FILE_DIFF path=\".env\"]"));
    assert!(!diff_output.contains(FAKE_SECRET));

    let delegate_request = after_delegate.single_request();
    let (delegate_output, delegate_success) = delegate_request
        .function_call_output_content_and_success(DELEGATE_CALL_ID)
        .expect("delegate output should be present");
    assert_ne!(delegate_success, Some(false));
    let delegate_output = delegate_output.expect("delegate output text");
    let delegate_json: Value =
        serde_json::from_str(&delegate_output).expect("delegate output should be valid JSON");
    assert_eq!(delegate_json["status"], "completed");
    assert_eq!(
        delegate_json["diagnostics"],
        serde_json::json!(["omitted git modified file .env: denied path"])
    );
    assert_eq!(
        delegate_json["context_summary"]["included_files"][0]["path"],
        "src/lib.rs"
    );
    assert!(!delegate_output.contains(FAKE_SECRET));

    let minimax_requests = minimax_server.received_requests().await.unwrap_or_default();
    assert_eq!(minimax_requests.len(), 1);
    let minimax_body =
        String::from_utf8(minimax_requests[0].body.clone()).expect("minimax request body text");
    assert!(!minimax_body.contains(FAKE_SECRET));
    assert!(!minimax_body.contains(".env"));
    assert!(!minimax_body.contains("auth.json"));
    assert!(!minimax_body.contains("id_rsa"));

    let rollout_text = read_rollout_text(test.home_path())?;
    assert!(!rollout_text.contains(FAKE_SECRET), "rollout leaked secret");
    assert_secret_absent_from_tree(test.home_path(), FAKE_SECRET)?;

    let final_source = fs::read_to_string(test.cwd_path().join("src/lib.rs"))?;
    assert_eq!(
        final_source,
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\npub fn unchanged() -> i32 { 7 }\n"
    );
    run_cargo_test(test.cwd_path())?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_reports_clear_error_when_supervisor_calls_unregistered_tool_name()
-> anyhow::Result<()> {
    let _lock = lock_minimax_env().await;
    let _minimax_api_key = EnvVarGuard::clear("MINIMAX_API_KEY");
    let _minimax_coding_plan_key = EnvVarGuard::clear("MINIMAX_CODING_PLAN_KEY");

    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| !request_has_function_call_output(req, UNKNOWN_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(
                UNKNOWN_CALL_ID,
                "totally_unknown_dynamic_tool",
                &serde_json::json!({}).to_string(),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let second_supervisor = responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, UNKNOWN_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_assistant_message("msg-1", "The dynamic tool is not available."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    test.cmd_with_server(&supervisor_server)
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MINIMAX_CODING_PLAN_KEY")
        .arg("--skip-git-repo-check")
        .arg("-s")
        .arg("danger-full-access")
        .arg("call an unknown dynamic tool")
        .assert()
        .success();

    let second_request = second_supervisor.single_request();
    let (tool_output, tool_success) = second_request
        .function_call_output_content_and_success(UNKNOWN_CALL_ID)
        .expect("unknown tool output should be present");
    assert_ne!(tool_success, Some(true));
    assert_eq!(
        tool_output,
        Some("unsupported call: totally_unknown_dynamic_tool".to_string())
    );

    Ok(())
}
