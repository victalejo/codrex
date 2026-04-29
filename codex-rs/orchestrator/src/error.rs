//! Error types for the orchestrator's parse/validation paths.
//!
//! Audit/dispatch errors live alongside their respective implementations
//! (commits 3-5) and carry their own enums — this module is dedicated to
//! the pre-execution validation that Phase 3 performs as soon as a
//! `DelegationSpec` is constructed or deserialized.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SpecError {
    #[error("intent must be non-empty")]
    EmptyIntent,

    #[error("max_retries must be <= 10 (got {0})")]
    MaxRetriesTooLarge(u8),

    #[error("forbidden_patterns[{index}] is not a valid regex: {source}")]
    InvalidForbiddenPattern {
        index: usize,
        #[source]
        source: regex::Error,
    },

    #[error(
        "AcceptanceCriterion::OutputMatches contains an invalid regex (criterion #{index}): {source}"
    )]
    InvalidOutputMatchesRegex {
        index: usize,
        #[source]
        source: regex::Error,
    },

    #[error(
        "AcceptanceCriterion::TestsPass requires DelegationSpec.expected_tests to be set (criterion #{0})"
    )]
    TestsPassWithoutExpectedTests(usize),

    #[error(
        "AcceptanceCriterion::NoForbiddenPatterns requires DelegationSpec.forbidden_patterns to be non-empty (criterion #{0})"
    )]
    NoForbiddenPatternsWithoutPatterns(usize),

    #[error("AcceptanceCriterion::Custom requires a non-empty name (criterion #{0})")]
    EmptyCustomName(usize),

    #[error("ScriptRef.path must not be empty")]
    EmptyScriptPath,

    #[error("UtilRef.symbol must not be empty (entry #{0})")]
    EmptyUtilSymbol(usize),
}

#[derive(Debug, Error)]
pub enum ClassifierError {
    #[error("unsupported delegation_rules.toml version {found} (supported: 1)")]
    UnsupportedRulesVersion { found: u32 },

    #[error("delegation rule name must not be empty (rule #{index})")]
    EmptyRuleName { index: usize },

    #[error("delegation rule '{rule_name}' must contain at least one pattern")]
    EmptyRulePatterns { rule_name: String },

    #[error("rule '{rule_name}' uses unsupported action '{action}'")]
    UnsupportedRuleAction { rule_name: String, action: String },

    #[error("rule '{rule_name}' patterns[{index}] is not a valid regex: {source}")]
    InvalidRulePattern {
        rule_name: String,
        index: usize,
        #[source]
        source: regex::Error,
    },

    #[error("failed to create classifier home directory {}: {source}", path.display())]
    CreateRulesDir {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write {}: {source}", path.display())]
    WriteRulesFile {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read {}: {source}", path.display())]
    ReadRulesFile {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {}: {source}", path.display())]
    ParseRulesFile {
        path: std::path::PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("failed to parse delegation_rules.toml: {0}")]
    ParseRulesToml(#[source] toml::de::Error),
}
