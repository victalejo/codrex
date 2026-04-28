//! Per-provider login / logout / list handlers for `codrex login <provider>`,
//! `codrex logout <provider>`, and `codrex login --list`.
//!
//! Phase 2.5 covers MiniMax. The handlers are written to be provider-agnostic
//! where possible so adding Qwen / DeepSeek / GLM in a future phase is a
//! matter of registering them in [`KnownProvider::resolve`] rather than
//! adding new flags.

use codex_login::ProviderCredentials;
use codex_login::auth_source;
use codex_login::list_provider_credentials;
use codex_login::remove_provider_credentials;
use codex_login::save_provider_credentials;
use codex_utils_cli::CliConfigOverrides;
use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;

use crate::login::load_config_or_exit;

/// Inputs that come from CLI flags on `codrex login <provider>`. Built in
/// `main.rs` so the handler stays narrowly typed.
#[derive(Debug, Clone, Default)]
pub struct ProviderLoginInput {
    pub with_api_key: bool,
    pub api_key_inline: Option<String>,
    pub coding_plan: bool,
    pub test_connection: bool,
}

/// Catalog of supported providers + the metadata each one needs for the
/// login flow. Today only MiniMax is here; new entries go in `resolve`.
struct KnownProvider {
    id: &'static str,
    display_name: &'static str,
    /// The env var(s) the runtime checks before falling back to auth.json.
    /// Used in error messages and the `--list` source column.
    env_var_standard: &'static str,
    env_var_alt: Option<&'static str>,
    /// Default model the optional connection test should send to.
    test_model: &'static str,
    /// Endpoint the connection test hits. Resolved at call time so users
    /// can override via the standard provider env vars.
    test_endpoint_default: &'static str,
    /// Env var name that overrides the test endpoint.
    test_endpoint_env: &'static str,
}

impl KnownProvider {
    fn resolve(id: &str) -> Option<Self> {
        match id {
            "minimax" => Some(Self {
                id: "minimax",
                display_name: "MiniMax",
                env_var_standard: codex_minimax::MINIMAX_API_KEY_ENV,
                env_var_alt: Some(codex_minimax::MINIMAX_CODING_PLAN_KEY_ENV),
                test_model: codex_minimax::MINIMAX_DEFAULT_MODEL,
                test_endpoint_default: codex_minimax::MINIMAX_DEFAULT_BASE_URL,
                test_endpoint_env: codex_minimax::MINIMAX_BASE_URL_ENV,
            }),
            _ => None,
        }
    }

    /// Comma-separated list of known provider ids — used in error messages
    /// when a user passes an unknown provider.
    fn known_list() -> &'static str {
        "minimax"
    }
}

/// Entry point for `codrex login <provider>`.
pub async fn run_login_provider(
    cli_config_overrides: CliConfigOverrides,
    provider_id: String,
    input: ProviderLoginInput,
) -> ! {
    let provider = match KnownProvider::resolve(&provider_id) {
        Some(p) => p,
        None => {
            eprintln!(
                "unknown provider '{provider_id}' (known: {})",
                KnownProvider::known_list()
            );
            std::process::exit(1);
        }
    };

    let config = load_config_or_exit(cli_config_overrides).await;
    let store_mode = config.cli_auth_credentials_store_mode;

    // Resolve the API key from the highest-priority source the caller
    // supplied. Order: --with-api-key (stdin) > --api-key (inline,
    // deprecated) > interactive prompt.
    let interactive_flow = !input.with_api_key && input.api_key_inline.is_none();
    let api_key = if input.with_api_key {
        crate::login::read_api_key_from_stdin()
    } else if let Some(inline) = input.api_key_inline.as_ref() {
        if inline.is_empty() {
            eprintln!(
                "--api-key requires a value (and stays in shell history; \
                 prefer --with-api-key for scripts)."
            );
            std::process::exit(1);
        }
        eprintln!(
            "warning: --api-key keeps the key in shell history. Prefer \
             `printenv {EV} | codrex login {ID} --with-api-key` in scripts.",
            EV = provider.env_var_standard,
            ID = provider.id,
        );
        inline.clone()
    } else if !std::io::stdin().is_terminal() {
        eprintln!(
            "no terminal attached and no key flags supplied — pipe a key \
             with `printenv {EV} | codrex login {ID} --with-api-key`.",
            EV = provider.env_var_standard,
            ID = provider.id,
        );
        std::process::exit(1);
    } else {
        prompt_hidden_api_key(provider.display_name)
    };
    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        eprintln!("API key was empty; aborting login.");
        std::process::exit(1);
    }

    let mut coding_plan = input.coding_plan;
    if interactive_flow && !coding_plan {
        coding_plan = prompt_yes_no(
            &format!("Is this a Coding Plan key for {}?", provider.display_name),
            false,
        );
    }

    let kind = if coding_plan {
        "coding_plan"
    } else {
        "standard"
    };
    let credentials = ProviderCredentials {
        api_key: api_key.clone(),
        kind: Some(kind.to_string()),
        last_verified: None,
    };

    if let Err(err) = save_provider_credentials(
        &config.codex_home,
        store_mode,
        provider.id,
        credentials.clone(),
    ) {
        eprintln!("Error saving credentials for '{}': {err}", provider.id);
        std::process::exit(1);
    }
    eprintln!(
        "✓ Saved credentials for provider '{}' ({kind})",
        provider.id
    );

    if cfg!(unix) {
        report_unix_permissions(&config.codex_home);
    }

    // Optional "test the connection" — only in the truly interactive flow,
    // or when --test-connection is explicitly requested. Keeps scripted
    // logins clean (no surprise prompts).
    let should_test = if input.test_connection {
        true
    } else if interactive_flow {
        prompt_yes_no("Test the connection?", true)
    } else {
        false
    };

    if should_test {
        match test_connection(&provider, &api_key).await {
            Ok(report) => {
                eprintln!(
                    "✓ Connection successful (model: {}, latency: {} ms)",
                    report.model, report.latency_ms
                );
                // Persist `last_verified` so `--list` can surface it.
                let mut updated = credentials;
                updated.last_verified = Some(chrono::Utc::now());
                let _ =
                    save_provider_credentials(&config.codex_home, store_mode, provider.id, updated);
            }
            Err(err) => {
                eprintln!(
                    "⚠ Connection test failed: {err}\n\
                     The credential WAS saved — re-run `codrex login {} \
                     --test-connection` after fixing the issue.",
                    provider.id
                );
            }
        }
    }

    std::process::exit(0);
}

/// Compatibility alias kept stable for the dispatch site in `main.rs`.
pub use run_login_provider as run_provider_login;

/// Entry point for `codrex logout <provider>`.
pub async fn run_provider_logout(
    cli_config_overrides: CliConfigOverrides,
    provider_id: String,
) -> ! {
    let provider = match KnownProvider::resolve(&provider_id) {
        Some(p) => p,
        None => {
            eprintln!(
                "unknown provider '{provider_id}' (known: {})",
                KnownProvider::known_list()
            );
            std::process::exit(1);
        }
    };

    let config = load_config_or_exit(cli_config_overrides).await;
    let store_mode = config.cli_auth_credentials_store_mode;

    match remove_provider_credentials(&config.codex_home, store_mode, provider.id) {
        Ok(true) => {
            eprintln!(
                "✓ Removed credentials for '{}' from local storage.",
                provider.id
            );
        }
        Ok(false) => {
            eprintln!(
                "No saved credentials for '{}' (nothing to remove).",
                provider.id
            );
        }
        Err(err) => {
            eprintln!("Error removing credentials for '{}': {err}", provider.id);
            std::process::exit(1);
        }
    }

    // Env-var advisory. We can't unset the user's shell env from inside
    // the binary; surface the actionable next step instead.
    let still_set: Vec<&str> = std::iter::once(provider.env_var_standard)
        .chain(provider.env_var_alt.into_iter())
        .filter(|var| {
            std::env::var(var)
                .ok()
                .is_some_and(|v| !v.trim().is_empty())
        })
        .collect();
    if !still_set.is_empty() {
        eprintln!();
        for var in &still_set {
            eprintln!("Note: {var} is still set in your environment.");
        }
        eprintln!(
            "To fully remove credentials:\n  unset {first}    # current shell\n  # then remove from ~/.zshrc, ~/.bash_profile, or wherever you set it",
            first = still_set[0]
        );
    }
    std::process::exit(0);
}

/// Entry point for `codrex login --list`.
pub async fn run_login_list(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let store_mode = config.cli_auth_credentials_store_mode;

    let mut rows: Vec<ListRow> = Vec::new();

    // OpenAI subset, if present in the auth file.
    if let Ok(Some(openai)) = codex_login::load_auth_dot_json(&config.codex_home, store_mode) {
        if openai.openai_api_key.is_some()
            || openai.tokens.is_some()
            || openai.agent_identity.is_some()
        {
            let kind = if openai.tokens.is_some() {
                "chatgpt"
            } else if openai.agent_identity.is_some() {
                "agent_identity"
            } else {
                "api_key"
            };
            rows.push(ListRow {
                provider: "openai".to_string(),
                kind: kind.to_string(),
                source: auth_source(store_mode).display_label(&config.codex_home),
                last_verified: openai
                    .last_refresh
                    .map(|d| d.format("%Y-%m-%d").to_string()),
            });
        }
    }

    // Other providers from auth.json::providers.
    if let Ok(entries) = list_provider_credentials(&config.codex_home, store_mode) {
        for (id, creds) in entries {
            rows.push(ListRow {
                provider: id,
                kind: creds.kind.unwrap_or_else(|| "—".to_string()),
                source: auth_source(store_mode).display_label(&config.codex_home),
                last_verified: creds
                    .last_verified
                    .map(|d| d.format("%Y-%m-%d").to_string()),
            });
        }
    }

    // Env-var-only credentials (shown for transparency; the key itself
    // is NEVER printed — only the env var name).
    for env_var in [
        codex_minimax::MINIMAX_API_KEY_ENV,
        codex_minimax::MINIMAX_CODING_PLAN_KEY_ENV,
    ] {
        if std::env::var(env_var)
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
            && !rows.iter().any(|r| r.provider == "minimax")
        {
            let kind = if env_var == codex_minimax::MINIMAX_CODING_PLAN_KEY_ENV {
                "coding_plan"
            } else {
                "standard"
            };
            rows.push(ListRow {
                provider: "minimax".to_string(),
                kind: kind.to_string(),
                source: format!("env: {env_var}"),
                last_verified: None,
            });
            break;
        }
    }
    if std::env::var("OPENAI_API_KEY")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
        && !rows.iter().any(|r| r.provider == "openai")
    {
        rows.push(ListRow {
            provider: "openai".to_string(),
            kind: "api_key".to_string(),
            source: "env: OPENAI_API_KEY".to_string(),
            last_verified: None,
        });
    }

    rows.sort_by(|a, b| a.provider.cmp(&b.provider));

    if rows.is_empty() {
        // Stdout, not stderr — the user is asking for output.
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "No credentials configured.\n");
        let _ = writeln!(stdout, "To get started:");
        let _ = writeln!(
            stdout,
            "  codrex login                    # OpenAI (interactive OAuth)"
        );
        let _ = writeln!(
            stdout,
            "  codrex login minimax            # MiniMax (paste API key)"
        );
        std::process::exit(0);
    }

    print_list_rows(&rows);
    std::process::exit(0);
}

#[derive(Debug)]
struct ListRow {
    provider: String,
    kind: String,
    source: String,
    last_verified: Option<String>,
}

fn print_list_rows(rows: &[ListRow]) {
    // Compute column widths so the table aligns regardless of provider id
    // length. We never print the API key — only the source it lives in.
    let mut w_provider = "Provider".len();
    let mut w_kind = "Type".len();
    let mut w_source = "Source".len();
    let w_last = "Last verified".len();
    for r in rows {
        w_provider = w_provider.max(r.provider.len());
        w_kind = w_kind.max(r.kind.len());
        w_source = w_source.max(r.source.len());
    }
    let mut stdout = std::io::stdout();
    let _ = writeln!(
        stdout,
        "{:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}",
        "Provider",
        "Type",
        "Source",
        "Last verified",
        w1 = w_provider,
        w2 = w_kind,
        w3 = w_source,
        w4 = w_last,
    );
    for r in rows {
        let last = r.last_verified.as_deref().unwrap_or("—");
        let _ = writeln!(
            stdout,
            "{:<w1$}  {:<w2$}  {:<w3$}  {:<w4$}",
            r.provider,
            r.kind,
            r.source,
            last,
            w1 = w_provider,
            w2 = w_kind,
            w3 = w_source,
            w4 = w_last,
        );
    }
}

fn prompt_hidden_api_key(provider_display_name: &str) -> String {
    let prompt = format!("Paste your {provider_display_name} API key (input hidden): ");
    match rpassword::prompt_password(prompt) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("Failed to read API key: {err}");
            std::process::exit(1);
        }
    }
}

fn prompt_yes_no(question: &str, default_yes: bool) -> bool {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{question} {suffix}: ");
    let _ = stderr.flush();
    let mut buf = String::new();
    if std::io::stdin().read_line(&mut buf).is_err() {
        return default_yes;
    }
    let answer = buf.trim().to_lowercase();
    if answer.is_empty() {
        return default_yes;
    }
    matches!(answer.as_str(), "y" | "yes")
}

fn report_unix_permissions(codex_home: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let auth = codex_home.join("auth.json");
        if let Ok(meta) = std::fs::metadata(&auth) {
            let mode = meta.permissions().mode() & 0o777;
            if mode == 0o600 {
                eprintln!("✓ Permissions verified: 0600");
            } else {
                eprintln!(
                    "⚠ auth.json permissions are {mode:o} (expected 0600). \
                     Run `chmod 0600 {}` to tighten.",
                    auth.display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = codex_home;
    }
}

#[derive(Debug)]
struct ConnectionReport {
    model: String,
    latency_ms: u128,
}

/// Hits the provider's chat completions endpoint with a minimal prompt and
/// reports the model name + latency. Only invoked when the user opts in.
async fn test_connection(
    provider: &KnownProvider,
    api_key: &str,
) -> Result<ConnectionReport, String> {
    let endpoint = std::env::var(provider.test_endpoint_env)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| provider.test_endpoint_default.to_string());
    let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": provider.test_model,
        "messages": [{"role": "user", "content": "reply with the word ok"}],
        "stream": false,
        "max_tokens": 4
    });
    let started = std::time::Instant::now();
    let resp = reqwest::Client::new()
        .post(&url)
        .bearer_auth(api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|err| format!("HTTP error: {err}"))?;
    let latency_ms = started.elapsed().as_millis();
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let snippet = body.chars().take(200).collect::<String>();
        return Err(format!("HTTP {status}: {snippet}"));
    }
    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|err| format!("invalid JSON response: {err}"))?;
    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(provider.test_model)
        .to_string();
    Ok(ConnectionReport { model, latency_ms })
}

#[allow(dead_code)]
fn drain_stdin() -> String {
    // Helper kept for forward compat with future provider-specific stdin
    // readers. Currently unused — `read_api_key_from_stdin` lives in
    // `login::login` for symmetry with the OpenAI flow.
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}
