use crate::ClassificationOutcome;
use crate::Classifier;
use crate::RulesClassifier;
use pretty_assertions::assert_eq;
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
