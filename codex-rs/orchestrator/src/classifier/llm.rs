use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use codex_api::ReqwestTransport;
use codex_api::ResponseEvent;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesClient;
use codex_api::ResponsesOptions;
use codex_api::create_text_param_for_request;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthMode;
use codex_login::CodexAuth;
use codex_model_provider::auth_provider_from_auth;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use lru::LruCache;
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

use crate::ClassificationOutcome;
use crate::Classifier;
use crate::DelegationSpec;

pub const DEFAULT_LLM_FALLBACK_MODEL: &str = "gpt-5-mini";
pub const DEFAULT_LLM_FALLBACK_CHATGPT_MODEL: &str = "gpt-5";
pub const DEFAULT_LLM_FALLBACK_PROVIDER: &str = "openai";
pub const DEFAULT_LLM_FALLBACK_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_LLM_FALLBACK_CACHE_SIZE: usize = 256;
pub const CHATGPT_AUTH_DISABLED_REASON: &str = "llm fallback disabled (auth_mode=chatgpt)";
static EMITTED_LLM_WARNING_KEYS: LazyLock<Mutex<HashSet<&'static str>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
const INCOMPATIBLE_MODEL_WARNING_KEY: &str = "incompatible_model";
const CHATGPT_AUTH_DISABLED_WARNING_KEY: &str = "chatgpt_auth_disabled";

/// Structured result returned by the LLM fallback classifier.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassificationTrace {
    pub outcome: ClassificationOutcome,
    pub llm_model: Option<String>,
    pub llm_confidence: Option<f32>,
    pub llm_reasoning: Option<String>,
    pub llm_error: Option<String>,
    pub cache_hit: bool,
}

impl ClassificationTrace {
    pub fn reason(&self) -> &str {
        match &self.outcome {
            ClassificationOutcome::Delegate { reason, .. }
            | ClassificationOutcome::PassThrough { reason, .. } => reason,
        }
    }

    pub fn rule_name(&self) -> Option<&str> {
        match &self.outcome {
            ClassificationOutcome::Delegate { rule_name, .. }
            | ClassificationOutcome::PassThrough { rule_name, .. } => rule_name.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmFallbackConfig {
    pub provider: String,
    pub model: String,
    pub timeout: Duration,
    pub cache_size: usize,
    pub enabled: bool,
}

impl Default for LlmFallbackConfig {
    fn default() -> Self {
        Self {
            provider: DEFAULT_LLM_FALLBACK_PROVIDER.to_string(),
            model: DEFAULT_LLM_FALLBACK_MODEL.to_string(),
            timeout: DEFAULT_LLM_FALLBACK_TIMEOUT,
            cache_size: DEFAULT_LLM_FALLBACK_CACHE_SIZE,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LlmClassification {
    pub should_delegate: bool,
    pub confidence: f32,
    pub reasoning: String,
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum LlmError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("invalid JSON from llm fallback: {0}")]
    InvalidJson(String),
    #[error("auth error: {0}")]
    Auth(String),
    #[error("provider error: {0}")]
    Provider(String),
}

/// Thin client abstraction so the fallback policy can be unit-tested without network calls.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn classify(&self, intent: &str) -> Result<LlmClassification, LlmError>;
}

/// OpenAI-backed fallback classifier used when rules do not match.
pub struct LlmFallbackClassifier {
    client: Arc<dyn LlmClient>,
    cache: Arc<Mutex<LruCache<String, CachedLlmDecision>>>,
    config: LlmFallbackConfig,
    disabled_state: Option<DisabledLlmFallbackState>,
}

#[derive(Debug, Clone, Copy)]
struct CachedLlmDecision {
    should_delegate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiFallbackAvailability {
    Enabled,
    DisabledByChatgptAuth,
    Unavailable,
}

#[derive(Debug, Clone)]
struct DisabledLlmFallbackState {
    reason: String,
    kind: DisabledLlmFallbackKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledLlmFallbackKind {
    Unavailable,
    ChatgptAuthDisabled,
}

impl LlmFallbackClassifier {
    pub fn new(client: Arc<dyn LlmClient>, config: LlmFallbackConfig) -> Self {
        let cache_size = NonZeroUsize::new(config.cache_size).unwrap_or(NonZeroUsize::MIN);
        Self {
            client,
            cache: Arc::new(Mutex::new(LruCache::new(cache_size))),
            config,
            disabled_state: None,
        }
    }

    pub fn with_disabled_reason(mut self, reason: impl Into<String>) -> Self {
        self.disabled_state = Some(DisabledLlmFallbackState::unavailable(reason));
        self
    }

    pub fn with_chatgpt_auth_disabled(mut self) -> Self {
        self.disabled_state = Some(DisabledLlmFallbackState::chatgpt_auth_disabled());
        self
    }

    pub async fn classify(&self, intent: &str) -> ClassificationTrace {
        if !self.config.enabled {
            return self
                .disabled_state
                .clone()
                .unwrap_or_else(|| DisabledLlmFallbackState::unavailable("llm fallback disabled"))
                .into_trace();
        }

        let cached = match self.cache.lock() {
            Ok(mut cache) => cache.get(intent).copied(),
            Err(poisoned) => poisoned.into_inner().get(intent).copied(),
        };
        if let Some(cached) = cached {
            return ClassificationTrace {
                outcome: classification_outcome_for_llm(intent, cached.should_delegate, true),
                llm_model: Some(self.config.model.clone()),
                llm_confidence: None,
                llm_reasoning: None,
                llm_error: None,
                cache_hit: true,
            };
        }

        match tokio::time::timeout(self.config.timeout, self.client.classify(intent)).await {
            Ok(Ok(classification)) => {
                match self.cache.lock() {
                    Ok(mut cache) => {
                        cache.put(
                            intent.to_string(),
                            CachedLlmDecision {
                                should_delegate: classification.should_delegate,
                            },
                        );
                    }
                    Err(poisoned) => {
                        let mut cache = poisoned.into_inner();
                        cache.put(
                            intent.to_string(),
                            CachedLlmDecision {
                                should_delegate: classification.should_delegate,
                            },
                        );
                    }
                }
                ClassificationTrace {
                    outcome: classification_outcome_for_llm(
                        intent,
                        classification.should_delegate,
                        false,
                    ),
                    llm_model: Some(self.config.model.clone()),
                    llm_confidence: Some(classification.confidence),
                    llm_reasoning: Some(classification.reasoning),
                    llm_error: None,
                    cache_hit: false,
                }
            }
            Ok(Err(error)) => unavailable_trace(error.to_string()),
            Err(_) => unavailable_trace(format!(
                "llm fallback timed out after {} ms",
                self.config.timeout.as_millis()
            )),
        }
    }
}

pub async fn classify_with_fallback<C: Classifier + ?Sized>(
    rules: &C,
    llm_fallback: &LlmFallbackClassifier,
    prompt: &str,
) -> ClassificationTrace {
    let rules_outcome = rules.classify(prompt).await;
    if should_use_llm_fallback(&rules_outcome) {
        llm_fallback.classify(prompt).await
    } else {
        ClassificationTrace {
            outcome: rules_outcome,
            llm_model: None,
            llm_confidence: None,
            llm_reasoning: None,
            llm_error: None,
            cache_hit: false,
        }
    }
}

pub fn load_openai_auth(
    codex_home: &Path,
    store_mode: AuthCredentialsStoreMode,
) -> Result<Option<CodexAuth>, LlmError> {
    let env_auth = codex_login::read_openai_api_key_from_env()
        .map(|api_key| CodexAuth::from_api_key(&api_key));
    let stored_auth = CodexAuth::from_auth_storage(codex_home, store_mode)
        .map_err(|error| LlmError::Auth(error.to_string()))?;
    Ok(resolve_openai_auth_sources(env_auth, stored_auth))
}

pub fn resolve_openai_auth_sources(
    env_auth: Option<CodexAuth>,
    stored_auth: Option<CodexAuth>,
) -> Option<CodexAuth> {
    env_auth.or(stored_auth)
}

pub fn openai_fallback_availability(auth: Option<&CodexAuth>) -> OpenAiFallbackAvailability {
    match auth.map(CodexAuth::auth_mode) {
        Some(AuthMode::ApiKey) => OpenAiFallbackAvailability::Enabled,
        Some(AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens) => {
            OpenAiFallbackAvailability::DisabledByChatgptAuth
        }
        Some(AuthMode::AgentIdentity) | None => OpenAiFallbackAvailability::Unavailable,
    }
}

pub fn is_model_compatible_with_auth(model: &str, auth_mode: &AuthMode) -> bool {
    match auth_mode {
        AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens => matches!(
            model,
            "gpt-5" | "gpt-5-codex" | "o3" | "o4-mini" | "gpt-4o" | "gpt-4.1"
        ),
        AuthMode::ApiKey => true,
        AuthMode::AgentIdentity => true,
    }
}

pub fn default_model_for_auth(auth_mode: &AuthMode) -> &'static str {
    match auth_mode {
        AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens => DEFAULT_LLM_FALLBACK_CHATGPT_MODEL,
        AuthMode::ApiKey => DEFAULT_LLM_FALLBACK_MODEL,
        AuthMode::AgentIdentity => DEFAULT_LLM_FALLBACK_CHATGPT_MODEL,
    }
}

pub fn resolve_llm_fallback_model(
    configured_model: Option<&str>,
    auth_mode: Option<AuthMode>,
) -> String {
    let resolved = resolve_model_for_auth(configured_model, auth_mode);
    if let Some((configured_model, auth_mode, fallback_model)) = resolved.warning {
        warn_incompatible_model_once(&configured_model, auth_mode, fallback_model);
    }
    resolved.model
}

struct ResolvedLlmFallbackModel {
    model: String,
    warning: Option<(String, AuthMode, &'static str)>,
}

fn resolve_model_for_auth(
    configured_model: Option<&str>,
    auth_mode: Option<AuthMode>,
) -> ResolvedLlmFallbackModel {
    let default_model = auth_mode
        .as_ref()
        .map_or(DEFAULT_LLM_FALLBACK_CHATGPT_MODEL, default_model_for_auth);
    match (configured_model, auth_mode) {
        (Some(model), Some(auth_mode)) if !is_model_compatible_with_auth(model, &auth_mode) => {
            ResolvedLlmFallbackModel {
                model: default_model.to_string(),
                warning: Some((model.to_string(), auth_mode, default_model)),
            }
        }
        (Some(model), _) => ResolvedLlmFallbackModel {
            model: model.to_string(),
            warning: None,
        },
        (None, Some(auth_mode)) => ResolvedLlmFallbackModel {
            model: default_model_for_auth(&auth_mode).to_string(),
            warning: None,
        },
        (None, None) => ResolvedLlmFallbackModel {
            model: DEFAULT_LLM_FALLBACK_CHATGPT_MODEL.to_string(),
            warning: None,
        },
    }
}

fn warn_incompatible_model_once(configured_model: &str, auth_mode: AuthMode, fallback_model: &str) {
    emit_warning_once(INCOMPATIBLE_MODEL_WARNING_KEY, || {
        let auth_mode_label = auth_mode_label(auth_mode);
        tracing::warn!(
            configured_model,
            auth_mode = auth_mode_label,
            fallback_model,
            "configured llm_fallback model '{configured_model}' is not available with your auth mode ({auth_mode_label}). Falling back to '{fallback_model}'. To use '{configured_model}', authenticate with an API key via `codrex login --with-api-key`."
        );
    });
}

fn warn_chatgpt_auth_disabled_once() {
    emit_warning_once(CHATGPT_AUTH_DISABLED_WARNING_KEY, || {
        tracing::warn!(
            auth_mode = "chatgpt",
            provider = DEFAULT_LLM_FALLBACK_PROVIDER,
            fallback_state = "disabled",
            "LLM fallback classifier disabled — your OpenAI auth is ChatGPT account, which doesn't support API access to the models needed for classification. Tasks that don't match a delegation rule will pass through without LLM fallback. To enable LLM fallback: use an API key via `codrex login --with-api-key`, or rely on rules only (no action needed)."
        );
        eprintln!(
            "warn: LLM fallback classifier disabled — your OpenAI auth is\n\
ChatGPT account, which doesn't support API access to the models\n\
needed for classification. Tasks that don't match a delegation\n\
rule will pass through without LLM fallback.\n\
\n\
To enable LLM fallback:\n\
  - Use an API key: codrex login --with-api-key\n\
  - Or rely on rules only (no action needed)"
        );
    });
}

fn emit_warning_once(key: &'static str, emit: impl FnOnce()) {
    let should_emit = match EMITTED_LLM_WARNING_KEYS.lock() {
        Ok(mut emitted) => emitted.insert(key),
        Err(poisoned) => {
            let mut emitted = poisoned.into_inner();
            emitted.insert(key)
        }
    };
    if should_emit {
        emit();
    }
}

fn auth_mode_label(auth_mode: AuthMode) -> &'static str {
    match auth_mode {
        AuthMode::ApiKey => "API key",
        AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens => "ChatGPT account",
        AuthMode::AgentIdentity => "Agent Identity",
    }
}

pub struct OpenAiLlmClient {
    client: ResponsesClient<ReqwestTransport>,
    model: String,
}

impl OpenAiLlmClient {
    pub fn new(model: impl Into<String>, auth: &CodexAuth) -> Result<Self, LlmError> {
        let model = model.into();
        let provider_info = ModelProviderInfo::create_openai_provider(/*base_url*/ None);
        let provider = provider_info
            .to_api_provider(Some(auth.auth_mode()))
            .map_err(|error| LlmError::Provider(error.to_string()))?;
        let auth_provider = auth_provider_from_auth(auth);
        let transport = ReqwestTransport::new(reqwest::Client::new());
        let client = ResponsesClient::new(transport, provider, auth_provider);
        Ok(Self { client, model })
    }
}

#[async_trait]
impl LlmClient for OpenAiLlmClient {
    async fn classify(&self, intent: &str) -> Result<LlmClassification, LlmError> {
        let request = ResponsesApiRequest {
            model: self.model.clone(),
            instructions: llm_classifier_instructions().to_string(),
            input: vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!("Request: {intent}"),
                }],
                phase: None,
            }],
            tools: Vec::new(),
            tool_choice: "none".to_string(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: create_text_param_for_request(
                /*verbosity*/ None,
                &Some(classification_output_schema()),
                /*output_schema_strict*/ true,
            ),
            client_metadata: None,
        };

        let mut stream = self
            .client
            .stream_request(request, ResponsesOptions::default())
            .await
            .map_err(|error| LlmError::Transport(error.to_string()))?;

        let mut text = String::new();
        let mut final_message_text = String::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|error| LlmError::Transport(error.to_string()))? {
                ResponseEvent::OutputTextDelta(delta) => text.push_str(&delta),
                ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }) => {
                    for item in content {
                        if let ContentItem::OutputText { text } = item {
                            final_message_text.push_str(&text);
                        }
                    }
                }
                _ => {}
            }
        }

        let raw = if text.trim().is_empty() {
            final_message_text
        } else {
            text
        };
        parse_llm_classification(&raw)
    }
}

fn classification_outcome_for_llm(
    intent: &str,
    should_delegate: bool,
    cache_hit: bool,
) -> ClassificationOutcome {
    let reason = if cache_hit {
        "llm fallback (cached)"
    } else {
        "llm fallback"
    };

    if !should_delegate {
        return ClassificationOutcome::PassThrough {
            reason: reason.to_string(),
            rule_name: None,
        };
    }

    match DelegationSpec::new_bare(intent) {
        Ok(spec) => ClassificationOutcome::Delegate {
            spec,
            reason: reason.to_string(),
            rule_name: None,
        },
        Err(_) => ClassificationOutcome::PassThrough {
            reason: format!("{reason}, but intent was invalid"),
            rule_name: None,
        },
    }
}

fn unavailable_trace(error: String) -> ClassificationTrace {
    ClassificationTrace {
        outcome: ClassificationOutcome::PassThrough {
            reason: "no rule matched + llm fallback unavailable".to_string(),
            rule_name: None,
        },
        llm_model: None,
        llm_confidence: None,
        llm_reasoning: None,
        llm_error: Some(error),
        cache_hit: false,
    }
}

fn disabled_trace(reason: impl Into<String>) -> ClassificationTrace {
    ClassificationTrace {
        outcome: ClassificationOutcome::PassThrough {
            reason: format!("no rule matched + {}", reason.into()),
            rule_name: None,
        },
        llm_model: None,
        llm_confidence: None,
        llm_reasoning: None,
        llm_error: None,
        cache_hit: false,
    }
}

fn should_use_llm_fallback(outcome: &ClassificationOutcome) -> bool {
    matches!(
        outcome,
        ClassificationOutcome::PassThrough { reason, rule_name }
            if rule_name.is_none() && reason.contains("no rule matched")
    )
}

fn parse_llm_classification(raw: &str) -> Result<LlmClassification, LlmError> {
    let parsed: LlmClassification =
        serde_json::from_str(raw).map_err(|error| LlmError::InvalidJson(error.to_string()))?;
    if !(0.0..=1.0).contains(&parsed.confidence) || !parsed.confidence.is_finite() {
        return Err(LlmError::InvalidJson(
            "confidence must be between 0.0 and 1.0".to_string(),
        ));
    }
    Ok(parsed)
}

fn classification_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["should_delegate", "confidence", "reasoning"],
        "properties": {
            "should_delegate": { "type": "boolean" },
            "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
            "reasoning": { "type": "string" }
        }
    })
}

fn llm_classifier_instructions() -> &'static str {
    "You are a classifier. Decide if the following user request should be\
\n\
delegated to a code-writing model (which is good at mechanical\
\n\
implementation but weak at architecture, debugging, security, and\
\n\
external integrations).\
\n\
\n\
Reply ONLY with valid JSON in this exact format:\
\n\
{\"should_delegate\": true|false, \"confidence\": 0.0-1.0, \"reasoning\": \"<one short sentence>\"}\
\n\
\n\
Delegate when: implementing a clearly-specified function, writing\
\n\
tests, mechanical refactor, code translation between languages.\
\n\
\n\
Do NOT delegate when: architectural design, debugging complex issues,\
\n\
security/auth decisions, integrations with new external services,\
\n\
ambiguous or open-ended questions."
}

impl DisabledLlmFallbackState {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            kind: DisabledLlmFallbackKind::Unavailable,
        }
    }

    fn chatgpt_auth_disabled() -> Self {
        Self {
            reason: CHATGPT_AUTH_DISABLED_REASON.to_string(),
            kind: DisabledLlmFallbackKind::ChatgptAuthDisabled,
        }
    }

    fn into_trace(self) -> ClassificationTrace {
        match self.kind {
            DisabledLlmFallbackKind::Unavailable => unavailable_trace(self.reason),
            DisabledLlmFallbackKind::ChatgptAuthDisabled => {
                warn_chatgpt_auth_disabled_once();
                disabled_trace(self.reason)
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn reset_llm_fallback_warning_state_for_tests() {
    match EMITTED_LLM_WARNING_KEYS.lock() {
        Ok(mut emitted) => emitted.clear(),
        Err(poisoned) => {
            let mut emitted = poisoned.into_inner();
            emitted.clear();
        }
    }
}
