use super::*;
use crate::token_data::IdTokenInfo;
use anyhow::Context;
use base64::Engine;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

use codex_keyring_store::tests::MockKeyringStore;
use keyring::Error as KeyringError;

#[tokio::test]
async fn file_storage_load_returns_auth_dot_json() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("test-key".to_string()),
        tokens: None,
        last_refresh: Some(Utc::now()),
        agent_identity: None,
    };

    storage
        .save(&auth_dot_json)
        .context("failed to save auth file")?;

    let loaded = storage.load().context("failed to load auth file")?;
    assert_eq!(Some(auth_dot_json), loaded);
    Ok(())
}

#[tokio::test]
async fn file_storage_save_persists_auth_dot_json() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("test-key".to_string()),
        tokens: None,
        last_refresh: Some(Utc::now()),
        agent_identity: None,
    };

    let file = get_auth_file(codex_home.path());
    storage
        .save(&auth_dot_json)
        .context("failed to save auth file")?;

    let same_auth_dot_json = storage
        .try_read_auth_json(&file)
        .context("failed to read auth file after save")?;
    assert_eq!(auth_dot_json, same_auth_dot_json);
    Ok(())
}

#[tokio::test]
async fn file_storage_round_trips_agent_identity_auth() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let agent_identity = jwt_with_payload(json!({
        "agent_runtime_id": "agent-runtime-id",
        "agent_private_key": "private-key",
        "account_id": "account-id",
        "chatgpt_user_id": "user-id",
        "email": "user@example.com",
        "plan_type": "pro",
        "chatgpt_account_is_fedramp": false,
    }));
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::AgentIdentity),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: Some(agent_identity),
    };

    storage.save(&auth_dot_json)?;

    let loaded = storage.load()?;
    assert_eq!(Some(auth_dot_json), loaded);
    Ok(())
}

#[tokio::test]
async fn file_storage_loads_agent_identity_as_jwt() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let agent_identity_jwt = jwt_with_payload(json!({
        "agent_runtime_id": "agent-runtime-id",
        "agent_private_key": "private-key",
        "account_id": "account-id",
        "chatgpt_user_id": "user-id",
        "email": "user@example.com",
        "plan_type": "pro",
        "chatgpt_account_is_fedramp": false,
    }));
    let auth_file = get_auth_file(codex_home.path());
    std::fs::write(
        &auth_file,
        serde_json::to_string_pretty(&json!({
            "auth_mode": "agentIdentity",
            "agent_identity": agent_identity_jwt,
        }))?,
    )?;

    let loaded = storage.load()?;

    assert_eq!(
        loaded.expect("auth should load").agent_identity.as_deref(),
        Some(agent_identity_jwt.as_str())
    );
    Ok(())
}

#[test]
fn file_storage_delete_removes_auth_file() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("sk-test-key".to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
    };
    let storage = create_auth_storage(dir.path().to_path_buf(), AuthCredentialsStoreMode::File);
    storage.save(&auth_dot_json)?;
    assert!(dir.path().join("auth.json").exists());
    let storage = FileAuthStorage::new(dir.path().to_path_buf());
    let removed = storage.delete()?;
    assert!(removed);
    assert!(!dir.path().join("auth.json").exists());
    Ok(())
}

#[test]
fn ephemeral_storage_save_load_delete_is_in_memory_only() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let storage = create_auth_storage(
        dir.path().to_path_buf(),
        AuthCredentialsStoreMode::Ephemeral,
    );
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("sk-ephemeral".to_string()),
        tokens: None,
        last_refresh: Some(Utc::now()),
        agent_identity: None,
    };

    storage.save(&auth_dot_json)?;
    let loaded = storage.load()?;
    assert_eq!(Some(auth_dot_json), loaded);

    let removed = storage.delete()?;
    assert!(removed);
    let loaded = storage.load()?;
    assert_eq!(None, loaded);
    assert!(!get_auth_file(dir.path()).exists());
    Ok(())
}

fn seed_keyring_and_fallback_auth_file_for_delete<F>(
    mock_keyring: &MockKeyringStore,
    codex_home: &Path,
    compute_key: F,
) -> anyhow::Result<(String, PathBuf)>
where
    F: FnOnce() -> std::io::Result<String>,
{
    let key = compute_key()?;
    mock_keyring.save(KEYRING_SERVICE, &key, "{}")?;
    let auth_file = get_auth_file(codex_home);
    std::fs::write(&auth_file, "stale")?;
    Ok((key, auth_file))
}

fn seed_keyring_with_auth<F>(
    mock_keyring: &MockKeyringStore,
    compute_key: F,
    auth: &AuthDotJson,
) -> anyhow::Result<()>
where
    F: FnOnce() -> std::io::Result<String>,
{
    let key = compute_key()?;
    let serialized = serde_json::to_string(auth)?;
    mock_keyring.save(KEYRING_SERVICE, &key, &serialized)?;
    Ok(())
}

fn assert_keyring_saved_auth_and_removed_fallback(
    mock_keyring: &MockKeyringStore,
    key: &str,
    codex_home: &Path,
    expected: &AuthDotJson,
) {
    let saved_value = mock_keyring
        .saved_value(key)
        .expect("keyring entry should exist");
    let expected_serialized = serde_json::to_string(expected).expect("serialize expected auth");
    assert_eq!(saved_value, expected_serialized);
    let auth_file = get_auth_file(codex_home);
    assert!(
        !auth_file.exists(),
        "fallback auth.json should be removed after keyring save"
    );
}

fn id_token_with_prefix(prefix: &str) -> IdTokenInfo {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let header = Header {
        alg: "none",
        typ: "JWT",
    };
    let payload = json!({
        "email": format!("{prefix}@example.com"),
        "https://api.openai.com/auth": {
            "chatgpt_account_id": format!("{prefix}-account"),
        },
    });
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header_b64 = encode(&serde_json::to_vec(&header).expect("serialize header"));
    let payload_b64 = encode(&serde_json::to_vec(&payload).expect("serialize payload"));
    let signature_b64 = encode(b"sig");
    let fake_jwt = format!("{header_b64}.{payload_b64}.{signature_b64}");

    crate::token_data::parse_chatgpt_jwt_claims(&fake_jwt).expect("fake JWT should parse")
}

fn auth_with_prefix(prefix: &str) -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some(format!("{prefix}-api-key")),
        tokens: Some(TokenData {
            id_token: id_token_with_prefix(prefix),
            access_token: format!("{prefix}-access"),
            refresh_token: format!("{prefix}-refresh"),
            account_id: Some(format!("{prefix}-account-id")),
        }),
        last_refresh: None,
        agent_identity: None,
    }
}

fn jwt_with_payload(payload: serde_json::Value) -> String {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header_b64 = encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
    let payload_b64 = encode(&serde_json::to_vec(&payload).expect("payload should serialize"));
    let signature_b64 = encode(b"sig");
    format!("{header_b64}.{payload_b64}.{signature_b64}")
}

#[test]
fn keyring_auth_storage_load_returns_deserialized_auth() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = KeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let expected = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("sk-test".to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
    };
    seed_keyring_with_auth(
        &mock_keyring,
        || compute_store_key(codex_home.path()),
        &expected,
    )?;

    let loaded = storage.load()?;
    assert_eq!(Some(expected), loaded);
    Ok(())
}

#[test]
fn keyring_auth_storage_compute_store_key_for_home_directory() -> anyhow::Result<()> {
    let codex_home = PathBuf::from("~/.codex");

    let key = compute_store_key(codex_home.as_path())?;

    assert_eq!(key, "cli|940db7b1d0e4eb40");
    Ok(())
}

#[test]
fn keyring_auth_storage_save_persists_and_removes_fallback_file() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = KeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let auth_file = get_auth_file(codex_home.path());
    std::fs::write(&auth_file, "stale")?;
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: Default::default(),
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            account_id: Some("account".to_string()),
        }),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
    };

    storage.save(&auth)?;

    let key = compute_store_key(codex_home.path())?;
    assert_keyring_saved_auth_and_removed_fallback(&mock_keyring, &key, codex_home.path(), &auth);
    Ok(())
}

#[test]
fn keyring_auth_storage_delete_removes_keyring_and_file() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = KeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let (key, auth_file) =
        seed_keyring_and_fallback_auth_file_for_delete(&mock_keyring, codex_home.path(), || {
            compute_store_key(codex_home.path())
        })?;

    let removed = storage.delete()?;

    assert!(removed, "delete should report removal");
    assert!(
        !mock_keyring.contains(&key),
        "keyring entry should be removed"
    );
    assert!(
        !auth_file.exists(),
        "fallback auth.json should be removed after keyring delete"
    );
    Ok(())
}

#[test]
fn auto_auth_storage_load_prefers_keyring_value() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = AutoAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let keyring_auth = auth_with_prefix("keyring");
    seed_keyring_with_auth(
        &mock_keyring,
        || compute_store_key(codex_home.path()),
        &keyring_auth,
    )?;

    let file_auth = auth_with_prefix("file");
    storage.file_storage.save(&file_auth)?;

    let loaded = storage.load()?;
    assert_eq!(loaded, Some(keyring_auth));
    Ok(())
}

#[test]
fn auto_auth_storage_load_uses_file_when_keyring_empty() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = AutoAuthStorage::new(codex_home.path().to_path_buf(), Arc::new(mock_keyring));

    let expected = auth_with_prefix("file-only");
    storage.file_storage.save(&expected)?;

    let loaded = storage.load()?;
    assert_eq!(loaded, Some(expected));
    Ok(())
}

#[test]
fn auto_auth_storage_load_falls_back_when_keyring_errors() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = AutoAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let key = compute_store_key(codex_home.path())?;
    mock_keyring.set_error(&key, KeyringError::Invalid("error".into(), "load".into()));

    let expected = auth_with_prefix("fallback");
    storage.file_storage.save(&expected)?;

    let loaded = storage.load()?;
    assert_eq!(loaded, Some(expected));
    Ok(())
}

#[test]
fn auto_auth_storage_save_prefers_keyring() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = AutoAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let key = compute_store_key(codex_home.path())?;

    let stale = auth_with_prefix("stale");
    storage.file_storage.save(&stale)?;

    let expected = auth_with_prefix("to-save");
    storage.save(&expected)?;

    assert_keyring_saved_auth_and_removed_fallback(
        &mock_keyring,
        &key,
        codex_home.path(),
        &expected,
    );
    Ok(())
}

#[test]
fn auto_auth_storage_save_falls_back_when_keyring_errors() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = AutoAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let key = compute_store_key(codex_home.path())?;
    mock_keyring.set_error(&key, KeyringError::Invalid("error".into(), "save".into()));

    let auth = auth_with_prefix("fallback");
    storage.save(&auth)?;

    let auth_file = get_auth_file(codex_home.path());
    assert!(
        auth_file.exists(),
        "fallback auth.json should be created when keyring save fails"
    );
    let saved = storage
        .file_storage
        .load()?
        .context("fallback auth should exist")?;
    assert_eq!(saved, auth);
    assert!(
        mock_keyring.saved_value(&key).is_none(),
        "keyring should not contain value when save fails"
    );
    Ok(())
}

#[test]
fn auto_auth_storage_delete_removes_keyring_and_file() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = AutoAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let (key, auth_file) =
        seed_keyring_and_fallback_auth_file_for_delete(&mock_keyring, codex_home.path(), || {
            compute_store_key(codex_home.path())
        })?;

    let removed = storage.delete()?;

    assert!(removed, "delete should report removal");
    assert!(
        !mock_keyring.contains(&key),
        "keyring entry should be removed"
    );
    assert!(
        !auth_file.exists(),
        "fallback auth.json should be removed after delete"
    );
    Ok(())
}

// ===========================================================================
// Codrex Phase 2.5: multi-provider schema tests
// ===========================================================================

#[test]
fn legacy_openai_only_auth_json_deserializes_into_auth_file() {
    // A file written by upstream Codex (no `providers` key) must still
    // load cleanly into the new `AuthFile` shape.
    let raw = r#"{
        "OPENAI_API_KEY": "sk-legacy",
        "auth_mode": "apikey"
    }"#;
    let parsed: AuthFile = serde_json::from_str(raw).expect("legacy auth.json parses");
    assert_eq!(parsed.openai.openai_api_key.as_deref(), Some("sk-legacy"));
    assert_eq!(parsed.openai.auth_mode, Some(AuthMode::ApiKey));
    assert!(parsed.providers.is_empty());
}

#[test]
fn auth_file_serialization_omits_empty_providers() {
    // A file with no providers must not include the `providers` key on
    // disk so we never write empty `{}` stubs.
    let auth = AuthFile {
        openai: AuthDotJson {
            auth_mode: Some(AuthMode::ApiKey),
            openai_api_key: Some("sk-legacy".to_string()),
            tokens: None,
            last_refresh: None,
            agent_identity: None,
        },
        providers: HashMap::new(),
    };
    let json = serde_json::to_value(&auth).expect("serialize");
    assert_eq!(json["OPENAI_API_KEY"], serde_json::json!("sk-legacy"));
    assert!(
        json.get("providers").is_none(),
        "empty providers map must not appear on disk"
    );
}

#[test]
fn auth_file_with_providers_serializes_at_top_level() {
    let mut providers: HashMap<String, ProviderCredentials> = HashMap::new();
    providers.insert(
        "minimax".to_string(),
        ProviderCredentials {
            api_key: "sk-cp-secret".to_string(),
            kind: Some("coding_plan".to_string()),
            last_verified: None,
        },
    );
    let auth = AuthFile {
        openai: AuthDotJson::default(),
        providers,
    };
    let json = serde_json::to_value(&auth).expect("serialize");
    // OpenAI-shaped fields are flattened at the top level.
    assert_eq!(json["OPENAI_API_KEY"], serde_json::Value::Null);
    // Providers map lives at top-level under `providers`.
    assert_eq!(
        json["providers"]["minimax"]["api_key"],
        serde_json::json!("sk-cp-secret")
    );
    assert_eq!(
        json["providers"]["minimax"]["kind"],
        serde_json::json!("coding_plan")
    );
    assert!(json["providers"]["minimax"].get("last_verified").is_none());
}

#[test]
fn auth_file_roundtrip_preserves_both_openai_and_providers() {
    let mut providers: HashMap<String, ProviderCredentials> = HashMap::new();
    providers.insert(
        "minimax".to_string(),
        ProviderCredentials {
            api_key: "sk-cp-1".to_string(),
            kind: Some("coding_plan".to_string()),
            last_verified: None,
        },
    );
    providers.insert(
        "qwen".to_string(),
        ProviderCredentials {
            api_key: "sk-q-2".to_string(),
            kind: None,
            last_verified: None,
        },
    );
    let original = AuthFile {
        openai: AuthDotJson {
            auth_mode: Some(AuthMode::ApiKey),
            openai_api_key: Some("sk-openai".to_string()),
            tokens: None,
            last_refresh: None,
            agent_identity: None,
        },
        providers,
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let roundtripped: AuthFile = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(roundtripped, original);
}

#[test]
fn auth_file_with_only_providers_loads_without_openai() {
    // Provider-only auth.json (no OpenAI auth) must deserialize cleanly.
    let raw = r#"{
        "providers": {
            "minimax": {
                "api_key": "sk-cp-only",
                "kind": "coding_plan"
            }
        }
    }"#;
    let parsed: AuthFile = serde_json::from_str(raw).expect("parses");
    assert_eq!(parsed.openai, AuthDotJson::default());
    assert_eq!(parsed.providers.len(), 1);
    assert_eq!(
        parsed.providers["minimax"].api_key,
        "sk-cp-only".to_string()
    );
}

#[test]
fn auth_file_is_empty_helper_distinguishes_meaningful_state() {
    let empty = AuthFile::default();
    assert!(empty.is_empty());

    let with_openai = AuthFile {
        openai: AuthDotJson {
            openai_api_key: Some("sk-x".into()),
            ..AuthDotJson::default()
        },
        providers: HashMap::new(),
    };
    assert!(!with_openai.is_empty());

    let mut providers = HashMap::new();
    providers.insert(
        "minimax".to_string(),
        ProviderCredentials {
            api_key: "sk-y".into(),
            kind: None,
            last_verified: None,
        },
    );
    let with_provider = AuthFile {
        openai: AuthDotJson::default(),
        providers,
    };
    assert!(!with_provider.is_empty());
}

#[test]
fn save_auth_preserves_providers_across_openai_writes() -> anyhow::Result<()> {
    use crate::auth::manager::load_provider_credentials;
    use crate::auth::manager::save_auth;
    use crate::auth::manager::save_provider_credentials;

    let codex_home = tempdir()?;
    let mode = AuthCredentialsStoreMode::File;

    // Step 1: store a MiniMax credential.
    save_provider_credentials(
        codex_home.path(),
        mode,
        "minimax",
        ProviderCredentials {
            api_key: "sk-cp-keep".to_string(),
            kind: Some("coding_plan".to_string()),
            last_verified: None,
        },
    )?;

    // Step 2: do an OpenAI-only login. Providers MUST survive.
    let openai = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("sk-openai-after".to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
    };
    save_auth(codex_home.path(), &openai, mode)?;

    // Step 3: verify both halves still readable.
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let loaded = storage.load_file()?.expect("auth file exists");
    assert_eq!(
        loaded.openai.openai_api_key.as_deref(),
        Some("sk-openai-after")
    );
    let minimax = load_provider_credentials(codex_home.path(), mode, "minimax")?
        .expect("minimax credential preserved across openai login");
    assert_eq!(minimax.api_key, "sk-cp-keep");
    assert_eq!(minimax.kind.as_deref(), Some("coding_plan"));
    Ok(())
}

#[test]
fn provider_credentials_remove_deletes_file_when_last_credential_gone() -> anyhow::Result<()> {
    use crate::auth::manager::remove_provider_credentials;
    use crate::auth::manager::save_provider_credentials;

    let codex_home = tempdir()?;
    let mode = AuthCredentialsStoreMode::File;
    save_provider_credentials(
        codex_home.path(),
        mode,
        "minimax",
        ProviderCredentials {
            api_key: "sk-cp".into(),
            kind: None,
            last_verified: None,
        },
    )?;
    assert!(get_auth_file(codex_home.path()).exists());

    let removed = remove_provider_credentials(codex_home.path(), mode, "minimax")?;
    assert!(removed, "remove_provider_credentials should report success");
    assert!(
        !get_auth_file(codex_home.path()).exists(),
        "auth.json should be deleted when no credentials remain"
    );
    Ok(())
}

#[test]
fn provider_credentials_remove_keeps_file_when_openai_present() -> anyhow::Result<()> {
    use crate::auth::manager::remove_provider_credentials;
    use crate::auth::manager::save_auth;
    use crate::auth::manager::save_provider_credentials;

    let codex_home = tempdir()?;
    let mode = AuthCredentialsStoreMode::File;
    save_auth(
        codex_home.path(),
        &AuthDotJson {
            auth_mode: Some(AuthMode::ApiKey),
            openai_api_key: Some("sk-openai".into()),
            tokens: None,
            last_refresh: None,
            agent_identity: None,
        },
        mode,
    )?;
    save_provider_credentials(
        codex_home.path(),
        mode,
        "minimax",
        ProviderCredentials {
            api_key: "sk-cp".into(),
            kind: None,
            last_verified: None,
        },
    )?;

    let removed = remove_provider_credentials(codex_home.path(), mode, "minimax")?;
    assert!(removed);
    assert!(
        get_auth_file(codex_home.path()).exists(),
        "auth.json must survive when OpenAI is still configured"
    );
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let loaded = storage.load_file()?.expect("auth still present");
    assert_eq!(loaded.openai.openai_api_key.as_deref(), Some("sk-openai"));
    assert!(loaded.providers.is_empty());
    Ok(())
}

#[test]
fn provider_credentials_remove_returns_false_when_absent() -> anyhow::Result<()> {
    use crate::auth::manager::remove_provider_credentials;
    let codex_home = tempdir()?;
    let removed =
        remove_provider_credentials(codex_home.path(), AuthCredentialsStoreMode::File, "minimax")?;
    assert!(!removed);
    Ok(())
}

#[test]
fn list_provider_credentials_returns_sorted_entries() -> anyhow::Result<()> {
    use crate::auth::manager::list_provider_credentials;
    use crate::auth::manager::save_provider_credentials;
    let codex_home = tempdir()?;
    let mode = AuthCredentialsStoreMode::File;
    for id in ["qwen", "minimax", "deepseek"] {
        save_provider_credentials(
            codex_home.path(),
            mode,
            id,
            ProviderCredentials {
                api_key: format!("sk-{id}"),
                kind: None,
                last_verified: None,
            },
        )?;
    }
    let listed = list_provider_credentials(codex_home.path(), mode)?;
    let ids: Vec<&str> = listed.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, vec!["deepseek", "minimax", "qwen"]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn provider_credentials_file_is_chmod_0600() -> anyhow::Result<()> {
    use crate::auth::manager::save_provider_credentials;
    use std::os::unix::fs::PermissionsExt;
    let codex_home = tempdir()?;
    save_provider_credentials(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        "minimax",
        ProviderCredentials {
            api_key: "sk-cp".into(),
            kind: None,
            last_verified: None,
        },
    )?;
    let auth_file = get_auth_file(codex_home.path());
    let perms = std::fs::metadata(&auth_file)?.permissions();
    let mode = perms.mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "auth.json must be chmod 0600 after provider credential write"
    );
    Ok(())
}
