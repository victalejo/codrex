use super::CLARIFY_PREFIX;
use super::DelegateToMinimaxResponse;
use super::MiniMaxContextSummary;
use super::MiniMaxDelegationResult;
use super::MiniMaxDelegationStatus;
use super::WorkerPatchFormat;
use crate::sensitive_output::first_sensitive_path;
use crate::sensitive_output::sanitize_output_text;
use codex_apply_patch::ApplyPatchError;
use codex_apply_patch::Hunk;
use codex_apply_patch::MaybeApplyPatchVerified;
use codex_apply_patch::ParseError;
use codex_apply_patch::maybe_parse_apply_patch_verified;
use codex_apply_patch::parse_patch;
use codex_exec_server::LOCAL_FS;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

pub(super) fn parse_delegate_output(
    output: &str,
    context_diagnostics: &[String],
    context_summary: Option<MiniMaxContextSummary>,
) -> DelegateToMinimaxResponse {
    if output.trim().is_empty() {
        return MiniMaxDelegationResult::invalid_with_context_summary(
            "worker_response_not_json: empty response".to_string(),
            sanitize_diagnostics(context_diagnostics.iter().cloned()),
            context_summary,
        );
    }

    if let Some(question) = output.trim().strip_prefix(CLARIFY_PREFIX) {
        return MiniMaxDelegationResult::clarify(
            sanitize_metadata(question.trim()).trim().to_string(),
            sanitize_diagnostics(context_diagnostics.iter().cloned()),
            context_summary,
        );
    }

    let mut diagnostics = sanitize_diagnostics(context_diagnostics.iter().cloned());
    let normalized_output = match normalize_worker_output(output) {
        Ok(normalized) => {
            diagnostics.extend(normalized.diagnostics);
            normalized.text
        }
        Err(error) => {
            diagnostics.extend(error.diagnostics);
            return MiniMaxDelegationResult::invalid_with_context_summary(
                error.error,
                diagnostics,
                context_summary,
            );
        }
    };

    let response = match serde_json::from_str::<RawWorkerResponse>(&normalized_output) {
        Ok(response) => response,
        Err(_) => {
            if let Some(format) = detect_raw_patch_format(normalized_output.as_str()) {
                diagnostics.push(format!(
                    "worker returned raw {} patch instead of JSON",
                    format.as_str()
                ));
            }
            return MiniMaxDelegationResult::invalid_with_context_summary(
                "worker_response_not_json: expected JSON object with status completed/clarify/invalid"
                    .to_string(),
                diagnostics,
                context_summary,
            );
        }
    };

    diagnostics.extend(sanitize_diagnostics(response.diagnostics));

    match response.status {
        MiniMaxDelegationStatus::Completed => {
            let Some(format) = response.format else {
                return MiniMaxDelegationResult::invalid_with_context_summary(
                    "invalid_completed_response: missing format".to_string(),
                    diagnostics,
                    context_summary,
                );
            };

            let summary = sanitize_metadata(response.summary.unwrap_or_default().trim())
                .trim()
                .to_string();
            if summary.is_empty() {
                return MiniMaxDelegationResult::invalid_with_context_summary(
                    "invalid_completed_response: missing summary".to_string(),
                    diagnostics,
                    context_summary,
                );
            }

            let patch_source = match resolve_patch_source(response.patch, response.patch_lines) {
                Ok(patch_source) => {
                    diagnostics.extend(patch_source.diagnostics);
                    patch_source.patch
                }
                Err(error) => {
                    diagnostics.extend(error.diagnostics);
                    return MiniMaxDelegationResult::invalid_with_context_summary(
                        error.error,
                        diagnostics,
                        context_summary,
                    );
                }
            };

            let normalized_patch = match normalize_patch_for_format(&format, patch_source.as_str())
            {
                Ok(patch) => {
                    diagnostics.extend(patch.diagnostics);
                    patch.patch
                }
                Err(error) => {
                    diagnostics.extend(error.diagnostics);
                    return MiniMaxDelegationResult::invalid_with_context_summary(
                        error.error,
                        diagnostics,
                        context_summary,
                    );
                }
            };

            MiniMaxDelegationResult::completed(
                format,
                summary,
                normalized_patch,
                diagnostics,
                context_summary,
            )
        }
        MiniMaxDelegationStatus::Clarify => {
            let question = sanitize_metadata(response.question.unwrap_or_default().trim())
                .trim()
                .to_string();
            if question.is_empty() {
                return MiniMaxDelegationResult::invalid_with_context_summary(
                    "invalid_clarify_response: missing question".to_string(),
                    diagnostics,
                    context_summary,
                );
            }

            MiniMaxDelegationResult::clarify(question, diagnostics, context_summary)
        }
        MiniMaxDelegationStatus::Invalid => {
            let error = sanitize_metadata(response.error.unwrap_or_default().trim())
                .trim()
                .to_string();
            let error = if error.is_empty() {
                "invalid_worker_response: missing error".to_string()
            } else {
                error
            };
            MiniMaxDelegationResult::invalid_with_context_summary(
                error,
                diagnostics,
                context_summary,
            )
        }
    }
}

pub(super) async fn validate_delegate_result_against_worktree(
    result: DelegateToMinimaxResponse,
    cwd: &Path,
) -> DelegateToMinimaxResponse {
    if result.status != MiniMaxDelegationStatus::Completed
        || result.format.as_ref() != Some(&WorkerPatchFormat::ApplyPatch)
    {
        return result;
    }

    let Some(patch) = result.patch.as_ref() else {
        return result;
    };

    let parsed = match parse_patch(patch) {
        Ok(parsed) => parsed,
        Err(error) => {
            return invalid_from_completed_result(
                result,
                map_apply_patch_parse_error(&error),
                None,
            );
        }
    };

    if let Some(path) = first_sensitive_path(paths_from_hunks(&parsed.hunks)) {
        return invalid_from_completed_result(
            result,
            format!("blocked_sensitive_patch: patch modifies sensitive path {path}"),
            None,
        );
    }

    let cwd = match AbsolutePathBuf::try_from(cwd.to_path_buf()) {
        Ok(cwd) => cwd,
        Err(_) => {
            return invalid_from_completed_result(
                result,
                "patch_not_applicable: current working directory is not absolute".to_string(),
                None,
            );
        }
    };

    let argv = vec!["apply_patch".to_string(), patch.clone()];
    match maybe_parse_apply_patch_verified(&argv, &cwd, LOCAL_FS.as_ref(), None).await {
        MaybeApplyPatchVerified::Body(_) => result,
        MaybeApplyPatchVerified::CorrectnessError(error) => {
            let error_message = map_applicability_error(&parsed.hunks, &error);
            invalid_from_completed_result(result, error_message.clone(), Some(error_message))
        }
        MaybeApplyPatchVerified::ShellParseError(_) | MaybeApplyPatchVerified::NotApplyPatch => {
            invalid_from_completed_result(
                result,
                "invalid_apply_patch_format: patch could not be parsed as apply_patch".to_string(),
                None,
            )
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkerResponse {
    status: MiniMaxDelegationStatus,
    #[serde(default)]
    format: Option<WorkerPatchFormat>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    patch: Option<String>,
    #[serde(default)]
    patch_lines: Option<Vec<String>>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    diagnostics: Vec<String>,
}

struct NormalizedText {
    text: String,
    diagnostics: Vec<String>,
}

struct NormalizationFailure {
    error: String,
    diagnostics: Vec<String>,
}

struct ResolvedPatchSource {
    patch: String,
    diagnostics: Vec<String>,
}

fn sanitize_metadata(text: &str) -> String {
    sanitize_output_text(text).text
}

fn sanitize_diagnostics(items: impl IntoIterator<Item = String>) -> Vec<String> {
    items
        .into_iter()
        .map(|item| sanitize_metadata(item.as_str()))
        .collect()
}

fn normalize_worker_output(output: &str) -> Result<NormalizedText, NormalizationFailure> {
    let mut diagnostics = Vec::new();
    let mut text = output.trim().to_string();

    if text.contains("\r\n") {
        text = text.replace("\r\n", "\n");
        diagnostics.push("normalized worker response: normalized line endings".to_string());
    }

    if let Some(stripped) = strip_markdown_fence(text.as_str()) {
        text = stripped.trim().to_string();
        diagnostics.push("normalized worker response: stripped markdown fence".to_string());
    }

    match extract_single_json_object(text.as_str()) {
        Ok(Some(extracted)) if extracted != text => {
            diagnostics
                .push("normalized worker response: extracted single JSON object".to_string());
            text = extracted;
        }
        Ok(Some(_)) | Ok(None) => {}
        Err(error) => {
            return Err(NormalizationFailure { error, diagnostics });
        }
    }

    Ok(NormalizedText { text, diagnostics })
}

fn resolve_patch_source(
    patch: Option<String>,
    patch_lines: Option<Vec<String>>,
) -> Result<ResolvedPatchSource, NormalizationFailure> {
    match (patch, patch_lines) {
        (Some(patch), Some(patch_lines)) if !patch.trim().is_empty() => {
            let diagnostics =
                vec!["normalized worker patch: ignored patch_lines because patch string was also provided".to_string()];
            Ok(ResolvedPatchSource { patch, diagnostics })
        }
        (Some(patch), Some(patch_lines)) if patch.trim().is_empty() && !patch_lines.is_empty() => {
            Ok(ResolvedPatchSource {
                patch: patch_lines.join("\n"),
                diagnostics: vec![
                    "normalized worker patch: joined patch_lines into patch string".to_string(),
                ],
            })
        }
        (Some(patch), _) if !patch.trim().is_empty() => Ok(ResolvedPatchSource {
            patch,
            diagnostics: Vec::new(),
        }),
        (_, Some(patch_lines)) if !patch_lines.is_empty() => Ok(ResolvedPatchSource {
            patch: patch_lines.join("\n"),
            diagnostics: vec![
                "normalized worker patch: joined patch_lines into patch string".to_string(),
            ],
        }),
        _ => Err(NormalizationFailure {
            error: "invalid_completed_response: missing patch".to_string(),
            diagnostics: Vec::new(),
        }),
    }
}

fn normalize_patch_for_format(
    format: &WorkerPatchFormat,
    patch: &str,
) -> Result<ResolvedPatchSource, NormalizationFailure> {
    match format {
        WorkerPatchFormat::ApplyPatch => normalize_apply_patch_text(patch),
        WorkerPatchFormat::UnifiedDiff => normalize_unified_diff_text(patch),
    }
}

fn normalize_apply_patch_text(patch: &str) -> Result<ResolvedPatchSource, NormalizationFailure> {
    let mut diagnostics = Vec::new();
    let mut text = patch.trim().to_string();

    if text.contains("\r\n") {
        text = text.replace("\r\n", "\n");
        diagnostics.push("normalized worker patch: normalized line endings".to_string());
    }

    loop {
        if let Some(stripped) = strip_markdown_fence(text.as_str()) {
            text = stripped.trim().to_string();
            diagnostics.push("normalized worker patch: stripped markdown fence".to_string());
            continue;
        }

        if let Some((extracted, diagnostic)) = extract_wrapped_apply_patch(text.as_str()) {
            text = extracted.trim().to_string();
            diagnostics.push(diagnostic.to_string());
            continue;
        }

        break;
    }

    if looks_like_unified_diff(text.as_str()) {
        return Err(NormalizationFailure {
            error: "invalid_apply_patch_format: patch looks like unified diff, not apply_patch"
                .to_string(),
            diagnostics,
        });
    }

    if text.contains("```") {
        return Err(NormalizationFailure {
            error: "invalid_apply_patch_format: patch contains markdown fence".to_string(),
            diagnostics,
        });
    }

    if text.contains("apply_patch <<") {
        return Err(NormalizationFailure {
            error: "invalid_apply_patch_format: patch contains apply_patch heredoc wrapper"
                .to_string(),
            diagnostics,
        });
    }

    match parse_patch(text.as_str()) {
        Ok(parsed) => {
            if parsed.hunks.is_empty() {
                return Err(NormalizationFailure {
                    error: "invalid_apply_patch_format: patch contained no file operations"
                        .to_string(),
                    diagnostics,
                });
            }

            Ok(ResolvedPatchSource {
                patch: text,
                diagnostics,
            })
        }
        Err(error) => Err(NormalizationFailure {
            error: map_apply_patch_parse_error(&error),
            diagnostics,
        }),
    }
}

fn normalize_unified_diff_text(patch: &str) -> Result<ResolvedPatchSource, NormalizationFailure> {
    let mut diagnostics = Vec::new();
    let mut text = patch.trim().to_string();

    if text.contains("\r\n") {
        text = text.replace("\r\n", "\n");
        diagnostics.push("normalized worker patch: normalized line endings".to_string());
    }

    if let Some(stripped) = strip_markdown_fence(text.as_str()) {
        text = stripped.trim().to_string();
        diagnostics.push("normalized worker patch: stripped markdown fence".to_string());
    }

    if looks_like_unified_diff(text.as_str()) {
        Ok(ResolvedPatchSource {
            patch: text,
            diagnostics,
        })
    } else {
        Err(NormalizationFailure {
            error: "invalid_unified_diff_format: expected diff --git, ---/+++, or @@ markers"
                .to_string(),
            diagnostics,
        })
    }
}

fn strip_markdown_fence(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return None;
    }

    let first_newline = trimmed.find('\n')?;
    let last_newline = trimmed.rfind('\n')?;
    if last_newline <= first_newline {
        return None;
    }

    let last_line = trimmed[last_newline + 1..].trim();
    if last_line != "```" {
        return None;
    }

    Some(trimmed[first_newline + 1..last_newline].to_string())
}

fn extract_single_json_object(text: &str) -> Result<Option<String>, String> {
    let mut objects = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'{' {
            index += 1;
            continue;
        }

        let Some(end) = find_balanced_json_object_end(text, index) else {
            break;
        };
        let candidate = &text[index..end];
        if serde_json::from_str::<Value>(candidate)
            .ok()
            .is_some_and(|value| value.is_object())
        {
            objects.push(candidate.to_string());
        }
        index = end;
    }

    match objects.as_slice() {
        [] => Ok(None),
        [object] => Ok(Some(object.clone())),
        _ => Err("ambiguous_worker_output: multiple JSON objects found".to_string()),
    }
}

fn find_balanced_json_object_end(text: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(start + index + ch.len_utf8());
                }
            }
            _ => {}
        }
    }

    None
}

fn extract_wrapped_apply_patch(text: &str) -> Option<(String, &'static str)> {
    let begin_marker = "*** Begin Patch";
    let end_marker = "*** End Patch";
    if text.matches(begin_marker).count() != 1 || text.matches(end_marker).count() != 1 {
        return None;
    }

    let begin_index = text.find(begin_marker)?;
    let end_index = text.rfind(end_marker)?;
    let end_offset = end_index + end_marker.len();
    let prefix = text[..begin_index].trim();
    let suffix = text[end_offset..].trim();
    if prefix.is_empty() && suffix.is_empty() {
        return None;
    }

    let diagnostic = if text.contains("apply_patch <<") {
        "normalized worker patch: extracted apply_patch heredoc"
    } else {
        "normalized worker patch: extracted patch body"
    };

    Some((text[begin_index..end_offset].to_string(), diagnostic))
}

fn detect_raw_patch_format(text: &str) -> Option<WorkerPatchFormat> {
    if text.contains("*** Begin Patch")
        || text.contains("*** Update File: ")
        || text.contains("*** Add File: ")
        || text.contains("*** Delete File: ")
        || text.contains("apply_patch <<")
    {
        return Some(WorkerPatchFormat::ApplyPatch);
    }

    looks_like_unified_diff(text).then_some(WorkerPatchFormat::UnifiedDiff)
}

fn looks_like_unified_diff(text: &str) -> bool {
    text.contains("diff --git") || text.contains("--- ") && text.contains("+++ ")
}

fn map_apply_patch_parse_error(error: &ParseError) -> String {
    match error {
        ParseError::InvalidPatchError(message)
            if message.contains("first line of the patch must be '*** Begin Patch'") =>
        {
            "invalid_apply_patch_format: missing *** Begin Patch".to_string()
        }
        ParseError::InvalidPatchError(message)
            if message.contains("last line of the patch must be '*** End Patch'") =>
        {
            "invalid_apply_patch_format: missing *** End Patch".to_string()
        }
        ParseError::InvalidHunkError { message, .. } if message.contains("'--- ") => {
            "invalid_apply_patch_format: patch looks like unified diff, not apply_patch".to_string()
        }
        ParseError::InvalidHunkError { message, .. }
            if message.contains("not a valid hunk header") =>
        {
            "invalid_apply_patch_format: malformed hunk header".to_string()
        }
        ParseError::InvalidHunkError { message, .. } => {
            format!(
                "invalid_apply_patch_format: {}",
                sanitize_metadata(message.as_str())
            )
        }
        ParseError::InvalidPatchError(message) => {
            format!(
                "invalid_apply_patch_format: {}",
                sanitize_metadata(message.as_str())
            )
        }
    }
}

fn invalid_from_completed_result(
    mut result: DelegateToMinimaxResponse,
    error: String,
    diagnostic: Option<String>,
) -> DelegateToMinimaxResponse {
    result.status = MiniMaxDelegationStatus::Invalid;
    result.format = None;
    result.summary = None;
    result.patch = None;
    result.question = None;
    result.error = Some(sanitize_metadata(error.as_str()));
    if let Some(diagnostic) = diagnostic {
        result
            .diagnostics
            .push(sanitize_metadata(diagnostic.as_str()));
    }
    result
}

fn paths_from_hunks(hunks: &[Hunk]) -> Vec<&Path> {
    let mut paths = Vec::new();
    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => paths.push(path.as_path()),
            Hunk::UpdateFile {
                path, move_path, ..
            } => {
                paths.push(path.as_path());
                if let Some(move_path) = move_path {
                    paths.push(move_path.as_path());
                }
            }
        }
    }
    paths
}

fn first_update_path(hunks: &[Hunk]) -> Option<String> {
    hunks.iter().find_map(|hunk| match hunk {
        Hunk::UpdateFile { path, .. } => Some(path.display().to_string()),
        Hunk::AddFile { .. } | Hunk::DeleteFile { .. } => None,
    })
}

fn map_applicability_error(hunks: &[Hunk], error: &ApplyPatchError) -> String {
    match error {
        ApplyPatchError::ComputeReplacements(_) => {
            let path = first_update_path(hunks).unwrap_or_else(|| "patch target".to_string());
            format!("patch_not_applicable: context did not match {path}")
        }
        ApplyPatchError::IoError(io_error) => {
            let path = first_update_path(hunks).unwrap_or_else(|| "patch target".to_string());
            let message = io_error.to_string();
            if message.contains("No such file or directory") {
                format!("patch_not_applicable: update target {path} does not exist")
            } else {
                format!("patch_not_applicable: could not read {path}")
            }
        }
        ApplyPatchError::ParseError(error) => map_apply_patch_parse_error(error),
        ApplyPatchError::ImplicitInvocation => {
            "invalid_apply_patch_format: patch could not be parsed as apply_patch".to_string()
        }
    }
}
