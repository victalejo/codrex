use super::MiniMaxContextFile;
use super::MiniMaxContextFileSource;
use super::MiniMaxContextSummary;
use super::MiniMaxDelegationResult;
use super::MiniMaxDelegationStatus;
use super::WorkerPatchFormat;
use super::parse_delegate_output;
use pretty_assertions::assert_eq;
use std::fs;
use tempfile::TempDir;

fn sample_context_summary() -> MiniMaxContextSummary {
    MiniMaxContextSummary {
        included_files: vec![MiniMaxContextFile {
            path: "src/lib.rs".to_string(),
            source: MiniMaxContextFileSource::ExplicitFile,
            truncated: false,
            redacted: false,
        }],
        omitted_count: 0,
    }
}

fn write_repo_file(repo: &TempDir, relative_path: &str, contents: &str) {
    let full_path = repo.path().join(relative_path);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent).expect("create parent directories");
    }
    fs::write(full_path, contents).expect("write repo file");
}

fn parse_completed_patch(output: &str) -> MiniMaxDelegationResult {
    parse_delegate_output(output, &[], Some(sample_context_summary()))
}

#[test]
fn parse_delegate_output_accepts_patch_lines() {
    let result = parse_completed_patch(
        r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch_lines":["*** Begin Patch","*** Update File: src/lib.rs","@@","-pub fn add(a: i32, b: i32) -> i32 { a - b }","+pub fn add(a: i32, b: i32) -> i32 { a + b }","*** End Patch"],"diagnostics":[]}"#,
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Completed);
    assert_eq!(result.format, Some(WorkerPatchFormat::ApplyPatch));
    assert_eq!(
        result.patch,
        Some(
            "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch"
                .to_string(),
        ),
    );
}

#[test]
fn parse_delegate_output_accepts_fenced_json_object() {
    let result = parse_completed_patch(
        "```json\n{\"status\":\"completed\",\"format\":\"apply_patch\",\"summary\":\"Fix add\",\"patch\":\"*** Begin Patch\\n*** Update File: src/lib.rs\\n@@\\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\\n*** End Patch\",\"diagnostics\":[]}\n```",
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Completed);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item == "normalized worker response: stripped markdown fence")
    );
}

#[test]
fn parse_delegate_output_accepts_fenced_apply_patch_body() {
    let result = parse_completed_patch(
        r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"```patch\n*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch\n```","diagnostics":[]}"#,
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Completed);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item == "normalized worker patch: stripped markdown fence")
    );
}

#[test]
fn parse_delegate_output_accepts_apply_patch_heredoc_wrapper() {
    let result = parse_completed_patch(
        r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"apply_patch <<'PATCH'\n*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch\nPATCH","diagnostics":[]}"#,
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Completed);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item == "normalized worker patch: extracted apply_patch heredoc")
    );
}

#[test]
fn parse_delegate_output_normalizes_crlf() {
    let result = parse_completed_patch(
        "{\"status\":\"completed\",\"format\":\"apply_patch\",\"summary\":\"Fix add\",\"patch\":\"*** Begin Patch\\r\\n*** Update File: src/lib.rs\\r\\n@@\\r\\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\\r\\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\\r\\n*** End Patch\\r\\n\",\"diagnostics\":[]}",
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Completed);
    assert!(
        result
            .patch
            .as_ref()
            .is_some_and(|patch| !patch.contains('\r'))
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item == "normalized worker patch: normalized line endings")
    );
}

#[test]
fn parse_delegate_output_extracts_single_json_object_from_prose() {
    let result = parse_completed_patch(
        "Here is the candidate.\n{\"status\":\"completed\",\"format\":\"apply_patch\",\"summary\":\"Fix add\",\"patch\":\"*** Begin Patch\\n*** Update File: src/lib.rs\\n@@\\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\\n*** End Patch\",\"diagnostics\":[]}\nUse it directly.",
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Completed);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item == "normalized worker response: extracted single JSON object")
    );
}

#[test]
fn parse_delegate_output_rejects_multiple_json_objects() {
    let result = parse_delegate_output(
        "{\"status\":\"invalid\",\"error\":\"first\",\"diagnostics\":[]}\n{\"status\":\"invalid\",\"error\":\"second\",\"diagnostics\":[]}",
        &[],
        None,
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Invalid);
    assert_eq!(
        result.error,
        Some("ambiguous_worker_output: multiple JSON objects found".to_string()),
    );
}

#[test]
fn parse_delegate_output_rejects_missing_end_patch_marker() {
    let result = parse_delegate_output(
        r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }","diagnostics":[]}"#,
        &[],
        None,
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Invalid);
    assert_eq!(
        result.error,
        Some("invalid_apply_patch_format: missing *** End Patch".to_string()),
    );
}

#[test]
fn parse_delegate_output_rejects_missing_begin_patch_marker() {
    let result = parse_delegate_output(
        r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch","diagnostics":[]}"#,
        &[],
        None,
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Invalid);
    assert_eq!(
        result.error,
        Some("invalid_apply_patch_format: missing *** Begin Patch".to_string()),
    );
}

#[test]
fn parse_delegate_output_rejects_unified_diff_declared_as_apply_patch() {
    let result = parse_delegate_output(
        r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }","diagnostics":[]}"#,
        &[],
        None,
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Invalid);
    assert_eq!(
        result.error,
        Some(
            "invalid_apply_patch_format: patch looks like unified diff, not apply_patch"
                .to_string()
        ),
    );
}

#[test]
fn parse_delegate_output_redacts_secret_like_metadata() {
    let result = parse_delegate_output(
        r#"{"status":"invalid","error":"api_key=sk-test-secret-should-not-leak","diagnostics":["token=sk-test-secret-should-not-leak"]}"#,
        &[],
        None,
    );

    assert_eq!(result.status, MiniMaxDelegationStatus::Invalid);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| !error.contains("sk-test-secret-should-not-leak"))
    );
    assert!(
        result
            .diagnostics
            .iter()
            .all(|item| !item.contains("sk-test-secret-should-not-leak"))
    );
}

#[tokio::test]
async fn validate_delegate_result_accepts_applicable_patch_without_changing_worktree() {
    let repo = TempDir::new().expect("repo");
    write_repo_file(
        &repo,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 { a - b }\n",
    );
    let result = parse_completed_patch(
        r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch","diagnostics":[]}"#,
    );

    let validated = super::validate_delegate_result_against_worktree(result, repo.path()).await;

    assert_eq!(validated.status, MiniMaxDelegationStatus::Completed);
    assert_eq!(
        fs::read_to_string(repo.path().join("src/lib.rs")).expect("read file after validation"),
        "pub fn add(a: i32, b: i32) -> i32 { a - b }\n",
    );
}

#[tokio::test]
async fn validate_delegate_result_rejects_non_applicable_patch() {
    let repo = TempDir::new().expect("repo");
    write_repo_file(
        &repo,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
    );
    let result = parse_completed_patch(
        r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@ -1,5 +1,5 @@\n-pub fn add(a: i32, b: i32) -> i32 {\n-    a - b\n-}\n+pub fn add(a: i32, b: i32) -> i32 {\n+    a + b\n+}\n*** End Patch","diagnostics":[]}"#,
    );

    let validated = super::validate_delegate_result_against_worktree(result, repo.path()).await;

    assert_eq!(validated.status, MiniMaxDelegationStatus::Invalid);
    assert_eq!(
        validated.error,
        Some("patch_not_applicable: context did not match src/lib.rs".to_string()),
    );
}

#[tokio::test]
async fn validate_delegate_result_rejects_missing_update_target() {
    let repo = TempDir::new().expect("repo");
    let result = parse_completed_patch(
        r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn add(a: i32, b: i32) -> i32 { a - b }\n+pub fn add(a: i32, b: i32) -> i32 { a + b }\n*** End Patch","diagnostics":[]}"#,
    );

    let validated = super::validate_delegate_result_against_worktree(result, repo.path()).await;

    assert_eq!(validated.status, MiniMaxDelegationStatus::Invalid);
    assert_eq!(
        validated.error,
        Some("patch_not_applicable: update target src/lib.rs does not exist".to_string()),
    );
}

#[tokio::test]
async fn validate_delegate_result_accepts_add_file_patch() {
    let repo = TempDir::new().expect("repo");
    let result = parse_completed_patch(
        r#"{"status":"completed","format":"apply_patch","summary":"Add helper","patch":"*** Begin Patch\n*** Add File: src/helper.rs\n+pub fn helper() -> i32 { 7 }\n*** End Patch","diagnostics":[]}"#,
    );

    let validated = super::validate_delegate_result_against_worktree(result, repo.path()).await;

    assert_eq!(validated.status, MiniMaxDelegationStatus::Completed);
    assert!(
        !repo.path().join("src/helper.rs").exists(),
        "dry-run validation must not create files"
    );
}

#[tokio::test]
async fn validate_delegate_result_rejects_sensitive_paths_without_leaking() {
    let repo = TempDir::new().expect("repo");
    let result = parse_completed_patch(
        r#"{"status":"completed","format":"apply_patch","summary":"Update secret","patch":"*** Begin Patch\n*** Add File: .env\n+OPENAI_API_KEY=sk-test-secret-should-not-leak\n*** End Patch","diagnostics":[]}"#,
    );

    let validated = super::validate_delegate_result_against_worktree(result, repo.path()).await;

    assert_eq!(validated.status, MiniMaxDelegationStatus::Invalid);
    assert_eq!(
        validated.error,
        Some("blocked_sensitive_patch: patch modifies sensitive path .env".to_string()),
    );
    assert!(
        validated
            .diagnostics
            .iter()
            .all(|item| !item.contains("sk-test-secret-should-not-leak"))
    );
}
