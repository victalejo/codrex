use crate::minimax_delegate::DELEGATE_TO_MINIMAX_TOOL_NAME;
use crate::minimax_delegate::DelegateToMinimaxResponse;
use crate::minimax_delegate::MiniMaxDelegationStatus;
use crate::minimax_delegate::WorkerPatchFormat;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_utils_cache::sha1_digest;

pub(crate) const STRICT_DELEGATION_MARKER: &str = "<strict_delegation mode=\"required\" />";
pub(crate) const STRICT_DELEGATION_BLOCK_MESSAGE: &str = "blocked: strict delegation mode requires applying a completed patch candidate returned by delegate_to_minimax. No matching candidate is available.";

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StrictDelegationViolation {
    pub(crate) reason: StrictDelegationViolationReason,
    pub(crate) has_completed_candidate: bool,
    pub(crate) candidate_count: usize,
}

impl StrictDelegationState {
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

fn first_text_output(response: &DynamicToolResponse) -> Option<String> {
    response.content_items.iter().find_map(|item| match item {
        DynamicToolCallOutputContentItem::InputText { text } => Some(text.clone()),
        DynamicToolCallOutputContentItem::InputImage { .. } => None,
    })
}

#[cfg(test)]
mod tests {
    use super::STRICT_DELEGATION_MARKER;
    use super::StrictDelegationAttemptStatus;
    use super::StrictDelegationState;
    use super::normalize_patch_candidate;
    use super::strict_delegation_enabled;
    use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
    use codex_protocol::dynamic_tools::DynamicToolResponse;

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
}
