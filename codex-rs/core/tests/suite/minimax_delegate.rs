#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::Result;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::DelegateToMinimaxRequest;
use codex_core::MiniMaxDelegationStatus;
use codex_core::delegate_to_minimax;
use codex_core::delegate_to_minimax_dynamic_tool;
use codex_login::ProviderCredentials;
use codex_login::save_provider_credentials;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_apply_patch_function_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use serial_test::serial;
use std::ffi::OsStr;
use std::ffi::OsString;
use wiremock::MockServer;
use wiremock::matchers::method;
use wiremock::matchers::path;

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

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn call_output(req: &ResponsesRequest, call_id: &str) -> (String, Option<bool>) {
    let (content_opt, success) = req
        .function_call_output_content_and_success(call_id)
        .expect("function_call_output should exist");
    let content = content_opt.expect("function_call_output should contain text");
    (content, success)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(minimax_delegate_e2e)]
async fn supervisor_can_delegate_to_minimax_and_apply_patch_candidate() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let minimax_server = MockServer::start().await;
    let delegate_call_id = "delegate-minimax-1";
    let apply_call_id = "apply-patch-1";
    let delegate_args = json!({
        "task_description": "Change add to return a + b.",
        "acceptance_criteria": [
            "Keep the existing function signature.",
            "Only update src/lib.rs."
        ],
        "context_files": ["src/lib.rs"]
    })
    .to_string();
    let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 {\n-    a - b\n-}\n+pub fn add(a: i32, b: i32) -> i32 {\n+    a + b\n+}\n*** End Patch";

    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(delegate_call_id, "delegate_to_minimax", &delegate_args),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_apply_patch_function_call(apply_call_id, patch),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-1", "Applied the reviewed MiniMax patch."),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    let minimax_body = nonstreaming_chat_response_body(
        &json!({
            "status": "completed",
            "format": "apply_patch",
            "summary": "Implement add",
            "patch": patch,
            "diagnostics": []
        })
        .to_string(),
    );
    wiremock::Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(minimax_body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&minimax_server)
        .await;

    let minimax_base_url = format!("{}/v1", minimax_server.uri());
    let _minimax_base_url = EnvVarGuard::set("MINIMAX_BASE_URL", OsStr::new(&minimax_base_url));

    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.include_apply_patch_tool = true;
    });
    let base_test = builder.build(&server).await?;
    save_provider_credentials(
        base_test.codex_home_path(),
        AuthCredentialsStoreMode::File,
        "minimax",
        ProviderCredentials {
            api_key: "minimax-file-token".to_string(),
            kind: Some("coding_plan".to_string()),
            last_verified: None,
        },
    )?;
    std::fs::create_dir_all(base_test.workspace_path("src"))?;
    std::fs::write(
        base_test.workspace_path("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
    )?;
    let new_thread = base_test
        .thread_manager
        .start_thread_with_tools(
            base_test.config.clone(),
            vec![delegate_to_minimax_dynamic_tool()],
            /*persist_extended_history*/ false,
        )
        .await?;
    let mut test = base_test;
    test.codex = new_thread.thread;
    test.session_configured = new_thread.session_configured;

    test.submit_turn("Delegate the mechanical edit to MiniMax, review it, then apply it.")
        .await?;

    let EventMsg::DynamicToolCallRequest(request) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::DynamicToolCallRequest(_))
    })
    .await
    else {
        unreachable!("event guard guarantees DynamicToolCallRequest");
    };

    assert_eq!(request.call_id, delegate_call_id);
    assert_eq!(request.tool, "delegate_to_minimax");
    let delegate_request: DelegateToMinimaxRequest =
        serde_json::from_value(request.arguments.clone())?;
    assert_eq!(delegate_request.task_description, "Change add to return a + b.");
    assert_eq!(delegate_request.context_files, vec!["src/lib.rs".to_string()]);

    let delegate_result = delegate_to_minimax(
        delegate_request,
        test.cwd_path(),
        test.codex_home_path(),
    )
    .await?;
    assert_eq!(delegate_result.status, MiniMaxDelegationStatus::Completed);
    let delegate_result_json = serde_json::to_string(&delegate_result)?;

    test.codex
        .submit(Op::DynamicToolResponse {
            id: request.call_id,
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: delegate_result_json.clone(),
                }],
                success: true,
            },
        })
        .await?;

    wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);

    let first_request_tools = tool_names(&requests[0].body_json());
    assert!(
        first_request_tools
            .iter()
            .any(|name| name == "delegate_to_minimax"),
        "supervisor request should expose delegate_to_minimax: {first_request_tools:?}"
    );

    let (delegate_output, success_flag) = call_output(&requests[1], delegate_call_id);
    assert_eq!(success_flag, Some(true));
    let delegate_output_json: Value = serde_json::from_str(&delegate_output)?;
    assert_eq!(delegate_output_json["status"], "completed");
    assert_eq!(delegate_output_json["summary"], "Implement add");
    assert_eq!(delegate_output_json["patch"], patch);

    let (apply_output, apply_success_flag) = call_output(&requests[2], apply_call_id);
    assert_eq!(apply_success_flag, Some(true));
    assert!(
        apply_output.contains("Updated the following files"),
        "apply_patch output should come from the normal tool flow: {apply_output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(test.workspace_path("src/lib.rs"))?,
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"
    );

    Ok(())
}
