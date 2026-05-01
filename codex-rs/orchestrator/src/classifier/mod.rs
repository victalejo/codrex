use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

mod llm;

pub use llm::CHATGPT_AUTH_DISABLED_REASON;
pub use llm::ClassificationTrace;
pub use llm::DEFAULT_LLM_FALLBACK_CACHE_SIZE;
pub use llm::DEFAULT_LLM_FALLBACK_MODEL;
pub use llm::DEFAULT_LLM_FALLBACK_PROVIDER;
pub use llm::DEFAULT_LLM_FALLBACK_TIMEOUT;
pub use llm::LlmClassification;
pub use llm::LlmClient;
pub use llm::LlmError;
pub use llm::LlmFallbackClassifier;
pub use llm::LlmFallbackConfig;
pub use llm::OpenAiFallbackAvailability;
pub use llm::OpenAiLlmClient;
pub use llm::classify_with_fallback;
pub use llm::load_openai_auth;
pub use llm::openai_fallback_availability;
pub use llm::resolve_llm_fallback_model;
pub use llm::resolve_openai_auth_sources;

use crate::ClassificationOutcome;
use crate::Classifier;
use crate::ClassifierError;
use crate::DelegationSpec;
use crate::ValidatedRegex;

const SUPPORTED_RULES_VERSION: u32 = 1;
const DEFAULT_RULES_FILE_NAME: &str = "delegation_rules.toml";
const DEFAULT_RULES_TOML: &str = r#"# Reglas evaluadas en orden. Primera que matchea gana.
# Cada regla tiene: name, action ("delegate" | "no_delegate"), patterns (regex array).
# El intent se matchea contra la unión de los patterns.

version = 1

[[rule]]
name = "implement_function"
action = "delegate"
patterns = [
  # "implement a function validate_email"
  "(?i)\\bimplement(?:ar|s|ed|ing)?\\s+(?:a\\s+|the\\s+)?function\\b",
  # "write a function add(a, b)"
  "(?i)\\bwrite\\s+(?:a\\s+|the\\s+)?function\\b",
  # "create a function fibonacci"
  "(?i)\\bcreate\\s+(?:a\\s+|the\\s+)?function\\b",
  # "implement validate_email", "implement Foo", "implement parse_date"
  # — captura "implement <identifier>" cuando el identifier es snake_case,
  # camelCase, o PascalCase. Es case-sensitive a propósito porque esas
  # convenciones distinguen mayúsculas/minúsculas por definición.
  "\\bimplement(?:ar|s|ed|ing)?\\s+(?:[a-z][a-z0-9]*_[a-z0-9_]+|[A-Z][a-zA-Z0-9]+|[a-z][a-zA-Z0-9]*[A-Z][a-zA-Z0-9]*)\\b",
]

[[rule]]
name = "write_tests"
action = "delegate"
patterns = [
  "(?i)\\bwrite\\s+(?:unit\\s+)?tests?\\b",
  "(?i)\\badd\\s+tests?\\s+for\\b",
]

[[rule]]
name = "translate_code"
action = "delegate"
patterns = [
  "(?i)\\btranslate\\s+(?:this\\s+|the\\s+)?(?:code|function|class)\\s+(?:from|to)\\b",
  "(?i)\\bport\\s+(?:this\\s+|the\\s+)?(?:code|function|class)\\s+(?:from|to)\\b",
]

[[rule]]
name = "mechanical_refactor"
action = "delegate"
patterns = [
  "(?i)\\brename\\s+\\w+\\s+to\\s+\\w+\\b",
  "(?i)\\bextract\\s+(?:method|function|variable|constant)\\b",
]

[[rule]]
name = "design_arch"
action = "no_delegate"
patterns = [
  "(?i)\\bdesign\\s+(?:the\\s+)?(?:architecture|api|schema|system)\\b",
  "(?i)\\bhow\\s+should\\s+I\\s+structure\\b",
  "(?i)\\bwhat'?s?\\s+the\\s+best\\s+(?:approach|way|pattern)\\b",
]

[[rule]]
name = "debug_complex"
action = "no_delegate"
patterns = [
  "(?i)\\bdebug\\b.*\\bweird\\b",
  "(?i)\\bwhy\\s+(?:does|is)\\s+this\\s+(?:fail|crash|hang)\\b",
  "(?i)\\binvestigate\\s+(?:this\\s+)?(?:bug|issue|crash)\\b",
]

[[rule]]
name = "security"
action = "no_delegate"
patterns = [
  "(?i)\\bauth(?:enticat\\w+|orization)?\\b",
  "(?i)\\bcredential\\s+(?:storage|management|handling)\\b",
  "(?i)\\bencrypt(?:ion)?\\s+(?:scheme|algorithm)\\b",
]

[[rule]]
name = "external_integration"
action = "no_delegate"
patterns = [
  "(?i)\\bintegrate\\s+with\\s+(?:a\\s+new\\s+)?(?:service|api|sdk)\\b",
  "(?i)\\bset\\s+up\\s+(?:a\\s+new\\s+)?(?:webhook|oauth|sso)\\b",
]
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesClassifier {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    pub action: RuleAction,
    pub patterns: Vec<ValidatedRegex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    Delegate,
    NoDelegate,
}

impl RulesClassifier {
    pub fn from_toml_str(toml_source: &str) -> Result<Self, ClassifierError> {
        let raw: RawRulesFile =
            toml::from_str(toml_source).map_err(ClassifierError::ParseRulesToml)?;
        Self::from_raw(raw)
    }

    pub fn from_default_path(home: &Path) -> Result<Self, ClassifierError> {
        Self::ship_defaults_if_missing(home)?;
        let path = rules_path(home);
        let toml_source =
            std::fs::read_to_string(&path).map_err(|source| ClassifierError::ReadRulesFile {
                path: path.clone(),
                source,
            })?;
        let raw: RawRulesFile =
            toml::from_str(&toml_source).map_err(|source| ClassifierError::ParseRulesFile {
                path: path.clone(),
                source,
            })?;
        Self::from_raw(raw)
    }

    pub fn ship_defaults_if_missing(home: &Path) -> Result<bool, ClassifierError> {
        std::fs::create_dir_all(home).map_err(|source| ClassifierError::CreateRulesDir {
            path: home.to_path_buf(),
            source,
        })?;
        let path = rules_path(home);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => return Ok(false),
            Err(source) => {
                return Err(ClassifierError::WriteRulesFile { path, source });
            }
        };
        file.write_all(DEFAULT_RULES_TOML.as_bytes())
            .and_then(|_| file.flush())
            .map_err(|source| ClassifierError::WriteRulesFile { path, source })?;
        Ok(true)
    }

    fn from_raw(raw: RawRulesFile) -> Result<Self, ClassifierError> {
        if raw.version != SUPPORTED_RULES_VERSION {
            return Err(ClassifierError::UnsupportedRulesVersion { found: raw.version });
        }
        let rules = raw
            .rules
            .into_iter()
            .enumerate()
            .map(|(index, raw_rule)| Rule::from_raw(index, raw_rule))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { rules })
    }
}

#[async_trait]
impl Classifier for RulesClassifier {
    async fn classify(&self, prompt: &str) -> ClassificationOutcome {
        for rule in &self.rules {
            if !rule.matches(prompt) {
                continue;
            }

            return match rule.action {
                RuleAction::Delegate => match DelegationSpec::new_bare(prompt) {
                    Ok(spec) => ClassificationOutcome::Delegate {
                        spec,
                        reason: format!("matched rule '{}'", rule.name),
                        rule_name: Some(rule.name.clone()),
                    },
                    Err(_) => ClassificationOutcome::PassThrough {
                        reason: format!("matched rule '{}', but intent was invalid", rule.name),
                        rule_name: Some(rule.name.clone()),
                    },
                },
                RuleAction::NoDelegate => ClassificationOutcome::PassThrough {
                    reason: format!("matched rule '{}'", rule.name),
                    rule_name: Some(rule.name.clone()),
                },
            };
        }

        ClassificationOutcome::PassThrough {
            reason: "no rule matched (LLM fallback in commit 7)".to_string(),
            rule_name: None,
        }
    }
}

impl Rule {
    fn from_raw(index: usize, raw_rule: RawRule) -> Result<Self, ClassifierError> {
        let name = raw_rule.name.trim().to_string();
        if name.is_empty() {
            return Err(ClassifierError::EmptyRuleName { index });
        }
        if raw_rule.patterns.is_empty() {
            return Err(ClassifierError::EmptyRulePatterns { rule_name: name });
        }
        let action = RuleAction::from_str(&name, &raw_rule.action)?;
        let patterns = raw_rule
            .patterns
            .into_iter()
            .enumerate()
            .map(|(pattern_index, pattern)| {
                ValidatedRegex::new(pattern).map_err(|source| ClassifierError::InvalidRulePattern {
                    rule_name: name.clone(),
                    index: pattern_index,
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            name,
            action,
            patterns,
        })
    }

    fn matches(&self, prompt: &str) -> bool {
        self.patterns
            .iter()
            .any(|pattern| pattern.regex().is_match(prompt))
    }
}

impl RuleAction {
    fn from_str(rule_name: &str, action: &str) -> Result<Self, ClassifierError> {
        match action {
            "delegate" => Ok(Self::Delegate),
            "no_delegate" => Ok(Self::NoDelegate),
            other => Err(ClassifierError::UnsupportedRuleAction {
                rule_name: rule_name.to_string(),
                action: other.to_string(),
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawRulesFile {
    version: u32,
    #[serde(rename = "rule", default)]
    rules: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    name: String,
    action: String,
    #[serde(default)]
    patterns: Vec<String>,
}

fn rules_path(home: &Path) -> PathBuf {
    home.join(DEFAULT_RULES_FILE_NAME)
}

#[cfg(test)]
mod tests;
