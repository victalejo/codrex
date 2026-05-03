use crate::minimax_delegate::DELEGATE_TO_MINIMAX_TOOL_NAME;
use crate::minimax_delegate::DelegateToMinimaxResponse;
use crate::minimax_delegate::MiniMaxDelegationStatus;
use crate::minimax_delegate::WorkerPatchFormat;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_shell_command::bash::parse_shell_lc_plain_commands;
use codex_shell_command::bash::parse_shell_lc_single_command_prefix;
use codex_shell_command::parse_command::extract_shell_command;
use codex_utils_cache::sha1_digest;
use regex_lite::Regex;
use std::path::Path;
use std::sync::OnceLock;

pub(crate) const STRICT_DELEGATION_MARKER: &str = "<strict_delegation mode=\"required\" />";
pub(crate) const STRICT_DELEGATION_APPLY_PATCH_BLOCK_MESSAGE: &str = "strict_delegation_violation: apply_patch is only allowed for validated delegate candidates returned by delegate_to_minimax.";
pub(crate) const STRICT_DELEGATION_SHELL_BLOCK_MESSAGE: &str = "blocked: strict delegation mode forbids manual file modifications via shell. Apply a completed patch candidate returned by delegate_to_minimax instead.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StrictDelegationAttemptStatus {
    Completed,
    Clarify,
    InfraError,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StrictDelegationCandidate {
    pub(crate) hash: [u8; 20],
    normalized_patch: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StrictDelegationState {
    delegate_called: bool,
    last_status: Option<StrictDelegationAttemptStatus>,
    candidates: Vec<StrictDelegationCandidate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StrictDelegationViolationReason {
    NoCompletedCandidate,
    PatchMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StrictDelegationManualAction {
    ApplyPatch,
    ShellWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[expect(
    dead_code,
    reason = "reserved strict delegation skip reasons for startup and routing diagnostics"
)]
pub(crate) enum StrictDelegationSkipReason {
    SupervisorSelectedManualPatch,
    SupervisorSelfRestrictedBeforeToolCall,
    StartupPluginSyncFailed,
    AuthRefreshFailed,
    ToolUnavailable,
    SkillsRoutingIntercepted,
    CandidateMissing,
    CandidateInvalid,
}

impl StrictDelegationSkipReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SupervisorSelectedManualPatch => "supervisor_selected_manual_patch",
            Self::SupervisorSelfRestrictedBeforeToolCall => {
                "supervisor_self_restricted_before_tool_call"
            }
            Self::StartupPluginSyncFailed => "startup_plugin_sync_failed",
            Self::AuthRefreshFailed => "auth_refresh_failed",
            Self::ToolUnavailable => "tool_unavailable",
            Self::SkillsRoutingIntercepted => "skills_routing_intercepted",
            Self::CandidateMissing => "candidate_missing",
            Self::CandidateInvalid => "candidate_invalid",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StrictDelegationTrace {
    pub(crate) delegate_called: bool,
    pub(crate) delegate_skip_reason: StrictDelegationSkipReason,
}

impl StrictDelegationTrace {
    pub(crate) fn default_for_action(action: StrictDelegationManualAction) -> Self {
        let delegate_skip_reason = match action {
            StrictDelegationManualAction::ApplyPatch => {
                StrictDelegationSkipReason::SupervisorSelectedManualPatch
            }
            StrictDelegationManualAction::ShellWrite => {
                StrictDelegationSkipReason::CandidateMissing
            }
        };
        Self {
            delegate_called: false,
            delegate_skip_reason,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StrictDelegationViolation {
    pub(crate) reason: StrictDelegationViolationReason,
    pub(crate) has_completed_candidate: bool,
    pub(crate) candidate_count: usize,
    pub(crate) delegate_trace: StrictDelegationTrace,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StrictCommandDecision {
    Allow,
    Block {
        reason: String,
        command_kind: &'static str,
    },
}

impl StrictDelegationState {
    pub(crate) fn trace_for_completed_turn(
        &self,
        tool_calls: u64,
    ) -> Option<StrictDelegationTrace> {
        if self.delegate_called || tool_calls > 0 {
            return None;
        }

        Some(StrictDelegationTrace {
            delegate_called: false,
            delegate_skip_reason:
                StrictDelegationSkipReason::SupervisorSelfRestrictedBeforeToolCall,
        })
    }

    pub(crate) fn trace_for_manual_action(
        &self,
        action: StrictDelegationManualAction,
    ) -> StrictDelegationTrace {
        let delegate_skip_reason = if !self.delegate_called {
            match action {
                StrictDelegationManualAction::ApplyPatch => {
                    StrictDelegationSkipReason::SupervisorSelectedManualPatch
                }
                StrictDelegationManualAction::ShellWrite => {
                    StrictDelegationSkipReason::CandidateMissing
                }
            }
        } else {
            match self.last_status {
                Some(StrictDelegationAttemptStatus::Invalid)
                | Some(StrictDelegationAttemptStatus::InfraError) => {
                    StrictDelegationSkipReason::CandidateInvalid
                }
                Some(StrictDelegationAttemptStatus::Clarify) => {
                    StrictDelegationSkipReason::CandidateMissing
                }
                Some(StrictDelegationAttemptStatus::Completed) if self.candidates.is_empty() => {
                    StrictDelegationSkipReason::CandidateMissing
                }
                Some(StrictDelegationAttemptStatus::Completed) => {
                    StrictDelegationSkipReason::SupervisorSelectedManualPatch
                }
                None => StrictDelegationSkipReason::CandidateMissing,
            }
        };

        StrictDelegationTrace {
            delegate_called: self.delegate_called,
            delegate_skip_reason,
        }
    }

    pub(crate) fn record_delegate_response(
        &mut self,
        tool_name: &str,
        response: &DynamicToolResponse,
    ) {
        if tool_name != DELEGATE_TO_MINIMAX_TOOL_NAME {
            return;
        }

        self.delegate_called = true;

        if !response.success {
            self.last_status = Some(StrictDelegationAttemptStatus::InfraError);
            return;
        }

        let Some(text) = first_text_output(response) else {
            self.last_status = Some(StrictDelegationAttemptStatus::Invalid);
            return;
        };

        let Ok(result) = serde_json::from_str::<DelegateToMinimaxResponse>(&text) else {
            self.last_status = Some(StrictDelegationAttemptStatus::Invalid);
            return;
        };

        self.last_status = Some(match &result.status {
            MiniMaxDelegationStatus::Completed => StrictDelegationAttemptStatus::Completed,
            MiniMaxDelegationStatus::Clarify => StrictDelegationAttemptStatus::Clarify,
            MiniMaxDelegationStatus::Invalid => StrictDelegationAttemptStatus::Invalid,
        });

        if result.status != MiniMaxDelegationStatus::Completed
            || result.format.as_ref() != Some(&WorkerPatchFormat::ApplyPatch)
        {
            return;
        }

        let Some(patch) = result.patch else {
            return;
        };
        let normalized_patch = normalize_patch_candidate(&patch);
        let hash = sha1_digest(normalized_patch.as_bytes());
        if self
            .candidates
            .iter()
            .any(|candidate| candidate.hash == hash)
        {
            return;
        }
        self.candidates.push(StrictDelegationCandidate {
            hash,
            normalized_patch,
        });
    }

    pub(crate) fn validate_apply_patch(
        &self,
        patch: &str,
    ) -> Result<(), StrictDelegationViolation> {
        if self.candidates.is_empty() {
            return Err(StrictDelegationViolation {
                reason: StrictDelegationViolationReason::NoCompletedCandidate,
                has_completed_candidate: false,
                candidate_count: 0,
                delegate_trace: self
                    .trace_for_manual_action(StrictDelegationManualAction::ApplyPatch),
            });
        }

        let normalized_patch = normalize_patch_candidate(patch);
        if self
            .candidates
            .iter()
            .any(|candidate| candidate.normalized_patch == normalized_patch)
        {
            Ok(())
        } else {
            Err(StrictDelegationViolation {
                reason: StrictDelegationViolationReason::PatchMismatch,
                has_completed_candidate: true,
                candidate_count: self.candidates.len(),
                delegate_trace: self
                    .trace_for_manual_action(StrictDelegationManualAction::ApplyPatch),
            })
        }
    }
}

pub(crate) fn strict_delegation_enabled(developer_instructions: Option<&str>) -> bool {
    developer_instructions
        .is_some_and(|instructions| instructions.contains(STRICT_DELEGATION_MARKER))
}

pub(crate) fn normalize_patch_candidate(patch: &str) -> String {
    patch
        .replace("\r\n", "\n")
        .trim_end_matches(['\n', '\r'])
        .to_string()
}

pub(crate) fn check_shell_command_allowed_in_strict_delegation(
    command: &[String],
) -> StrictCommandDecision {
    check_shell_command_allowed_with_depth(command, /*depth*/ 0)
}

pub(crate) fn check_shell_text_allowed_in_strict_delegation(
    command: &str,
) -> StrictCommandDecision {
    tokenize_shell_script(command).map_or(StrictCommandDecision::Allow, |tokens| {
        check_shell_command_allowed_in_strict_delegation(&tokens)
    })
}

fn check_shell_command_allowed_with_depth(
    command: &[String],
    depth: usize,
) -> StrictCommandDecision {
    const MAX_SHELL_WRAPPER_DEPTH: usize = 4;

    if let Some(command_kind) = detect_mutating_shell_command(command) {
        return StrictCommandDecision::Block {
            reason: STRICT_DELEGATION_SHELL_BLOCK_MESSAGE.to_string(),
            command_kind,
        };
    }

    if depth >= MAX_SHELL_WRAPPER_DEPTH {
        return StrictCommandDecision::Allow;
    }

    if let Some(inner_commands) = parse_shell_lc_plain_commands(command) {
        for inner_command in inner_commands {
            let decision = check_shell_command_allowed_with_depth(&inner_command, depth + 1);
            if decision != StrictCommandDecision::Allow {
                return decision;
            }
        }
    }

    if let Some(inner_command) = parse_shell_lc_single_command_prefix(command) {
        let decision = check_shell_command_allowed_with_depth(&inner_command, depth + 1);
        if decision != StrictCommandDecision::Allow {
            return decision;
        }
    }

    if let Some((_shell, script)) = extract_shell_command(command)
        && let Some(tokens) = tokenize_shell_script(script)
    {
        return check_shell_command_allowed_with_depth(&tokens, depth + 1);
    }

    StrictCommandDecision::Allow
}

fn detect_mutating_shell_command(command: &[String]) -> Option<&'static str> {
    if command.is_empty() {
        return None;
    }

    if command
        .iter()
        .any(|token| contains_write_redirection(token))
    {
        return Some("write_redirection");
    }

    let cmd0 = executable_basename(command.first()?.as_str())?;

    if cmd0 == "apply_patch" {
        return Some("apply_patch_via_shell");
    }

    if matches!(
        cmd0.as_str(),
        "touch"
            | "truncate"
            | "install"
            | "rm"
            | "mv"
            | "cp"
            | "mkdir"
            | "rmdir"
            | "ln"
            | "chmod"
            | "chown"
    ) {
        return Some(match cmd0.as_str() {
            "touch" | "truncate" | "install" => "file_write_utility",
            "rm" | "mv" | "cp" | "mkdir" | "rmdir" | "ln" | "chmod" | "chown" => "file_operation",
            _ => unreachable!("handled by matches! above"),
        });
    }

    if cmd0 == "tee" {
        return Some("file_write_utility");
    }

    if cmd0 == "dd" && dd_writes_output_file(command) {
        return Some("file_write_utility");
    }

    if cmd0 == "sed" && sed_edits_in_place(command) {
        return Some("in_place_edit");
    }

    if matches!(cmd0.as_str(), "perl" | "ruby") && script_edits_in_place(command) {
        return Some("in_place_edit");
    }

    if is_python_command(cmd0.as_str()) && command.iter().skip(1).any(|arg| python_writes_file(arg))
    {
        return Some("script_write");
    }

    if is_node_command(cmd0.as_str()) && command.iter().skip(1).any(|arg| node_writes_file(arg)) {
        return Some("script_write");
    }

    if cmd0 == "git" && git_mutates_worktree(command) {
        return Some("git_worktree_mutation");
    }

    None
}

fn contains_write_redirection(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| matches!(ch, '"' | '\''));
    for prefix in ["&>", "1>>", "2>>", ">>", ">|", "1>", "2>", ">"] {
        if trimmed == prefix {
            return true;
        }

        if let Some(rest) = trimmed.strip_prefix(prefix)
            && !rest.is_empty()
            && !rest.starts_with('&')
        {
            return true;
        }
    }

    false
}

fn executable_basename(raw: &str) -> Option<String> {
    let name = Path::new(raw).file_name()?.to_str()?.to_ascii_lowercase();
    #[cfg(windows)]
    {
        for suffix in [".exe", ".cmd", ".bat", ".com"] {
            if let Some(stripped) = name.strip_suffix(suffix) {
                return Some(stripped.to_string());
            }
        }
    }
    Some(name)
}

fn dd_writes_output_file(command: &[String]) -> bool {
    command
        .iter()
        .skip(1)
        .any(|arg| arg == "of" || arg.starts_with("of="))
}

fn sed_edits_in_place(command: &[String]) -> bool {
    command.iter().skip(1).any(|arg| {
        arg == "-i"
            || arg.starts_with("-i")
            || arg == "--in-place"
            || arg.starts_with("--in-place=")
    })
}

fn script_edits_in_place(command: &[String]) -> bool {
    command
        .iter()
        .skip(1)
        .any(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('i'))
}

fn is_python_command(command: &str) -> bool {
    command == "python" || command.starts_with("python")
}

fn python_writes_file(script: &str) -> bool {
    let lower = script.to_ascii_lowercase();
    python_open_write_regex().is_match(lower.as_str())
        || lower.contains(".write_text(")
        || lower.contains(".write_bytes(")
}

fn python_open_write_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"open\s*\([^)]*,\s*['"][wax][^'"]*['"]"#)
            .unwrap_or_else(|err| panic!("python write detection regex should compile: {err}"))
    })
}

fn is_node_command(command: &str) -> bool {
    command == "node" || command == "nodejs"
}

fn node_writes_file(script: &str) -> bool {
    let lower = script.to_ascii_lowercase();
    lower.contains("writefilesync(") || lower.contains("appendfilesync(")
}

fn git_mutates_worktree(command: &[String]) -> bool {
    let Some((index, subcommand)) = first_git_subcommand(command) else {
        return false;
    };

    match subcommand {
        "checkout" | "restore" | "reset" | "clean" | "apply" | "am" | "merge" | "rebase"
        | "cherry-pick" => true,
        "stash" => next_git_positional(command, index + 1)
            .is_some_and(|arg| matches!(arg, "apply" | "pop")),
        _ => false,
    }
}

fn first_git_subcommand(command: &[String]) -> Option<(usize, &str)> {
    let cmd0 = executable_basename(command.first()?.as_str())?;
    if cmd0 != "git" {
        return None;
    }

    let mut skip_next = false;
    for (index, arg) in command.iter().enumerate().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }

        let arg = arg.as_str();
        if git_global_option_with_inline_value(arg) {
            continue;
        }
        if git_global_option_with_value(arg) {
            skip_next = true;
            continue;
        }
        if arg == "--" || arg.starts_with('-') {
            continue;
        }

        return Some((index, arg));
    }

    None
}

fn next_git_positional(command: &[String], start: usize) -> Option<&str> {
    command
        .iter()
        .skip(start)
        .map(String::as_str)
        .find(|arg| *arg != "--" && !arg.starts_with('-'))
}

fn git_global_option_with_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-c"
            | "--config-env"
            | "--exec-path"
            | "--git-dir"
            | "--namespace"
            | "--super-prefix"
            | "--work-tree"
    )
}

fn git_global_option_with_inline_value(arg: &str) -> bool {
    matches!(
        arg,
        s if s.starts_with("--config-env=")
            || s.starts_with("--exec-path=")
            || s.starts_with("--git-dir=")
            || s.starts_with("--namespace=")
            || s.starts_with("--super-prefix=")
            || s.starts_with("--work-tree=")
    ) || ((arg.starts_with("-C") || arg.starts_with("-c")) && arg.len() > 2)
}

fn tokenize_shell_script(script: &str) -> Option<Vec<String>> {
    shlex::split(script).or_else(|| {
        let tokens = script
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        (!tokens.is_empty()).then_some(tokens)
    })
}

fn first_text_output(response: &DynamicToolResponse) -> Option<String> {
    response.content_items.iter().find_map(|item| match item {
        DynamicToolCallOutputContentItem::InputText { text } => Some(text.clone()),
        DynamicToolCallOutputContentItem::InputImage { .. } => None,
    })
}

#[cfg(test)]
mod tests {
    use super::STRICT_DELEGATION_MARKER;
    use super::STRICT_DELEGATION_SHELL_BLOCK_MESSAGE;
    use super::StrictCommandDecision;
    use super::StrictDelegationAttemptStatus;
    use super::StrictDelegationSkipReason;
    use super::StrictDelegationState;
    use super::StrictDelegationTrace;
    use super::check_shell_command_allowed_in_strict_delegation;
    use super::check_shell_text_allowed_in_strict_delegation;
    use super::normalize_patch_candidate;
    use super::strict_delegation_enabled;
    use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
    use codex_protocol::dynamic_tools::DynamicToolResponse;
    use pretty_assertions::assert_eq;

    fn block_kind(command: &[&str]) -> &'static str {
        let command = command
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>();
        match check_shell_command_allowed_in_strict_delegation(&command) {
            StrictCommandDecision::Allow => {
                panic!("expected strict delegation to block command: {command:?}");
            }
            StrictCommandDecision::Block { command_kind, .. } => command_kind,
        }
    }

    #[test]
    fn strict_delegation_marker_enables_mode() {
        assert!(strict_delegation_enabled(Some(STRICT_DELEGATION_MARKER)));
        assert!(!strict_delegation_enabled(Some("plain instructions")));
        assert!(!strict_delegation_enabled(None));
    }

    #[test]
    fn normalize_patch_candidate_ignores_line_endings_and_trailing_newline() {
        let unix = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch\n";
        let windows = "*** Begin Patch\r\n*** Add File: a.txt\r\n+hi\r\n*** End Patch\r\n\r\n";

        assert_eq!(
            normalize_patch_candidate(unix),
            normalize_patch_candidate(windows)
        );
    }

    #[test]
    fn record_delegate_response_tracks_apply_patch_candidates() {
        let mut state = StrictDelegationState::default();
        state.record_delegate_response(
            "delegate_to_minimax",
            &DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: r#"{"status":"completed","format":"apply_patch","summary":"Fix add","patch":"*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch","diagnostics":[]}"#.to_string(),
                }],
                success: true,
            },
        );

        assert_eq!(
            state.last_status,
            Some(StrictDelegationAttemptStatus::Completed)
        );
        assert!(
            state
                .validate_apply_patch("*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch\n")
                .is_ok()
        );
    }

    #[test]
    fn record_delegate_response_treats_transport_failure_as_infra_error() {
        let mut state = StrictDelegationState::default();
        state.record_delegate_response(
            "delegate_to_minimax",
            &DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: "MiniMax delegation failed: boom".to_string(),
                }],
                success: false,
            },
        );

        assert_eq!(
            state.last_status,
            Some(StrictDelegationAttemptStatus::InfraError)
        );
        assert!(
            state
                .validate_apply_patch("*** Begin Patch\n*** End Patch")
                .is_err()
        );
    }

    #[test]
    fn completed_turn_without_tool_calls_emits_self_restricted_trace() {
        let state = StrictDelegationState::default();

        assert_eq!(
            state.trace_for_completed_turn(/*tool_calls*/ 0),
            Some(StrictDelegationTrace {
                delegate_called: false,
                delegate_skip_reason:
                    StrictDelegationSkipReason::SupervisorSelfRestrictedBeforeToolCall,
            })
        );
        assert_eq!(state.trace_for_completed_turn(/*tool_calls*/ 1), None);
    }

    #[test]
    fn strict_delegation_blocks_mutating_shell_commands() {
        assert_eq!(
            block_kind(&["bash", "-lc", "echo hi > src/lib.rs"]),
            "write_redirection"
        );
        assert_eq!(
            block_kind(&["sh", "-c", "sed -i 's/- b/+ b/' src/lib.rs"]),
            "in_place_edit"
        );
        assert_eq!(
            block_kind(&["bash", "-lc", "printf hi | tee src/lib.rs"]),
            "file_write_utility"
        );
        assert_eq!(
            block_kind(&["python3", "-c", "open('src/lib.rs', 'w').write('hi')"]),
            "script_write"
        );
        assert_eq!(
            block_kind(&[
                "node",
                "-e",
                "require('fs').writeFileSync('src/lib.rs', 'hi')",
            ]),
            "script_write"
        );
        assert_eq!(
            block_kind(&["git", "apply", "patch.diff"]),
            "git_worktree_mutation"
        );
        assert_eq!(
            block_kind(&[
                "bash",
                "-lc",
                "apply_patch <<'EOF'\n*** Begin Patch\n*** End Patch\nEOF"
            ]),
            "apply_patch_via_shell"
        );
    }

    #[test]
    fn strict_delegation_allows_read_only_shell_commands() {
        for command in [
            vec!["cargo".to_string(), "test".to_string()],
            vec![
                "git".to_string(),
                "status".to_string(),
                "--short".to_string(),
            ],
            vec!["git".to_string(), "diff".to_string()],
            vec![
                "rg".to_string(),
                "add".to_string(),
                "src/lib.rs".to_string(),
            ],
            vec!["cat".to_string(), ".env".to_string()],
        ] {
            assert_eq!(
                check_shell_command_allowed_in_strict_delegation(&command),
                StrictCommandDecision::Allow,
                "command should remain allowed for strict delegation preflight: {command:?}"
            );
        }
    }

    #[test]
    fn strict_delegation_checks_shell_text_and_wrappers() {
        assert_eq!(
            check_shell_text_allowed_in_strict_delegation("bash -lc 'echo hi > src/lib.rs'"),
            StrictCommandDecision::Block {
                reason: STRICT_DELEGATION_SHELL_BLOCK_MESSAGE.to_string(),
                command_kind: "write_redirection",
            }
        );
        assert_eq!(
            check_shell_text_allowed_in_strict_delegation("sh -c 'sed -i s/x/y/ src/lib.rs'"),
            StrictCommandDecision::Block {
                reason: STRICT_DELEGATION_SHELL_BLOCK_MESSAGE.to_string(),
                command_kind: "in_place_edit",
            }
        );
        assert_eq!(
            check_shell_text_allowed_in_strict_delegation("git status --short"),
            StrictCommandDecision::Allow
        );
    }
}
