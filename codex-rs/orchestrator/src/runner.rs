//! Orchestration loop that wires `DispatchSink`, `Auditor`, and
//! `DecisionLog` together.
//!
//! This module owns the retry/escalate/drop control flow:
//! - Dispatch through a sink
//! - Audit the response
//! - Loop on `AuditDecision::Retry` up to `DelegationSpec.max_retries`
//! - Detect repeated error signatures and drop on likely loops
//! - Emit consistent JSONL rows for every attempt and decision

use crate::context::DelegationContext;
use crate::decision::AuditDecision;
use crate::decision::error_signature_for_retry_feedback;
use crate::spec::DelegationSpec;
use crate::traits::Auditor;
use crate::traits::DecisionLog;
use crate::traits::DispatchError;
use crate::traits::DispatchSink;
use crate::traits::LogStage;

const REPEATED_ERROR_LOOP_THRESHOLD: u8 = 2;
const CLARIFY_RATIONALE: &str = "model requested clarification before generating output";

#[derive(Debug, Clone)]
pub enum OrchestrateOutcome {
    Ok {
        response_text: String,
    },
    Clarify {
        question: String,
    },
    Escalate {
        reason: String,
        blocking_issue: String,
        attempts_exhausted: Option<u8>,
    },
    Drop {
        reason: String,
        repeated_signature: Option<String>,
    },
}

impl OrchestrateOutcome {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Ok { .. } => 0,
            Self::Clarify { .. } => 4,
            Self::Escalate { .. } => 2,
            Self::Drop { .. } => 3,
        }
    }
}

fn clarify_decision_payload(question: &str) -> serde_json::Value {
    let mut payload = serde_json::to_value(AuditDecision::Clarify {
        question: question.to_string(),
    })
    .expect("clarify decision should serialize");
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "rationale".to_string(),
            serde_json::Value::String(CLARIFY_RATIONALE.to_string()),
        );
    }
    payload
}

pub async fn run_orchestration_loop(
    spec: &DelegationSpec,
    model: &str,
    sink: &impl DispatchSink,
    auditor: &impl Auditor,
    log: &impl DecisionLog,
) -> std::result::Result<OrchestrateOutcome, String> {
    let mut ctx = DelegationContext::for_top_level(spec);
    let mut repeated_signature: Option<String> = None;
    let mut repeated_count: u8 = 0;

    loop {
        log.record(
            &ctx,
            LogStage::DispatchStart,
            serde_json::json!({"provider": "minimax", "model": model}),
        )
        .await;

        let outcome = match sink.dispatch(spec, &ctx).await {
            Ok(o) => o,
            Err(DispatchError::ClarificationRequested { question }) => {
                log.record(
                    &ctx,
                    LogStage::Clarify,
                    serde_json::json!({"question": question, "handled": false}),
                )
                .await;
                log.record(
                    &ctx,
                    LogStage::Decision,
                    clarify_decision_payload(&question),
                )
                .await;
                return Ok(OrchestrateOutcome::Clarify { question });
            }
            Err(other) => {
                log.record(
                    &ctx,
                    LogStage::DispatchEnd,
                    serde_json::json!({"error": other.to_string()}),
                )
                .await;
                return Err(format!("dispatch failed: {other}"));
            }
        };

        log.record(
            &ctx,
            LogStage::DispatchEnd,
            serde_json::json!({
                "latency_ms": outcome.latency_ms,
                "total_tokens": outcome.total_tokens,
                "response_len": outcome.response_text.len(),
            }),
        )
        .await;

        let report = auditor.audit(spec, &ctx, &outcome).await;
        for criterion in &report.criterion_results {
            log.record(
                &ctx,
                LogStage::AuditCriterion,
                serde_json::json!({
                    "name": criterion.name,
                    "passed": criterion.passed,
                    "duration_ms": criterion.duration_ms,
                    "details": criterion.details,
                }),
            )
            .await;
        }
        log.record(
            &ctx,
            LogStage::Audit,
            serde_json::to_value(&report.decision).unwrap_or_else(|_| serde_json::json!({})),
        )
        .await;

        match report.decision {
            AuditDecision::Ok { .. } => {
                log.record(
                    &ctx,
                    LogStage::Decision,
                    serde_json::json!({"verdict": "ok"}),
                )
                .await;
                return Ok(OrchestrateOutcome::Ok {
                    response_text: outcome.response_text,
                });
            }
            AuditDecision::Escalate {
                reason,
                blocking_issue,
            } => {
                log.record(
                    &ctx,
                    LogStage::Decision,
                    serde_json::json!({
                        "verdict": "escalate",
                        "reason": reason,
                        "blocking_issue": blocking_issue,
                    }),
                )
                .await;
                return Ok(OrchestrateOutcome::Escalate {
                    reason,
                    blocking_issue,
                    attempts_exhausted: None,
                });
            }
            AuditDecision::Clarify { question } => {
                log.record(
                    &ctx,
                    LogStage::Decision,
                    clarify_decision_payload(&question),
                )
                .await;
                return Ok(OrchestrateOutcome::Clarify { question });
            }
            AuditDecision::Drop { reason } => {
                log.record(
                    &ctx,
                    LogStage::Decision,
                    serde_json::json!({
                        "verdict": "drop",
                        "reason": reason,
                    }),
                )
                .await;
                return Ok(OrchestrateOutcome::Drop {
                    reason,
                    repeated_signature: None,
                });
            }
            AuditDecision::Retry { feedback, attempt } => {
                let signature = error_signature_for_retry_feedback(&feedback);
                if let Some(current_signature) = &signature {
                    repeated_count = if repeated_signature
                        .as_ref()
                        .is_some_and(|previous| previous == current_signature)
                    {
                        repeated_count.saturating_add(1)
                    } else {
                        1
                    };
                    repeated_signature = Some(current_signature.clone());
                } else {
                    repeated_count = 0;
                    repeated_signature = None;
                }

                let can_loop_detect = signature.is_some()
                    && repeated_count >= REPEATED_ERROR_LOOP_THRESHOLD
                    && ctx.attempt > 0;
                if can_loop_detect {
                    log.record(
                        &ctx,
                        LogStage::Decision,
                        serde_json::json!({
                            "verdict": "drop",
                            "reason": "loop_detected",
                            "repeated_signature": signature.clone(),
                            "attempt": attempt,
                        }),
                    )
                    .await;
                    return Ok(OrchestrateOutcome::Drop {
                        reason: "loop_detected".to_string(),
                        repeated_signature: signature,
                    });
                }

                if ctx.attempt >= spec.max_retries {
                    let attempts_exhausted = ctx.attempt.saturating_add(1);
                    log.record(
                        &ctx,
                        LogStage::Decision,
                        serde_json::json!({
                            "verdict": "escalate",
                            "reason": "max_retries_exhausted",
                            "attempts_exhausted": attempts_exhausted,
                        }),
                    )
                    .await;
                    return Ok(OrchestrateOutcome::Escalate {
                        reason: "max_retries_exhausted".to_string(),
                        blocking_issue: "retries exhausted".to_string(),
                        attempts_exhausted: Some(attempts_exhausted),
                    });
                }

                log.record(
                    &ctx,
                    LogStage::Decision,
                    serde_json::json!({
                        "verdict": "retry",
                        "reason": "criteria_failed",
                        "next_attempt": attempt,
                        "signature": signature,
                    }),
                )
                .await;
                ctx = ctx.next_attempt_with_feedback(feedback);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AcceptanceCriterion;
    use crate::DelegationSpec;
    use crate::InMemoryDecisionLog;
    use crate::LogStage;
    use crate::PatternAuditor;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    fn sse_body(content: &str) -> String {
        let content_chunk = serde_json::json!({
            "id": "resp-test",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"content": content}}],
        })
        .to_string();
        let usage_chunk = serde_json::json!({
            "id": "resp-test",
            "choices": [{"index": 0, "finish_reason": "stop", "delta": {}}],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 3,
                "total_tokens": 8,
            },
        })
        .to_string();
        format!("data: {content_chunk}\n\ndata: {usage_chunk}\n\ndata: [DONE]\n\n")
    }

    async fn run_with_mocked_minimax(
        spec: DelegationSpec,
        responses: &[&str],
        expected_markers: &[Option<&str>],
    ) -> (OrchestrateOutcome, Vec<crate::log::RecordedEvent>) {
        let server = wiremock::MockServer::start().await;
        let requests_seen = Arc::new(AtomicUsize::new(0));
        let responses: Vec<String> = responses
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let expected_markers: Vec<Option<String>> = expected_markers
            .iter()
            .map(|marker| marker.map(ToString::to_string))
            .collect();
        let requests_seen_for_mock = Arc::clone(&requests_seen);
        let responses_for_mock = responses.clone();
        let expected_markers_for_mock = expected_markers.clone();
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |request: &wiremock::Request| {
                let index = requests_seen_for_mock.fetch_add(1, Ordering::SeqCst);
                let body = String::from_utf8_lossy(&request.body);
                match (
                    expected_markers_for_mock.get(index),
                    responses_for_mock.get(index),
                ) {
                    (_, None) => ResponseTemplate::new(500).set_body_string("unexpected request"),
                    (Some(Some(marker)), Some(response)) => {
                        if body.contains(marker) {
                            ResponseTemplate::new(200)
                                .set_body_string(sse_body(response))
                                .insert_header("content-type", "text/event-stream")
                        } else {
                            ResponseTemplate::new(400)
                                .set_body_string("marker not found in request")
                        }
                    }
                    (Some(None), Some(response)) => ResponseTemplate::new(200)
                        .set_body_string(sse_body(response))
                        .insert_header("content-type", "text/event-stream"),
                    (None, Some(response)) => ResponseTemplate::new(200)
                        .set_body_string(sse_body(response))
                        .insert_header("content-type", "text/event-stream"),
                }
            })
            .expect(responses.len() as u64)
            .mount(&server)
            .await;

        let previous_base_url = std::env::var("MINIMAX_BASE_URL").ok();
        let previous_api_key = std::env::var("MINIMAX_API_KEY").ok();
        unsafe {
            std::env::set_var("MINIMAX_BASE_URL", format!("{}/v1", server.uri()));
            std::env::set_var("MINIMAX_API_KEY", "sk-test-key");
        }

        let log = InMemoryDecisionLog::new();
        let sink = crate::MinimaxDispatchSink::new("MiniMax-M2.7");
        let auditor = PatternAuditor::new();
        let outcome: OrchestrateOutcome =
            run_orchestration_loop(&spec, "MiniMax-M2.7", &sink, &auditor, &log)
                .await
                .expect("orchestration loop should not fail");

        unsafe {
            match previous_base_url {
                Some(value) => std::env::set_var("MINIMAX_BASE_URL", value),
                None => std::env::remove_var("MINIMAX_BASE_URL"),
            }
            match previous_api_key {
                Some(value) => std::env::set_var("MINIMAX_API_KEY", value),
                None => std::env::remove_var("MINIMAX_API_KEY"),
            }
        }

        let received = server.received_requests().await;
        assert_eq!(
            received.expect("wiremock should return requests").len(),
            responses.len()
        );

        (outcome, log.events())
    }

    fn attempts(events: &[crate::log::RecordedEvent]) -> Vec<u8> {
        let mut list: Vec<u8> = events.iter().map(|event| event.attempt).collect();
        list.sort_unstable();
        list.dedup();
        list
    }

    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn loop_retries_on_output_mismatch_then_passes() {
        let mut spec = DelegationSpec::new_bare("make it done").unwrap();
        spec.acceptance = vec![AcceptanceCriterion::output_matches(r"^DONE$").unwrap()];
        let (outcome, events) = run_with_mocked_minimax(
            spec,
            &["almost", "DONE"],
            &[None, Some("output_excerpt: almost")],
        )
        .await;

        assert!(matches!(outcome, OrchestrateOutcome::Ok { .. }));
        assert_eq!(outcome.exit_code(), 0);
        assert_eq!(attempts(&events), vec![0, 1]);
        assert!(events.iter().any(|event| event.stage == LogStage::Decision
            && event.attempt == 1
            && event.payload["verdict"] == "ok"));
    }

    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn loop_retries_until_max_retries_exhausted_with_distinct_signatures() {
        let mut spec = DelegationSpec::new_bare("make it done").unwrap();
        spec.acceptance = vec![AcceptanceCriterion::output_matches(r"^DONE$").unwrap()];
        let (outcome, events) = run_with_mocked_minimax(
            spec,
            &["wrong-1", "wrong-2", "wrong-3"],
            &[
                None,
                Some("output_excerpt: wrong-1"),
                Some("output_excerpt: wrong-2"),
            ],
        )
        .await;

        let OrchestrateOutcome::Escalate {
            ref reason,
            ref attempts_exhausted,
            ..
        } = outcome
        else {
            panic!("expected escalate outcome after retries exhausted");
        };
        assert_eq!(reason, "max_retries_exhausted");
        assert_eq!(attempts_exhausted, &Some(3));
        assert_eq!(outcome.exit_code(), 2);
        assert_eq!(attempts(&events), vec![0, 1, 2]);
        assert!(events.iter().any(|event| {
            event.stage == LogStage::Decision
                && event.attempt == 2
                && event.payload["verdict"] == "escalate"
                && event.payload["attempts_exhausted"] == 3
        }));
    }

    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn loop_drops_on_repeated_signature_without_retries_exceeding_limit() {
        let mut spec = DelegationSpec::new_bare("make it done").unwrap();
        spec.acceptance = vec![AcceptanceCriterion::output_matches(r"^DONE$").unwrap()];
        let (outcome, events) = run_with_mocked_minimax(
            spec,
            &["same-failing-output", "same-failing-output"],
            &[None, Some("output_excerpt: same-failing-output")],
        )
        .await;

        let OrchestrateOutcome::Drop {
            ref reason,
            ref repeated_signature,
        } = outcome
        else {
            panic!("expected drop on loop signature");
        };
        assert_eq!(reason, "loop_detected");
        assert!(repeated_signature.is_some());
        assert_eq!(outcome.exit_code(), 3);
        assert_eq!(attempts(&events), vec![0, 1]);
        assert!(!events.iter().any(|event| event.attempt > 1));
        assert!(events.iter().any(|event| {
            event.stage == LogStage::Decision
                && event.payload["verdict"] == "drop"
                && event.payload["reason"] == "loop_detected"
                && event.payload.get("repeated_signature").is_some()
        }));
    }

    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn loop_escalates_immediately_on_forbidden_match_without_retry() {
        let mut spec = DelegationSpec::new_bare("make it safe").unwrap();
        spec.set_forbidden_patterns(["forbidden"]).unwrap();
        spec.acceptance = vec![AcceptanceCriterion::NoForbiddenPatterns];

        let (outcome, events) =
            run_with_mocked_minimax(spec, &["this contains forbidden"], &[None]).await;

        let OrchestrateOutcome::Escalate {
            ref reason,
            ref blocking_issue,
            ref attempts_exhausted,
        } = outcome
        else {
            panic!("expected escalate on forbidden pattern");
        };
        assert_eq!(
            reason,
            "no_forbidden_patterns failed — manual intervention required"
        );
        assert_eq!(attempts_exhausted, &None);
        assert!(blocking_issue.contains("forbidden"));
        assert_eq!(outcome.exit_code(), 2);
        assert_eq!(attempts(&events), vec![0]);
        assert!(!events.iter().any(|event| event.attempt > 0));
        assert!(events.iter().any(|event| {
            event.stage == LogStage::Decision
                && event.payload["verdict"] == "escalate"
                && event.payload["reason"].as_str().is_some()
        }));
    }

    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn clarify_response_yields_clarify_outcome() {
        let spec = DelegationSpec::new_bare("implement validate_email").unwrap();
        let question = "should I use regex or a parser?";

        let (outcome, events) =
            run_with_mocked_minimax(spec, &[&format!("CLARIFY: {question}")], &[None]).await;

        let OrchestrateOutcome::Clarify {
            question: outcome_question,
        } = outcome
        else {
            panic!("expected clarify outcome");
        };
        assert_eq!(outcome_question, question);
        assert_eq!(
            OrchestrateOutcome::Clarify {
                question: outcome_question,
            }
            .exit_code(),
            4
        );

        let decision_event = events
            .iter()
            .find(|event| event.stage == LogStage::Decision)
            .expect("decision event should exist");
        let decision: crate::AuditDecision =
            serde_json::from_value(decision_event.payload.clone()).expect("decision should parse");
        assert_eq!(
            decision,
            crate::AuditDecision::Clarify {
                question: question.to_string(),
            }
        );
    }

    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn clarify_logs_distinctive_verdict() {
        let spec = DelegationSpec::new_bare("implement validate_email").unwrap();
        let question = "should I use regex or a parser?";

        let (_outcome, events) =
            run_with_mocked_minimax(spec, &[&format!("CLARIFY: {question}")], &[None]).await;

        let decision_event = events
            .iter()
            .find(|event| event.stage == LogStage::Decision)
            .expect("decision event should exist");
        assert_eq!(decision_event.payload["verdict"], "clarify");
        assert_eq!(decision_event.payload["question"], question);
        assert_eq!(
            decision_event.payload["rationale"],
            "model requested clarification before generating output"
        );
        assert_ne!(decision_event.payload["verdict"], "escalate");
    }

    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn clarify_does_not_invoke_audit_or_retry() {
        let spec = DelegationSpec::new_bare("implement validate_email").unwrap();

        let (_outcome, events) =
            run_with_mocked_minimax(spec, &["CLARIFY: should I use regex or a parser?"], &[None])
                .await;

        assert!(!events.iter().any(|event| event.stage == LogStage::Audit));
        assert!(
            !events
                .iter()
                .any(|event| event.stage == LogStage::AuditCriterion)
        );
        assert!(!events.iter().any(|event| {
            event.stage == LogStage::Decision && event.payload["verdict"] == "retry"
        }));
    }
}
