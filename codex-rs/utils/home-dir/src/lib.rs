use codex_utils_absolute_path::AbsolutePathBuf;
use dirs::home_dir;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Default config directory name for Codrex (e.g. `~/.codrex`).
const CODREX_DIR_NAME: &str = ".codrex";

/// Legacy config directory name inherited from upstream OpenAI Codex
/// (e.g. `~/.codex`). Read for backwards compatibility.
const LEGACY_CODEX_DIR_NAME: &str = ".codex";

/// Ensures the legacy-fallback warning is only printed once per process.
static LEGACY_FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

/// Returns the path to the Codrex configuration directory.
///
/// Resolution order:
/// 1. `CODREX_HOME` environment variable (preferred).
/// 2. `CODEX_HOME` environment variable (legacy upstream var, kept for
///    migration from OpenAI Codex).
/// 3. `~/.codrex` if it exists.
/// 4. `~/.codex` if it exists (legacy upstream default; emits a warning the
///    first time it is used so users know to migrate).
/// 5. `~/.codrex` (default — does not need to exist on disk).
///
/// When an env var is set, the value must exist and be a directory; the
/// value is canonicalized and this function returns an `Err` otherwise.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let codrex_home_env = std::env::var("CODREX_HOME")
        .ok()
        .filter(|val| !val.is_empty());
    let legacy_codex_home_env = std::env::var("CODEX_HOME")
        .ok()
        .filter(|val| !val.is_empty());
    find_codex_home_from_env(codrex_home_env.as_deref(), legacy_codex_home_env.as_deref())
}

fn find_codex_home_from_env(
    codrex_home_env: Option<&str>,
    legacy_codex_home_env: Option<&str>,
) -> std::io::Result<AbsolutePathBuf> {
    // Honor `CODREX_HOME` first, then fall back to the legacy `CODEX_HOME`.
    let env_var = codrex_home_env
        .map(|val| ("CODREX_HOME", val))
        .or_else(|| legacy_codex_home_env.map(|val| ("CODEX_HOME", val)));

    match env_var {
        Some((var_name, val)) => resolve_env_dir(var_name, val),
        None => resolve_default_home_dir(),
    }
}

fn resolve_env_dir(var_name: &str, val: &str) -> std::io::Result<AbsolutePathBuf> {
    let path = PathBuf::from(val);
    let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{var_name} points to {val:?}, but that path does not exist"),
        ),
        _ => std::io::Error::new(
            err.kind(),
            format!("failed to read {var_name} {val:?}: {err}"),
        ),
    })?;

    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{var_name} points to {val:?}, but that path is not a directory"),
        ));
    }

    let canonical = path.canonicalize().map_err(|err| {
        std::io::Error::new(
            err.kind(),
            format!("failed to canonicalize {var_name} {val:?}: {err}"),
        )
    })?;
    AbsolutePathBuf::from_absolute_path(canonical)
}

fn resolve_default_home_dir() -> std::io::Result<AbsolutePathBuf> {
    let home = home_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not find home directory",
        )
    })?;

    let codrex_dir = home.join(CODREX_DIR_NAME);
    let legacy_dir = home.join(LEGACY_CODEX_DIR_NAME);

    // Prefer the new `~/.codrex` directory whenever it exists. Otherwise, if
    // a legacy `~/.codex` directory is present, use it for migration and warn
    // the user once. If neither exists, return the new default path (which
    // does not need to exist on disk yet).
    if codrex_dir.exists() {
        return AbsolutePathBuf::from_absolute_path(codrex_dir);
    }

    if legacy_dir.exists() {
        warn_legacy_fallback_once(&legacy_dir, &codrex_dir);
        return AbsolutePathBuf::from_absolute_path(legacy_dir);
    }

    AbsolutePathBuf::from_absolute_path(codrex_dir)
}

fn warn_legacy_fallback_once(legacy_dir: &std::path::Path, codrex_dir: &std::path::Path) {
    if LEGACY_FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "warning: using legacy Codex config dir {} — \
         move it to {} (or set CODREX_HOME) to silence this warning.",
        legacy_dir.display(),
        codrex_dir.display(),
    );
}

#[cfg(test)]
mod tests {
    use super::find_codex_home_from_env;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::io::ErrorKind;
    use tempfile::TempDir;

    #[test]
    fn codrex_home_env_missing_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codrex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codrex home path should be valid utf-8");

        let err =
            find_codex_home_from_env(Some(missing_str), None).expect_err("missing CODREX_HOME");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(
            err.to_string().contains("CODREX_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn legacy_codex_home_env_missing_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codex home path should be valid utf-8");

        let err =
            find_codex_home_from_env(None, Some(missing_str)).expect_err("missing CODEX_HOME");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(
            err.to_string().contains("CODEX_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn codrex_home_env_file_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let file_path = temp_home.path().join("codrex-home.txt");
        fs::write(&file_path, "not a directory").expect("write temp file");
        let file_str = file_path
            .to_str()
            .expect("file codrex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(file_str), None).expect_err("file CODREX_HOME");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn codrex_home_env_valid_directory_canonicalizes() {
        let temp_home = TempDir::new().expect("temp home");
        let temp_str = temp_home
            .path()
            .to_str()
            .expect("temp codrex home path should be valid utf-8");

        let resolved = find_codex_home_from_env(Some(temp_str), None).expect("valid CODREX_HOME");
        let expected = temp_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn codrex_home_takes_precedence_over_legacy_codex_home() {
        let codrex_home = TempDir::new().expect("codrex home");
        let codex_home = TempDir::new().expect("codex home");
        let codrex_str = codrex_home
            .path()
            .to_str()
            .expect("codrex home should be valid utf-8");
        let codex_str = codex_home
            .path()
            .to_str()
            .expect("codex home should be valid utf-8");

        let resolved =
            find_codex_home_from_env(Some(codrex_str), Some(codex_str)).expect("valid env vars");
        let expected = codrex_home
            .path()
            .canonicalize()
            .expect("canonicalize codrex home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn legacy_codex_home_used_when_codrex_home_unset() {
        let codex_home = TempDir::new().expect("codex home");
        let codex_str = codex_home
            .path()
            .to_str()
            .expect("codex home should be valid utf-8");

        let resolved = find_codex_home_from_env(None, Some(codex_str)).expect("valid CODEX_HOME");
        let expected = codex_home
            .path()
            .canonicalize()
            .expect("canonicalize codex home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }
}
