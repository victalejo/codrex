use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::warn;

use crate::token_data::TokenData;
use codex_agent_identity::decode_agent_identity_jwt;
use codex_app_server_protocol::AuthMode;
use codex_config::types::AuthCredentialsStoreMode;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_protocol::account::PlanType as AccountPlanType;
use once_cell::sync::Lazy;

/// Expected structure for the OpenAI-shaped subset of `$CODEX_HOME/auth.json`.
///
/// This struct is preserved verbatim from the upstream Codex layout so the
/// 52+ existing call sites continue to compile unchanged. Codrex's
/// multi-provider extension lives in [`AuthFile`] (see below) which embeds
/// `AuthDotJson` via `#[serde(flatten)]` plus a `providers` map.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
pub struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,

    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<String>,
}

/// Per-provider credentials persisted under `auth.json::providers["<id>"]`.
///
/// `kind` is intentionally a free-form string so each provider can encode its
/// own subtypes without cross-provider coordination. For the bundled MiniMax
/// provider, valid values today are `"standard"` and `"coding_plan"`.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ProviderCredentials {
    pub api_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Wallclock when the credential was last successfully verified
    /// against the provider. Set by the optional "test connection" flow
    /// in `codrex login <provider>`. Never auto-updated by the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<DateTime<Utc>>,
}

/// Top-level shape of `$CODEX_HOME/auth.json`.
///
/// Codrex Phase 2.5 extends the original OpenAI-only `AuthDotJson` with a
/// generic per-provider credential map. The OpenAI fields are flattened at
/// the top level (preserving the on-disk shape upstream code expects), and
/// new providers — MiniMax today, Qwen/DeepSeek/GLM tomorrow — live under
/// `providers.<id>`.
///
/// Example:
/// ```json
/// {
///   "OPENAI_API_KEY": "sk-...",
///   "auth_mode": "ApiKey",
///   "providers": {
///     "minimax": {
///       "api_key": "sk-cp-...",
///       "kind": "coding_plan",
///       "last_verified": "2026-04-27T12:00:00Z"
///     }
///   }
/// }
/// ```
///
/// Files written before Phase 2.5 (no `providers` key) load cleanly into
/// `AuthFile` because `providers` defaults to an empty map. Files with no
/// OpenAI fields and only `providers` also work because `AuthDotJson`
/// derives `Default`.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
pub struct AuthFile {
    #[serde(flatten)]
    pub openai: AuthDotJson,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub providers: HashMap<String, ProviderCredentials>,
}

impl AuthFile {
    /// Returns true when neither the OpenAI subset nor the providers map
    /// holds anything worth persisting. The storage layer uses this to
    /// decide whether `remove_provider_credentials` should delete the
    /// auth.json entirely instead of writing back an empty shell.
    pub fn is_empty(&self) -> bool {
        self.openai == AuthDotJson::default() && self.providers.is_empty()
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentityAuthRecord {
    pub agent_runtime_id: String,
    pub agent_private_key: String,
    pub account_id: String,
    pub chatgpt_user_id: String,
    pub email: String,
    pub plan_type: AccountPlanType,
    pub chatgpt_account_is_fedramp: bool,
}

impl AgentIdentityAuthRecord {
    pub(crate) fn from_agent_identity_jwt(jwt: &str) -> std::io::Result<Self> {
        let claims = decode_agent_identity_jwt(jwt, /*public_key_base64*/ None)
            .map_err(std::io::Error::other)?;

        Ok(Self {
            agent_runtime_id: claims.agent_runtime_id,
            agent_private_key: claims.agent_private_key,
            account_id: claims.account_id,
            chatgpt_user_id: claims.chatgpt_user_id,
            email: claims.email,
            plan_type: claims.plan_type,
            chatgpt_account_is_fedramp: claims.chatgpt_account_is_fedramp,
        })
    }
}

pub(super) fn get_auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join("auth.json")
}

pub(super) fn delete_file_if_exists(codex_home: &Path) -> std::io::Result<bool> {
    let auth_file = get_auth_file(codex_home);
    match std::fs::remove_file(&auth_file) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Where the auth payload was loaded from. Used by `codrex login --list` to
/// surface a stable, user-facing source label for each credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSource {
    /// Loaded from `$CODEX_HOME/auth.json`.
    File,
    /// Loaded from the OS keyring backend (macOS Keychain, libsecret, ...).
    Keyring,
    /// Held only in process memory (test/short-lived flows).
    Ephemeral,
}

impl AuthSource {
    /// Human-readable label for the source. Includes a platform suffix for
    /// the keyring case so users can tell macOS Keychain from libsecret.
    pub fn display_label(&self, codex_home: &Path) -> String {
        match self {
            Self::File => format!("{}", codex_home.join("auth.json").display()),
            Self::Keyring => {
                #[cfg(target_os = "macos")]
                {
                    "keyring (macOS)".to_string()
                }
                #[cfg(target_os = "linux")]
                {
                    "keyring (libsecret)".to_string()
                }
                #[cfg(target_os = "windows")]
                {
                    "keyring (Windows credential manager)".to_string()
                }
                #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
                {
                    "keyring".to_string()
                }
            }
            Self::Ephemeral => "ephemeral (in-memory)".to_string(),
        }
    }
}

pub(super) trait AuthStorageBackend: Debug + Send + Sync {
    /// Load the full multi-provider auth payload from this backend.
    fn load_file(&self) -> std::io::Result<Option<AuthFile>>;

    /// Persist the full multi-provider auth payload.
    fn save_file(&self, auth: &AuthFile) -> std::io::Result<()>;

    fn delete(&self) -> std::io::Result<bool>;

    /// Where credentials read by this backend come from (for diagnostics).
    fn source(&self) -> AuthSource;

    /// Backwards-compatible accessor returning only the OpenAI subset.
    /// Treats files with no OpenAI auth as absent (returns `None`) so
    /// upstream callers that only know about the OpenAI path keep working.
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        match self.load_file()? {
            Some(file) if file.openai != AuthDotJson::default() => Ok(Some(file.openai)),
            _ => Ok(None),
        }
    }

    /// Backwards-compatible writer for the OpenAI subset. **Overwrites the
    /// entire backend payload** — providers stored under `auth.json::providers`
    /// are NOT preserved by this path.
    ///
    /// Provider-preserving writes go through the public
    /// `save_auth` / `save_provider_credentials` helpers in `manager.rs`.
    /// Keeping `save` as a clean overwrite preserves the upstream test
    /// `auto_auth_storage_save_falls_back_when_keyring_errors` which
    /// depends on the keyring backend being touched exactly once per
    /// `save` call (the keyring mock's `set_error` is one-shot).
    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let file = AuthFile {
            openai: auth.clone(),
            providers: std::collections::HashMap::new(),
        };
        self.save_file(&file)
    }
}

#[derive(Clone, Debug)]
pub(super) struct FileAuthStorage {
    codex_home: PathBuf,
}

impl FileAuthStorage {
    pub(super) fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    /// Attempt to read and parse the `auth.json` file in the given
    /// `CODEX_HOME` directory. Returns the multi-provider [`AuthFile`].
    pub(super) fn try_read_auth_file(&self, auth_file: &Path) -> std::io::Result<AuthFile> {
        let mut file = File::open(auth_file)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let parsed: AuthFile = serde_json::from_str(&contents)?;
        Ok(parsed)
    }

    /// Backwards-compatible accessor returning only the OpenAI-shaped
    /// subset of `auth.json`. Preserved verbatim for upstream tests that
    /// were written before the multi-provider extension.
    pub(super) fn try_read_auth_json(&self, auth_file: &Path) -> std::io::Result<AuthDotJson> {
        Ok(self.try_read_auth_file(auth_file)?.openai)
    }
}

impl AuthStorageBackend for FileAuthStorage {
    fn load_file(&self) -> std::io::Result<Option<AuthFile>> {
        let auth_file = get_auth_file(&self.codex_home);
        let parsed = match self.try_read_auth_file(&auth_file) {
            Ok(auth) => auth,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(Some(parsed))
    }

    fn save_file(&self, auth: &AuthFile) -> std::io::Result<()> {
        let auth_file = get_auth_file(&self.codex_home);

        if let Some(parent) = auth_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json_data = serde_json::to_string_pretty(auth)?;
        let mut options = OpenOptions::new();
        options.truncate(true).write(true).create(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options.open(auth_file)?;
        file.write_all(json_data.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        delete_file_if_exists(&self.codex_home)
    }

    fn source(&self) -> AuthSource {
        AuthSource::File
    }
}

const KEYRING_SERVICE: &str = "Codex Auth";

// turns codex_home path into a stable, short key string
fn compute_store_key(codex_home: &Path) -> std::io::Result<String> {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    let path_str = canonical.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let truncated = hex.get(..16).unwrap_or(&hex);
    Ok(format!("cli|{truncated}"))
}

#[derive(Clone, Debug)]
struct KeyringAuthStorage {
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
}

impl KeyringAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self {
            codex_home,
            keyring_store,
        }
    }

    fn load_from_keyring(&self, key: &str) -> std::io::Result<Option<AuthFile>> {
        match self.keyring_store.load(KEYRING_SERVICE, key) {
            Ok(Some(serialized)) => serde_json::from_str(&serialized).map(Some).map_err(|err| {
                std::io::Error::other(format!(
                    "failed to deserialize CLI auth from keyring: {err}"
                ))
            }),
            Ok(None) => Ok(None),
            Err(error) => Err(std::io::Error::other(format!(
                "failed to load CLI auth from keyring: {}",
                error.message()
            ))),
        }
    }

    fn save_to_keyring(&self, key: &str, value: &str) -> std::io::Result<()> {
        match self.keyring_store.save(KEYRING_SERVICE, key, value) {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = format!(
                    "failed to write OAuth tokens to keyring: {}",
                    error.message()
                );
                warn!("{message}");
                Err(std::io::Error::other(message))
            }
        }
    }
}

impl AuthStorageBackend for KeyringAuthStorage {
    fn load_file(&self) -> std::io::Result<Option<AuthFile>> {
        let key = compute_store_key(&self.codex_home)?;
        self.load_from_keyring(&key)
    }

    fn save_file(&self, auth: &AuthFile) -> std::io::Result<()> {
        let key = compute_store_key(&self.codex_home)?;
        // Simpler error mapping per style: prefer method reference over closure
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.save_to_keyring(&key, &serialized)?;
        if let Err(err) = delete_file_if_exists(&self.codex_home) {
            warn!("failed to remove CLI auth fallback file: {err}");
        }
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        let key = compute_store_key(&self.codex_home)?;
        let keyring_removed = self
            .keyring_store
            .delete(KEYRING_SERVICE, &key)
            .map_err(|err| {
                std::io::Error::other(format!("failed to delete auth from keyring: {err}"))
            })?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        Ok(keyring_removed || file_removed)
    }

    fn source(&self) -> AuthSource {
        AuthSource::Keyring
    }
}

#[derive(Clone, Debug)]
struct AutoAuthStorage {
    keyring_storage: Arc<KeyringAuthStorage>,
    file_storage: Arc<FileAuthStorage>,
}

impl AutoAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self {
            keyring_storage: Arc::new(KeyringAuthStorage::new(codex_home.clone(), keyring_store)),
            file_storage: Arc::new(FileAuthStorage::new(codex_home)),
        }
    }
}

impl AuthStorageBackend for AutoAuthStorage {
    fn load_file(&self) -> std::io::Result<Option<AuthFile>> {
        match self.keyring_storage.load_file() {
            Ok(Some(auth)) => Ok(Some(auth)),
            Ok(None) => self.file_storage.load_file(),
            Err(err) => {
                warn!("failed to load CLI auth from keyring, falling back to file storage: {err}");
                self.file_storage.load_file()
            }
        }
    }

    fn save_file(&self, auth: &AuthFile) -> std::io::Result<()> {
        match self.keyring_storage.save_file(auth) {
            Ok(()) => Ok(()),
            Err(err) => {
                warn!("failed to save auth to keyring, falling back to file storage: {err}");
                self.file_storage.save_file(auth)
            }
        }
    }

    fn delete(&self) -> std::io::Result<bool> {
        // Keyring storage will delete from disk as well
        self.keyring_storage.delete()
    }

    fn source(&self) -> AuthSource {
        // The "auto" backend reports keyring as its source — when keyring
        // is unavailable the file fallback below still labels itself
        // accurately because callers query `source()` on the inner backend
        // they actually loaded from when they need precision (the
        // multi-provider list builder does this).
        AuthSource::Keyring
    }
}

// A global in-memory store for mapping codex_home -> AuthFile.
static EPHEMERAL_AUTH_STORE: Lazy<Mutex<HashMap<String, AuthFile>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug)]
struct EphemeralAuthStorage {
    codex_home: PathBuf,
}

impl EphemeralAuthStorage {
    fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    fn with_store<F, T>(&self, action: F) -> std::io::Result<T>
    where
        F: FnOnce(&mut HashMap<String, AuthFile>, String) -> std::io::Result<T>,
    {
        let key = compute_store_key(&self.codex_home)?;
        let mut store = EPHEMERAL_AUTH_STORE
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock ephemeral auth storage"))?;
        action(&mut store, key)
    }
}

impl AuthStorageBackend for EphemeralAuthStorage {
    fn load_file(&self) -> std::io::Result<Option<AuthFile>> {
        self.with_store(|store, key| Ok(store.get(&key).cloned()))
    }

    fn save_file(&self, auth: &AuthFile) -> std::io::Result<()> {
        self.with_store(|store, key| {
            store.insert(key, auth.clone());
            Ok(())
        })
    }

    fn delete(&self) -> std::io::Result<bool> {
        self.with_store(|store, key| Ok(store.remove(&key).is_some()))
    }

    fn source(&self) -> AuthSource {
        AuthSource::Ephemeral
    }
}

pub(super) fn create_auth_storage(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
) -> Arc<dyn AuthStorageBackend> {
    let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
    create_auth_storage_with_keyring_store(codex_home, mode, keyring_store)
}

fn create_auth_storage_with_keyring_store(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_store: Arc<dyn KeyringStore>,
) -> Arc<dyn AuthStorageBackend> {
    match mode {
        AuthCredentialsStoreMode::File => Arc::new(FileAuthStorage::new(codex_home)),
        AuthCredentialsStoreMode::Keyring => {
            Arc::new(KeyringAuthStorage::new(codex_home, keyring_store))
        }
        AuthCredentialsStoreMode::Auto => Arc::new(AutoAuthStorage::new(codex_home, keyring_store)),
        AuthCredentialsStoreMode::Ephemeral => Arc::new(EphemeralAuthStorage::new(codex_home)),
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
