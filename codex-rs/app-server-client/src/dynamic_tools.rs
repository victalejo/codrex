use crate::legacy_core;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use std::path::Path;

/// Executes the currently supported app-server dynamic tools and returns the
/// model-visible tool response payload.
pub async fn execute_dynamic_tool_call(
    params: DynamicToolCallParams,
    cwd: Option<&Path>,
    codex_home: &Path,
) -> DynamicToolCallResponse {
    let DynamicToolCallParams {
        thread_id,
        tool,
        arguments,
        ..
    } = params;

    if tool != legacy_core::DELEGATE_TO_MINIMAX_TOOL_NAME {
        return dynamic_tool_failure_response(format!("Unsupported dynamic tool `{tool}`."));
    }

    let request = match serde_json::from_value::<legacy_core::DelegateToMinimaxRequest>(arguments) {
        Ok(request) => request,
        Err(err) => {
            return dynamic_tool_failure_response(format!(
                "delegate_to_minimax received invalid arguments: {err}"
            ));
        }
    };

    let Some(cwd) = cwd else {
        return dynamic_tool_failure_response(format!(
            "delegate_to_minimax could not resolve a working directory for thread {thread_id}"
        ));
    };

    match legacy_core::delegate_to_minimax(request, cwd, codex_home).await {
        Ok(response) => delegate_dynamic_tool_response(response),
        Err(err) => dynamic_tool_failure_response(format!("MiniMax delegation failed: {err}")),
    }
}

pub fn delegate_dynamic_tool_response(
    response: legacy_core::DelegateToMinimaxResponse,
) -> DynamicToolCallResponse {
    DynamicToolCallResponse {
        content_items: dynamic_tool_content_items(response),
        success: true,
    }
}

pub fn dynamic_tool_failure_response(message: String) -> DynamicToolCallResponse {
    DynamicToolCallResponse {
        content_items: vec![DynamicToolCallOutputContentItem::InputText { text: message }],
        success: false,
    }
}

fn dynamic_tool_content_items(
    response: legacy_core::DelegateToMinimaxResponse,
) -> Vec<DynamicToolCallOutputContentItem> {
    let text = serde_json::to_string(&response).unwrap_or_else(|err| {
        serde_json::json!({
            "status": "invalid",
            "error": format!("failed to serialize delegate_to_minimax response: {err}"),
        })
        .to_string()
    });

    vec![DynamicToolCallOutputContentItem::InputText { text }]
}
