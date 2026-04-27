//! Cross-cutting integration tests for Phase 2.5 multi-provider auth:
//! `codrex login <provider>`, `codrex logout <provider>`, and
//! `codrex login --list`.
//!
//! These run the freshly-built `codrex` binary against a temp
//! CODEX_HOME, exercising the full pipeline from clap → handlers →
//! `codex-login` storage. Tests serialize on `serial_test::serial`
//! when they touch process env vars.

use anyhow::Result;
use assert_cmd::Command;
use predicates::str::contains;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

fn codrex(codex_home: &Path) -> Result<Command> {
    let mut cmd = Command::new(codex_utils_cargo_bin::cargo_bin("codrex")?);
    cmd.env("CODREX_HOME", codex_home);
    cmd.env("CODEX_HOME", codex_home);
    // Strip env vars that could otherwise contaminate the test.
    cmd.env_remove("MINIMAX_API_KEY");
    cmd.env_remove("MINIMAX_CODING_PLAN_KEY");
    cmd.env_remove("OPENAI_API_KEY");
    Ok(cmd)
}

fn write_file_auth_config(codex_home: &Path) -> Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )?;
    Ok(())
}

fn read_auth_json(codex_home: &Path) -> Result<Value> {
    let auth_json = std::fs::read_to_string(codex_home.join("auth.json"))?;
    Ok(serde_json::from_str(&auth_json)?)
}

#[test]
fn login_minimax_with_stdin_writes_provider_credentials() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    let mut cmd = codrex(codex_home.path())?;
    cmd.args(["login", "minimax", "--with-api-key", "--coding-plan"])
        .write_stdin("sk-test-cp\n")
        .assert()
        .success()
        .stderr(contains("Saved credentials for provider 'minimax'"))
        .stderr(contains("(coding_plan)"))
        .stderr(contains("Permissions verified: 0600"));

    let auth = read_auth_json(codex_home.path())?;
    assert_eq!(
        auth["providers"]["minimax"]["api_key"], "sk-test-cp",
        "minimax api_key must round-trip into the providers map"
    );
    assert_eq!(auth["providers"]["minimax"]["kind"], "coding_plan");
    Ok(())
}

#[cfg(unix)]
#[test]
fn login_minimax_writes_file_chmod_0600() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    let mut cmd = codrex(codex_home.path())?;
    cmd.args(["login", "minimax", "--with-api-key"])
        .write_stdin("sk-perms\n")
        .assert()
        .success();

    let perms = std::fs::metadata(codex_home.path().join("auth.json"))?.permissions();
    let mode = perms.mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "auth.json must be chmod 0600 after `codrex login <provider>`"
    );
    Ok(())
}

#[test]
fn login_then_logout_roundtrip_with_file_backend() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    // login
    codrex(codex_home.path())?
        .args(["login", "minimax", "--with-api-key"])
        .write_stdin("sk-roundtrip\n")
        .assert()
        .success();
    assert!(codex_home.path().join("auth.json").exists());

    // list shows the provider
    let assert = codrex(codex_home.path())?
        .args(["login", "--list"])
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?.to_string();
    assert!(stdout.contains("minimax"), "list must surface the provider");
    assert!(
        !stdout.contains("sk-roundtrip"),
        "API key MUST never appear in --list output"
    );

    // logout removes it
    codrex(codex_home.path())?
        .args(["logout", "minimax"])
        .assert()
        .success()
        .stderr(contains("Removed credentials for 'minimax'"));

    // file is gone (last credential)
    assert!(
        !codex_home.path().join("auth.json").exists(),
        "auth.json must be deleted when the last credential is removed"
    );

    // list shows the empty-state hint
    codrex(codex_home.path())?
        .args(["login", "--list"])
        .assert()
        .success()
        .stdout(contains("No credentials configured"))
        .stdout(contains("codrex login minimax"));
    Ok(())
}

#[test]
fn logout_minimax_keeps_openai_credentials_intact() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    // OpenAI login
    codrex(codex_home.path())?
        .args([
            "-c",
            "forced_login_method=\"api\"",
            "login",
            "--with-api-key",
        ])
        .write_stdin("sk-openai-keep\n")
        .assert()
        .success();

    // MiniMax login
    codrex(codex_home.path())?
        .args(["login", "minimax", "--with-api-key"])
        .write_stdin("sk-minimax\n")
        .assert()
        .success();

    // Verify both present
    let auth = read_auth_json(codex_home.path())?;
    assert_eq!(auth["OPENAI_API_KEY"], "sk-openai-keep");
    assert_eq!(auth["providers"]["minimax"]["api_key"], "sk-minimax");

    // Logout MiniMax — OpenAI must survive.
    codrex(codex_home.path())?
        .args(["logout", "minimax"])
        .assert()
        .success();

    let auth = read_auth_json(codex_home.path())?;
    assert_eq!(
        auth["OPENAI_API_KEY"], "sk-openai-keep",
        "OpenAI auth must survive a surgical provider logout"
    );
    assert!(
        auth.get("providers")
            .and_then(|p| p.as_object())
            .is_none_or(|o| o.is_empty()),
        "minimax must be gone from providers after logout"
    );
    Ok(())
}

#[test]
fn logout_minimax_emits_env_var_hint_when_set() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    codrex(codex_home.path())?
        .args(["login", "minimax", "--with-api-key"])
        .write_stdin("sk-foo\n")
        .assert()
        .success();

    // Logout with MINIMAX_API_KEY still set in env — hint must fire.
    let mut cmd = codrex(codex_home.path())?;
    cmd.env("MINIMAX_API_KEY", "sk-shell-still-here")
        .args(["logout", "minimax"])
        .assert()
        .success()
        .stderr(contains("MINIMAX_API_KEY is still set"))
        .stderr(contains("unset MINIMAX_API_KEY"));
    Ok(())
}

#[test]
fn login_list_mixes_file_credentials_and_env_vars() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    // Save MiniMax in file.
    codrex(codex_home.path())?
        .args(["login", "minimax", "--with-api-key", "--coding-plan"])
        .write_stdin("sk-cp-file\n")
        .assert()
        .success();

    // List with OPENAI_API_KEY in env (not in file). Both must appear.
    let mut cmd = codrex(codex_home.path())?;
    let assert = cmd
        .env("OPENAI_API_KEY", "sk-env-openai")
        .args(["login", "--list"])
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?.to_string();
    assert!(stdout.contains("minimax"));
    assert!(stdout.contains("openai"));
    assert!(stdout.contains("env: OPENAI_API_KEY"));
    assert!(
        !stdout.contains("sk-env-openai") && !stdout.contains("sk-cp-file"),
        "no API key should appear in --list output: {stdout}"
    );
    Ok(())
}

#[test]
fn login_unknown_provider_errors_with_known_list() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    codrex(codex_home.path())?
        .args(["login", "bogus-provider", "--with-api-key"])
        .write_stdin("sk-noop\n")
        .assert()
        .failure()
        .stderr(contains("unknown provider 'bogus-provider'"))
        .stderr(contains("known: minimax"));
    Ok(())
}

#[test]
fn login_list_with_provider_arg_is_rejected_by_clap() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    codrex(codex_home.path())?
        .args(["login", "--list", "minimax"])
        .assert()
        .failure()
        .stderr(contains("'--list' cannot be used with"));
    Ok(())
}

#[test]
fn deprecated_api_key_flag_warns_about_shell_history() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    codrex(codex_home.path())?
        .args(["login", "minimax", "--api-key", "sk-inline"])
        .assert()
        .success()
        .stderr(contains("--api-key keeps the key in shell history"))
        .stderr(contains("Saved credentials for provider 'minimax'"));
    Ok(())
}

#[test]
fn login_minimax_empty_inline_key_errors_clearly() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    // `--api-key` with an empty value (e.g. `--api-key=""`) must not save.
    codrex(codex_home.path())?
        .args(["login", "minimax", "--api-key", ""])
        .assert()
        .failure()
        .stderr(contains("--api-key requires a value"));
    Ok(())
}

#[test]
fn login_list_when_only_env_var_set_shows_env_source() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    // No file. Env var only.
    let mut cmd = codrex(codex_home.path())?;
    let assert = cmd
        .env("MINIMAX_CODING_PLAN_KEY", "sk-cp-env-only")
        .args(["login", "--list"])
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?.to_string();
    assert!(stdout.contains("minimax"));
    assert!(stdout.contains("coding_plan"));
    assert!(stdout.contains("env: MINIMAX_CODING_PLAN_KEY"));
    assert!(!stdout.contains("sk-cp-env-only"));
    Ok(())
}

#[test]
fn logout_unknown_provider_errors_with_known_list() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    codrex(codex_home.path())?
        .args(["logout", "bogus-provider"])
        .assert()
        .failure()
        .stderr(contains("unknown provider 'bogus-provider'"));
    Ok(())
}

#[test]
fn logout_minimax_when_nothing_saved_returns_friendly_message() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_file_auth_config(codex_home.path())?;

    codrex(codex_home.path())?
        .args(["logout", "minimax"])
        .assert()
        .success()
        .stderr(contains("No saved credentials for 'minimax'"));
    Ok(())
}
