use crate::CHATGPT_AUTH_DISABLED_REASON;
use crate::ClassificationOutcome;
use crate::Classifier;
use crate::LlmClassification;
use crate::LlmClient;
use crate::LlmError;
use crate::LlmFallbackClassifier;
use crate::LlmFallbackConfig;
use crate::OpenAiFallbackAvailability;
use crate::RulesClassifier;
use crate::openai_fallback_availability;
use crate::resolve_llm_fallback_model;
use crate::resolve_openai_auth_sources;
use async_trait::async_trait;
use codex_login::AuthMode;
use codex_login::load_auth_dot_json;
use pretty_assertions::assert_eq;
use serial_test::serial;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;

const FIRST_MATCH_WINS_TOML: &str = r#"
version = 1

[[rule]]
name = "first_delegate"
action = "delegate"
patterns = ["(?i)\\bimplement\\b"]

[[rule]]
name = "second_no_delegate"
action = "no_delegate"
patterns = ["(?i)\\bimplement\\b"]
"#;

const NO_DELEGATE_TOML: &str = r#"
version = 1

[[rule]]
name = "design_arch"
action = "no_delegate"
patterns = ["(?i)\\bdesign\\s+(?:the\\s+)?architecture\\b"]
"#;

const INVALID_REGEX_TOML: &str = r#"
version = 1

[[rule]]
name = "broken"
action = "delegate"
patterns = ["("]
"#;

const UNKNOWN_ACTION_TOML: &str = r#"
version = 1

[[rule]]
name = "broken"
action = "foo"
patterns = ["(?i)\\bimplement\\b"]
"#;

const UNSUPPORTED_VERSION_TOML: &str = r#"
version = 999

[[rule]]
name = "broken"
action = "delegate"
patterns = ["(?i)\\bimplement\\b"]
"#;

const CUSTOM_RULES_TOML: &str = r#"
version = 1

[[rule]]
name = "leave_me_alone"
action = "no_delegate"
patterns = ["(?i)\\bkeep\\b"]
"#;

#[derive(Debug)]
struct MockLlmClient {
    responses: Mutex<HashMap<String, VecDeque<MockLlmResponse>>>,
    calls: AtomicUsize,
}

#[derive(Debug, Clone)]
enum MockLlmResponse {
    Immediate(Result<LlmClassification, LlmError>),
    Delayed {
        delay: Duration,
        result: Result<LlmClassification, LlmError>,
    },
}

impl MockLlmClient {
    fn with_responses(
        entries: impl IntoIterator<Item = (&'static str, MockLlmResponse)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(entries.into_iter().fold(
                HashMap::new(),
                |mut responses, (prompt, response)| {
                    responses
                        .entry(prompt.to_string())
                        .or_insert_with(VecDeque::new)
                        .push_back(response);
                    responses
                },
            )),
            calls: AtomicUsize::new(0),
        })
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn classify(&self, intent: &str) -> Result<LlmClassification, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let response = self
            .responses
            .lock()
            .unwrap()
            .get_mut(intent)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| panic!("missing mock response for {intent:?}"));
        match response {
            MockLlmResponse::Immediate(result) => result,
            MockLlmResponse::Delayed { delay, result } => {
                tokio::time::sleep(delay).await;
                result
            }
        }
    }
}

fn llm_config(enabled: bool) -> LlmFallbackConfig {
    LlmFallbackConfig {
        provider: "openai".to_string(),
        model: "gpt-5-mini".to_string(),
        timeout: Duration::from_millis(25),
        cache_size: 256,
        enabled,
    }
}

fn write_auth_json(home: &Path, body: &str) {
    std::fs::write(home.join("auth.json"), body).expect("auth.json should be written");
}

#[tokio::test]
async fn delegate_rule_matches_first_wins() {
    let classifier = RulesClassifier::from_toml_str(FIRST_MATCH_WINS_TOML).unwrap();

    let outcome = classifier.classify("implement a function add(a, b)").await;

    let ClassificationOutcome::Delegate {
        spec,
        reason,
        rule_name,
    } = outcome
    else {
        panic!("expected delegate outcome");
    };
    assert_eq!(spec.intent, "implement a function add(a, b)");
    assert_eq!(reason, "matched rule 'first_delegate'");
    assert_eq!(rule_name.as_deref(), Some("first_delegate"));
}

#[tokio::test]
async fn no_delegate_rule_matches() {
    let classifier = RulesClassifier::from_toml_str(NO_DELEGATE_TOML).unwrap();

    let outcome = classifier.classify("design the architecture").await;

    let ClassificationOutcome::PassThrough { reason, rule_name } = outcome else {
        panic!("expected pass-through outcome");
    };
    assert_eq!(reason, "matched rule 'design_arch'");
    assert_eq!(rule_name.as_deref(), Some("design_arch"));
}

#[tokio::test]
async fn no_match_returns_pass_through() {
    let classifier = RulesClassifier::from_toml_str(NO_DELEGATE_TOML).unwrap();

    let outcome = classifier
        .classify("what do you think about Rust as a language")
        .await;

    let ClassificationOutcome::PassThrough { reason, rule_name } = outcome else {
        panic!("expected pass-through outcome");
    };
    assert_eq!(reason, "no rule matched (LLM fallback in commit 7)");
    assert_eq!(rule_name, None);
}

#[tokio::test]
async fn case_insensitive_match() {
    let classifier = RulesClassifier::from_default_path(TempDir::new().unwrap().path()).unwrap();

    let outcome = classifier.classify("Implement A Function").await;

    assert!(matches!(outcome, ClassificationOutcome::Delegate { .. }));
}

#[test]
fn from_toml_str_validates_regex_at_parse_time() {
    let err = RulesClassifier::from_toml_str(INVALID_REGEX_TOML).unwrap_err();

    assert!(err.to_string().contains("rule 'broken'"));
    assert!(err.to_string().contains("patterns[0]"));
}

#[test]
fn from_toml_str_rejects_unknown_action() {
    let err = RulesClassifier::from_toml_str(UNKNOWN_ACTION_TOML).unwrap_err();

    assert!(err.to_string().contains("rule 'broken'"));
    assert!(err.to_string().contains("unsupported action 'foo'"));
}

#[test]
fn from_toml_str_rejects_unsupported_version() {
    let err = RulesClassifier::from_toml_str(UNSUPPORTED_VERSION_TOML).unwrap_err();

    assert!(
        err.to_string()
            .contains("unsupported delegation_rules.toml version 999")
    );
}

#[test]
fn ship_defaults_creates_file_when_missing() {
    let temp = TempDir::new().unwrap();

    let created = RulesClassifier::ship_defaults_if_missing(temp.path()).unwrap();

    assert!(created);
    let rules_path = temp.path().join("delegation_rules.toml");
    assert!(rules_path.exists());
}

#[test]
fn ship_defaults_does_not_overwrite_existing() {
    let temp = TempDir::new().unwrap();
    let rules_path = temp.path().join("delegation_rules.toml");
    std::fs::write(&rules_path, CUSTOM_RULES_TOML).unwrap();

    let created = RulesClassifier::ship_defaults_if_missing(temp.path()).unwrap();

    assert!(!created);
    assert_eq!(
        std::fs::read_to_string(rules_path).unwrap(),
        CUSTOM_RULES_TOML
    );
}

#[tokio::test]
async fn defaults_classifier_classifies_canonical_examples() {
    let temp = TempDir::new().unwrap();
    let classifier = RulesClassifier::from_default_path(temp.path()).unwrap();

    let cases = [
        ("implement validate_email", true),
        ("implement a function add", true),
        ("implement parseDate", true),
        ("implement UserService", true),
        ("write a function fibonacci", true),
        ("write tests for the parser", true),
        ("translate this code from Python to Go", true),
        ("design the auth schema", false),
        ("why does this crash randomly", false),
        ("what do you think about Rust", false),
        ("implement the design", false),
        ("implement this carefully", false),
    ];

    for (prompt, should_delegate) in cases {
        let outcome = classifier.classify(prompt).await;
        assert_eq!(
            matches!(outcome, ClassificationOutcome::Delegate { .. }),
            should_delegate,
            "unexpected classification for prompt {prompt:?}",
        );
    }
}

#[tokio::test]
async fn llm_fallback_returns_delegate_when_llm_says_yes() {
    let client = MockLlmClient::with_responses([(
        "convert this XML config to YAML format",
        MockLlmResponse::Immediate(Ok(LlmClassification {
            should_delegate: true,
            confidence: 0.85,
            reasoning: "Request asks for a mechanical code-format translation".to_string(),
        })),
    )]);
    let classifier = LlmFallbackClassifier::new(client, llm_config(true));

    let trace = classifier
        .classify("convert this XML config to YAML format")
        .await;

    assert!(matches!(
        trace.outcome,
        ClassificationOutcome::Delegate { .. }
    ));
    assert_eq!(trace.llm_model.as_deref(), Some("gpt-5-mini"));
    assert_eq!(trace.llm_confidence, Some(0.85));
    assert_eq!(
        trace.llm_reasoning.as_deref(),
        Some("Request asks for a mechanical code-format translation")
    );
    assert!(!trace.cache_hit);
    assert_eq!(trace.llm_error, None);
}

#[tokio::test]
async fn llm_fallback_returns_pass_through_when_llm_says_no() {
    let client = MockLlmClient::with_responses([(
        "explain why my coworker keeps disagreeing with me",
        MockLlmResponse::Immediate(Ok(LlmClassification {
            should_delegate: false,
            confidence: 0.78,
            reasoning: "Request is ambiguous and non-technical".to_string(),
        })),
    )]);
    let classifier = LlmFallbackClassifier::new(client, llm_config(true));

    let trace = classifier
        .classify("explain why my coworker keeps disagreeing with me")
        .await;

    let ClassificationOutcome::PassThrough { reason, rule_name } = &trace.outcome else {
        panic!("expected pass-through outcome");
    };
    assert_eq!(reason, "llm fallback");
    assert_eq!(rule_name, &None);
    assert_eq!(trace.llm_model.as_deref(), Some("gpt-5-mini"));
    assert_eq!(trace.llm_confidence, Some(0.78));
    assert_eq!(
        trace.llm_reasoning.as_deref(),
        Some("Request is ambiguous and non-technical")
    );
    assert!(!trace.cache_hit);
    assert_eq!(trace.llm_error, None);
}

#[tokio::test]
async fn llm_fallback_caches_result_for_repeat_intent() {
    let client = MockLlmClient::with_responses([(
        "convert this XML config to YAML format",
        MockLlmResponse::Immediate(Ok(LlmClassification {
            should_delegate: true,
            confidence: 0.91,
            reasoning: "Mechanical translation request".to_string(),
        })),
    )]);
    let classifier = LlmFallbackClassifier::new(client.clone(), llm_config(true));

    let first = classifier
        .classify("convert this XML config to YAML format")
        .await;
    let second = classifier
        .classify("convert this XML config to YAML format")
        .await;

    assert!(matches!(
        first.outcome,
        ClassificationOutcome::Delegate { .. }
    ));
    assert!(matches!(
        second.outcome,
        ClassificationOutcome::Delegate { .. }
    ));
    assert!(!first.cache_hit);
    assert!(second.cache_hit);
    assert_eq!(client.call_count(), 1);
}

#[tokio::test]
async fn llm_fallback_returns_pass_through_when_disabled() {
    let client = MockLlmClient::with_responses([]);
    let classifier = LlmFallbackClassifier::new(client.clone(), llm_config(false));

    let trace = classifier
        .classify("convert this XML config to YAML format")
        .await;

    let ClassificationOutcome::PassThrough { reason, rule_name } = &trace.outcome else {
        panic!("expected pass-through outcome");
    };
    assert_eq!(reason, "no rule matched + llm fallback unavailable");
    assert_eq!(rule_name, &None);
    assert_eq!(trace.llm_error.as_deref(), Some("llm fallback disabled"));
    assert_eq!(trace.llm_model, None);
    assert!(!trace.cache_hit);
    assert_eq!(client.call_count(), 0);
}

#[tokio::test]
async fn llm_fallback_returns_pass_through_on_timeout() {
    let client = MockLlmClient::with_responses([(
        "convert this XML config to YAML format",
        MockLlmResponse::Delayed {
            delay: Duration::from_millis(100),
            result: Ok(LlmClassification {
                should_delegate: true,
                confidence: 0.9,
                reasoning: "Too slow".to_string(),
            }),
        },
    )]);
    let classifier = LlmFallbackClassifier::new(client, llm_config(true));

    let trace = classifier
        .classify("convert this XML config to YAML format")
        .await;

    let ClassificationOutcome::PassThrough { reason, .. } = &trace.outcome else {
        panic!("expected pass-through outcome");
    };
    assert_eq!(reason, "no rule matched + llm fallback unavailable");
    assert!(
        trace
            .llm_error
            .as_deref()
            .is_some_and(|error| error.contains("timed out"))
    );
}

#[tokio::test]
async fn llm_fallback_returns_pass_through_on_invalid_json() {
    let client = MockLlmClient::with_responses([(
        "convert this XML config to YAML format",
        MockLlmResponse::Immediate(Err(LlmError::InvalidJson(
            "missing should_delegate".to_string(),
        ))),
    )]);
    let classifier = LlmFallbackClassifier::new(client, llm_config(true));

    let trace = classifier
        .classify("convert this XML config to YAML format")
        .await;

    assert!(matches!(
        trace.outcome,
        ClassificationOutcome::PassThrough { .. }
    ));
    assert_eq!(
        trace.llm_error.as_deref(),
        Some("invalid JSON from llm fallback: missing should_delegate")
    );
}

#[tokio::test]
async fn llm_fallback_returns_pass_through_on_network_error() {
    let client = MockLlmClient::with_responses([(
        "convert this XML config to YAML format",
        MockLlmResponse::Immediate(Err(LlmError::Transport("connection reset".to_string()))),
    )]);
    let classifier = LlmFallbackClassifier::new(client, llm_config(true));

    let trace = classifier
        .classify("convert this XML config to YAML format")
        .await;

    assert!(matches!(
        trace.outcome,
        ClassificationOutcome::PassThrough { .. }
    ));
    assert_eq!(
        trace.llm_error.as_deref(),
        Some("transport error: connection reset")
    );
}

#[tokio::test]
async fn cache_evicts_lru_at_capacity() {
    let client = MockLlmClient::with_responses([
        (
            "first prompt",
            MockLlmResponse::Immediate(Ok(LlmClassification {
                should_delegate: true,
                confidence: 0.8,
                reasoning: "first".to_string(),
            })),
        ),
        (
            "second prompt",
            MockLlmResponse::Immediate(Ok(LlmClassification {
                should_delegate: false,
                confidence: 0.7,
                reasoning: "second".to_string(),
            })),
        ),
        (
            "first prompt",
            MockLlmResponse::Immediate(Ok(LlmClassification {
                should_delegate: true,
                confidence: 0.82,
                reasoning: "first again".to_string(),
            })),
        ),
    ]);
    let mut config = llm_config(true);
    config.cache_size = 1;
    let classifier = LlmFallbackClassifier::new(client.clone(), config);

    let _ = classifier.classify("first prompt").await;
    let _ = classifier.classify("second prompt").await;
    let third = classifier.classify("first prompt").await;

    assert!(!third.cache_hit);
    assert_eq!(client.call_count(), 3);
}

#[tokio::test]
async fn cache_does_not_persist_across_classifier_instances() {
    let first_client = MockLlmClient::with_responses([(
        "convert this XML config to YAML format",
        MockLlmResponse::Immediate(Ok(LlmClassification {
            should_delegate: true,
            confidence: 0.83,
            reasoning: "first instance".to_string(),
        })),
    )]);
    let second_client = MockLlmClient::with_responses([(
        "convert this XML config to YAML format",
        MockLlmResponse::Immediate(Ok(LlmClassification {
            should_delegate: true,
            confidence: 0.84,
            reasoning: "second instance".to_string(),
        })),
    )]);

    let first = LlmFallbackClassifier::new(first_client.clone(), llm_config(true));
    let second = LlmFallbackClassifier::new(second_client.clone(), llm_config(true));

    let _ = first
        .classify("convert this XML config to YAML format")
        .await;
    let second_trace = second
        .classify("convert this XML config to YAML format")
        .await;

    assert!(!second_trace.cache_hit);
    assert_eq!(first_client.call_count(), 1);
    assert_eq!(second_client.call_count(), 1);
}

#[test]
fn resolve_uses_default_when_no_model_configured() {
    assert_eq!(resolve_llm_fallback_model(None), "gpt-5.4");
}

#[test]
fn resolve_passes_configured_model_through_unchanged() {
    assert_eq!(
        resolve_llm_fallback_model(Some("gpt-5.5")),
        "gpt-5.5",
        "configured model must be used as-is, with no auth-based downgrade"
    );
    assert_eq!(
        resolve_llm_fallback_model(Some("gpt-5.3-codex")),
        "gpt-5.3-codex"
    );
}

#[tokio::test]
#[tracing_test::traced_test]
#[serial]
async fn chatgpt_auth_disables_llm_fallback_with_warning() {
    super::llm::reset_llm_fallback_warning_state_for_tests();
    let client = MockLlmClient::with_responses([]);
    let classifier =
        LlmFallbackClassifier::new(client.clone(), llm_config(false)).with_chatgpt_auth_disabled();

    let trace = classifier.classify("convert this XML config to YAML").await;

    let ClassificationOutcome::PassThrough { reason, rule_name } = &trace.outcome else {
        panic!("expected pass-through outcome");
    };
    assert_eq!(
        reason,
        &format!("no rule matched + {CHATGPT_AUTH_DISABLED_REASON}")
    );
    assert_eq!(rule_name, &None);
    assert_eq!(trace.llm_error, None);
    assert_eq!(client.call_count(), 0);
    assert!(logs_contain("LLM fallback classifier disabled"));
    assert!(logs_contain("auth_mode=\"chatgpt\""));
}

#[tokio::test]
#[tracing_test::traced_test]
#[serial]
async fn chatgpt_auth_warning_emitted_only_once() {
    super::llm::reset_llm_fallback_warning_state_for_tests();
    let client = MockLlmClient::with_responses([]);
    let classifier =
        LlmFallbackClassifier::new(client, llm_config(false)).with_chatgpt_auth_disabled();

    let _ = classifier.classify("convert this XML config to YAML").await;
    let _ = classifier
        .classify("explain why my coworker disagrees")
        .await;

    logs_assert(|lines: &[&str]| {
        let count = lines
            .iter()
            .filter(|line| line.contains("LLM fallback classifier disabled"))
            .count();
        if count == 1 {
            Ok(())
        } else {
            Err(format!("expected exactly one warning, got {count}"))
        }
    });
}

#[tokio::test]
async fn apikey_auth_keeps_llm_fallback_enabled() {
    let temp = TempDir::new().unwrap();
    write_auth_json(
        temp.path(),
        r#"{
  "auth_mode": "apikey",
  "OPENAI_API_KEY": "sk-test-key"
}"#,
    );

    let auth = crate::load_openai_auth(temp.path(), codex_login::AuthCredentialsStoreMode::File)
        .unwrap()
        .expect("auth should load");
    assert_eq!(
        openai_fallback_availability(Some(&auth)),
        OpenAiFallbackAvailability::Enabled
    );
    assert_eq!(
        load_auth_dot_json(temp.path(), codex_login::AuthCredentialsStoreMode::File)
            .unwrap()
            .expect("auth dot json should load")
            .auth_mode,
        Some(AuthMode::ApiKey)
    );

    let client = MockLlmClient::with_responses([(
        "convert this XML config to YAML",
        MockLlmResponse::Immediate(Ok(LlmClassification {
            should_delegate: true,
            confidence: 0.9,
            reasoning: "mechanical conversion".to_string(),
        })),
    )]);
    let classifier = LlmFallbackClassifier::new(client.clone(), llm_config(true));

    let _ = classifier.classify("convert this XML config to YAML").await;

    assert_eq!(client.call_count(), 1);
}

#[tokio::test]
#[tracing_test::traced_test]
#[serial]
async fn no_credentials_disables_silently() {
    super::llm::reset_llm_fallback_warning_state_for_tests();
    let client = MockLlmClient::with_responses([]);
    let classifier = LlmFallbackClassifier::new(client.clone(), llm_config(false))
        .with_disabled_reason("no openai credentials configured");

    let trace = classifier.classify("convert this XML config to YAML").await;

    let ClassificationOutcome::PassThrough { reason, rule_name } = &trace.outcome else {
        panic!("expected pass-through outcome");
    };
    assert_eq!(
        reason,
        "no rule matched + llm fallback disabled (no credentials)"
    );
    assert_eq!(rule_name, &None);
    assert_eq!(
        trace.llm_error.as_deref(),
        Some("no openai credentials configured")
    );
    assert_eq!(client.call_count(), 0);
    assert!(!logs_contain("LLM fallback classifier disabled"));
}

#[tokio::test]
#[tracing_test::traced_test]
#[serial]
async fn no_credentials_uses_distinctive_disabled_reason_in_jsonl() {
    super::llm::reset_llm_fallback_warning_state_for_tests();
    let client = MockLlmClient::with_responses([]);
    let classifier = LlmFallbackClassifier::new(client.clone(), llm_config(false))
        .with_disabled_reason("no openai credentials configured");

    let trace = classifier.classify("convert this XML config to YAML").await;

    let ClassificationOutcome::PassThrough { reason, .. } = &trace.outcome else {
        panic!("expected pass-through outcome");
    };
    assert_eq!(
        reason,
        "no rule matched + llm fallback disabled (no credentials)"
    );
    assert_eq!(
        trace.llm_error.as_deref(),
        Some("no openai credentials configured")
    );
    assert_eq!(client.call_count(), 0);
    assert!(!logs_contain("LLM fallback classifier disabled"));
}

#[tokio::test]
async fn env_var_overrides_auth_json() {
    let temp = TempDir::new().unwrap();
    write_auth_json(
        temp.path(),
        r#"{
  "auth_mode": "chatgpt"
}"#,
    );
    let stored_auth =
        crate::load_openai_auth(temp.path(), codex_login::AuthCredentialsStoreMode::File)
            .unwrap()
            .expect("stored auth should load");
    let resolved = resolve_openai_auth_sources(
        Some(codex_login::CodexAuth::from_api_key("sk-env")),
        Some(stored_auth),
    )
    .expect("resolved auth should exist");

    assert_eq!(resolved.auth_mode(), AuthMode::ApiKey);
    assert_eq!(
        openai_fallback_availability(Some(&resolved)),
        OpenAiFallbackAvailability::Enabled
    );
}
