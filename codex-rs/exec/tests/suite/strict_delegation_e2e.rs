#![cfg(not(target_os = "windows"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use anyhow::Context;
use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use walkdir::WalkDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DELEGATE_CALL_ID: &str = "delegate-call";
const APPLY_CALL_ID: &str = "apply-call";
const SHELL_WRITE_CALL_ID: &str = "shell-write-call";
const FIX_ADD_PATCH: &str = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch\n";
const NON_APPLICABLE_ADD_PATCH: &str = "*** Begin Patch\n*** Update File: src/lib.rs\n@@ -1,5 +1,5 @@\n-pub fn add(a: i32, b: i32) -> i32 {\n-    a - b\n-}\n+pub fn add(a: i32, b: i32) -> i32 {\n+    a + b\n+}\n*** End Patch\n";
const STRICT_APPLY_BLOCK_MESSAGE: &str = "strict_delegation_violation: apply_patch is only allowed for validated delegate candidates returned by delegate_to_minimax.";
const STRICT_SHELL_BLOCK_MESSAGE: &str = "blocked: strict delegation mode forbids manual file modifications via shell. Apply a completed patch candidate returned by delegate_to_minimax instead.";
const SHELL_REWRITE_LIB_RS_COMMAND: &str =
    "cat <<'EOF' > src/lib.rs\npub fn add(a: i32, b: i32) -> i32 { a + b }\nEOF\n";

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

fn request_json(request: &wiremock::Request) -> Option<Value> {
    serde_json::from_slice(&request.body).ok()
}

fn request_has_function_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    request_json(request)
        .as_ref()
        .and_then(|body| body.get("input"))
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

fn run_exec_with_mock_delegate(
    test: &core_test_support::test_codex_exec::TestCodexExecBuilder,
    supervisor_server: &wiremock::MockServer,
    minimax_server: &wiremock::MockServer,
    prompt: &str,
) -> anyhow::Result<std::process::Output> {
    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let mut cmd = test.cmd_with_server(supervisor_server);
    cmd.arg("--skip-git-repo-check")
        .arg("--strict-delegation")
        .arg("-s")
        .arg("danger-full-access")
        .arg(prompt)
        .env("MINIMAX_API_KEY", "minimax-test-token")
        .env("MINIMAX_BASE_URL", minimax_base_url)
        .env_remove("MINIMAX_CODING_PLAN_KEY")
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

fn read_rollout_strings(home_path: &Path) -> anyhow::Result<Vec<String>> {
    let sessions_dir = home_path.join("sessions");
    let mut values = Vec::new();
    for entry in WalkDir::new(&sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() || !entry.file_name().to_string_lossy().ends_with(".jsonl")
        {
            continue;
        }

        for line in fs::read_to_string(entry.path())
            .with_context(|| format!("read rollout {}", entry.path().display()))?
            .lines()
        {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            collect_json_strings(&serde_json::from_str::<Value>(trimmed)?, &mut values);
        }
    }
    Ok(values)
}

fn collect_json_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect_json_strings(item, out)),
        Value::Object(map) => map
            .values()
            .for_each(|item| collect_json_strings(item, out)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn assert_rollout_contains_trace(
    home_path: &Path,
    delegate_called: bool,
    skip_reason: &str,
) -> anyhow::Result<()> {
    let strings = read_rollout_strings(home_path)?;
    let trace = strings
        .iter()
        .find(|text| text.contains("strict_delegation_trace:"))
        .cloned()
        .expect("strict delegation trace should be persisted");
    assert!(
        trace.contains("\"type\":\"strict_delegation_trace\""),
        "missing trace type in trace: {trace}"
    );
    assert!(
        trace.contains(&format!("\"delegate_called\":{delegate_called}")),
        "missing delegate_called={delegate_called} in trace: {trace}"
    );
    assert!(
        trace.contains(&format!("\"delegate_skip_reason\":\"{skip_reason}\"")),
        "missing skip reason `{skip_reason}` in trace: {trace}"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_e2e_applies_patch_lines_candidate() -> anyhow::Result<()> {
    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
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
                    r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch_lines":["*** Begin Patch","*** Update File: src/lib.rs","@@","-pub fn add(a: i32, b: i32) -> i32 { a - b }","+pub fn add(a: i32, b: i32) -> i32 { a + b }","*** End Patch"],"diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec_with_mock_delegate(
        &test,
        &supervisor_server,
        &minimax_server,
        "fix add with strict delegation",
    )?;
    assert!(
        output.status.success(),
        "codex-exec failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let delegate_request = after_delegate.single_request();
    let delegate_body = delegate_request.body_json();
    let tools = delegate_body
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array should be present");
    assert!(
        tools.iter().any(|tool| {
            tool.get("name").and_then(Value::as_str) == Some("delegate_to_minimax")
        }),
        "delegate_to_minimax should be registered in strict mode"
    );

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

    let supervisor_requests = supervisor_server
        .received_requests()
        .await
        .unwrap_or_default();
    assert!(
        !supervisor_requests
            .iter()
            .any(|request| request_has_function_call_output(request, SHELL_WRITE_CALL_ID)),
        "strict delegation valid flow should not need shell writes"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_e2e_applies_fenced_json_candidate() -> anyhow::Result<()> {
    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
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
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message("msg-1", "Applied the fenced JSON candidate."),
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
                    "```json\n{\"status\":\"completed\",\"format\":\"apply_patch\",\"summary\":\"Fix add\",\"patch\":\"*** Begin Patch\\n*** Update File: src/lib.rs\\n@@\\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\\n*** End Patch\",\"diagnostics\":[]}\n```",
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec_with_mock_delegate(
        &test,
        &supervisor_server,
        &minimax_server,
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
                item.as_str() == Some("normalized worker response: stripped markdown fence")
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
async fn strict_delegation_e2e_applies_fenced_patch_candidate() -> anyhow::Result<()> {
    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
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
    responses::mount_sse_once_match(
        &supervisor_server,
        |req: &wiremock::Request| request_has_function_call_output(req, APPLY_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("resp-3"),
            responses::ev_assistant_message("msg-1", "Applied the fenced patch candidate."),
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
                    r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"```patch\n*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch\n```","diagnostics":[]}"#,
                )),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec_with_mock_delegate(
        &test,
        &supervisor_server,
        &minimax_server,
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
                item.as_str() == Some("normalized worker patch: stripped markdown fence")
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
async fn strict_delegation_e2e_rejects_patch_not_applicable_before_apply() -> anyhow::Result<()> {
    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
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
                "Strict delegation blocked fallback after a non-applicable candidate.",
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
                .set_body_string(minimax_stream_body(&format!(
                    r#"{{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"{}","diagnostics":[]}}"#,
                    NON_APPLICABLE_ADD_PATCH
                        .trim_end()
                        .replace('\\', "\\\\")
                        .replace('\n', "\\n")
                        .replace('"', "\\\"")
                ))),
        )
        .expect(1)
        .mount(&minimax_server)
        .await;

    let output = run_exec_with_mock_delegate(
        &test,
        &supervisor_server,
        &minimax_server,
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
    assert_eq!(apply_output.as_deref(), Some(STRICT_APPLY_BLOCK_MESSAGE));
    assert_file_unchanged(&test);
    assert_rollout_contains_trace(test.home_path(), true, "candidate_invalid")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_delegation_e2e_blocks_manual_fallback_after_invalid_delegate() -> anyhow::Result<()>
{
    let test = test_codex_exec();
    init_add_repo(test.cwd_path())?;

    let supervisor_server = responses::start_mock_server().await;
    let minimax_server = MockServer::start().await;
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
                "Strict delegation blocked shell fallback after invalid delegate output.",
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

    let output = run_exec_with_mock_delegate(
        &test,
        &supervisor_server,
        &minimax_server,
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

    let shell_request = after_shell.single_request();
    let (shell_output, shell_success) = apply_output(&shell_request, SHELL_WRITE_CALL_ID);
    assert_ne!(shell_success, Some(true));
    assert_eq!(shell_output.as_deref(), Some(STRICT_SHELL_BLOCK_MESSAGE));
    assert_file_unchanged(&test);
    assert_rollout_contains_trace(test.home_path(), true, "candidate_invalid")?;

    Ok(())
}
