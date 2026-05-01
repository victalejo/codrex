use super::MiniMaxContextFileSource;
use super::git_status::GitStatusCandidate;
use super::git_status::MAX_GIT_CONTEXT_FILES;
use super::git_status::collect_git_status_candidates;
use codex_git_utils::get_git_repo_root;
use regex_lite::Regex;
use std::collections::HashSet;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

pub(crate) const DEFAULT_CONTEXT_FILE_MAX_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ContextPackDiagnostics {
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackedContextFile {
    pub path: String,
    pub content: String,
    pub truncated: bool,
    pub redacted: bool,
    pub source: MiniMaxContextFileSource,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ContextPack {
    pub files: Vec<PackedContextFile>,
    pub git_modified_paths: Vec<String>,
    pub diagnostics: ContextPackDiagnostics,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExplicitContextSnippet {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ContextPackRequest<'a> {
    pub explicit_files: &'a [String],
    pub explicit_snippets: &'a [ExplicitContextSnippet],
    pub task_text: &'a str,
    pub include_modified_files: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextCandidateSource {
    ExplicitFile,
    ExplicitSnippet,
    TaskMention,
    GitModified,
}

impl ContextCandidateSource {
    fn omitted_message(self, path: &str, reason: &str) -> String {
        match self {
            Self::GitModified => format!("omitted git modified file {path}: {reason}"),
            Self::ExplicitFile | Self::ExplicitSnippet | Self::TaskMention => {
                format!("omitted {path}: {reason}")
            }
        }
    }

    fn is_git_modified(self) -> bool {
        matches!(self, Self::GitModified)
    }

    fn context_file_source(self) -> MiniMaxContextFileSource {
        match self {
            Self::ExplicitFile => MiniMaxContextFileSource::ExplicitFile,
            Self::ExplicitSnippet => MiniMaxContextFileSource::ExplicitSnippet,
            Self::TaskMention => MiniMaxContextFileSource::TaskMention,
            Self::GitModified => MiniMaxContextFileSource::GitModified,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ContextPacker<'a> {
    cwd: &'a Path,
    repo_root: PathBuf,
    repo_root_canonical: PathBuf,
    total_budget_bytes: usize,
    per_file_budget_bytes: usize,
}

impl<'a> ContextPacker<'a> {
    pub(crate) fn new(
        cwd: &'a Path,
        total_budget_bytes: usize,
        per_file_budget_bytes: usize,
    ) -> Self {
        let repo_root = get_git_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
        let repo_root_canonical =
            dunce::canonicalize(&repo_root).unwrap_or_else(|_| repo_root.clone());

        Self {
            cwd,
            repo_root,
            repo_root_canonical,
            total_budget_bytes,
            per_file_budget_bytes,
        }
    }

    pub(crate) fn pack(
        &self,
        explicit_files: &[String],
        explicit_snippets: &[ExplicitContextSnippet],
        task_text: &str,
    ) -> ContextPack {
        self.pack_with_request(ContextPackRequest {
            explicit_files,
            explicit_snippets,
            task_text,
            include_modified_files: false,
        })
    }

    pub(crate) fn pack_with_request(&self, request: ContextPackRequest<'_>) -> ContextPack {
        let mut pack = ContextPack::default();
        let mut seen = HashSet::new();
        let mut remaining = self.total_budget_bytes;

        for snippet in request.explicit_snippets {
            let normalized_path = normalize_display_path(snippet.path.as_str());
            if !seen.insert(normalized_path.clone()) {
                continue;
            }

            if is_path_traversal(Path::new(&normalized_path)) {
                push_diag(
                    &mut pack.diagnostics.messages,
                    format!("omitted {}: outside repo (path traversal)", snippet.path),
                );
                continue;
            }

            if is_denied_path(Path::new(&normalized_path)) {
                push_diag(
                    &mut pack.diagnostics.messages,
                    format!("omitted {}: denied path", snippet.path),
                );
                continue;
            }

            if remaining == 0 {
                mark_total_budget_exhausted(&mut pack);
                break;
            }

            let added = add_context_entry(
                &mut pack,
                &mut remaining,
                normalized_path,
                snippet.content.clone(),
                self.per_file_budget_bytes,
                ContextCandidateSource::ExplicitSnippet,
            );
            if !added && remaining == 0 {
                break;
            }
        }

        for context_file in request.explicit_files {
            if remaining == 0 {
                mark_total_budget_exhausted(&mut pack);
                break;
            }

            self.pack_file_candidate(
                context_file,
                ContextCandidateSource::ExplicitFile,
                &mut pack,
                &mut seen,
                &mut remaining,
            );
        }

        for task_path in mentioned_task_paths(request.task_text) {
            if remaining == 0 {
                mark_total_budget_exhausted(&mut pack);
                break;
            }

            self.pack_file_candidate(
                task_path.as_str(),
                ContextCandidateSource::TaskMention,
                &mut pack,
                &mut seen,
                &mut remaining,
            );
        }

        if request.include_modified_files {
            self.pack_git_modified_files(&mut pack, &mut seen, &mut remaining);
        }

        pack
    }

    fn pack_file_candidate(
        &self,
        requested_path: &str,
        source: ContextCandidateSource,
        pack: &mut ContextPack,
        seen: &mut HashSet<String>,
        remaining: &mut usize,
    ) {
        let resolution = self.resolve_path(requested_path);
        let dedupe_key = resolution.display_path().map_or_else(
            || normalize_display_path(requested_path),
            normalize_display_path,
        );
        if !seen.insert(dedupe_key) {
            return;
        }

        let display_path = resolution
            .display_path()
            .map_or_else(|| normalize_display_path(requested_path), str::to_string);
        if *remaining == 0 {
            mark_total_budget_exhausted(pack);
            push_diag(
                &mut pack.diagnostics.messages,
                source.omitted_message(display_path.as_str(), "budget exhausted"),
            );
            return;
        }

        match resolution {
            PathResolution::Resolved {
                absolute_path,
                display_path,
            } => {
                let Ok(bytes) = std::fs::read(&absolute_path) else {
                    push_diag(
                        &mut pack.diagnostics.messages,
                        source.omitted_message(display_path.as_str(), "file missing"),
                    );
                    return;
                };

                if bytes.contains(&0) {
                    push_diag(
                        &mut pack.diagnostics.messages,
                        source.omitted_message(display_path.as_str(), "binary/non-utf8"),
                    );
                    return;
                }

                let Ok(content) = String::from_utf8(bytes) else {
                    push_diag(
                        &mut pack.diagnostics.messages,
                        source.omitted_message(display_path.as_str(), "binary/non-utf8"),
                    );
                    return;
                };

                let added = add_context_entry(
                    pack,
                    remaining,
                    display_path.clone(),
                    content,
                    self.per_file_budget_bytes,
                    source,
                );
                if added && source.is_git_modified() {
                    pack.git_modified_paths.push(display_path);
                }
            }
            PathResolution::Denied { display_path } => push_diag(
                &mut pack.diagnostics.messages,
                source.omitted_message(display_path.as_str(), "denied path"),
            ),
            PathResolution::OutsideRepo { display_path } => push_diag(
                &mut pack.diagnostics.messages,
                source.omitted_message(display_path.as_str(), "outside repo"),
            ),
            PathResolution::Missing { display_path } => push_diag(
                &mut pack.diagnostics.messages,
                source.omitted_message(display_path.as_str(), "file missing"),
            ),
        }
    }

    fn pack_git_modified_files(
        &self,
        pack: &mut ContextPack,
        seen: &mut HashSet<String>,
        remaining: &mut usize,
    ) {
        let git_candidates = match collect_git_status_candidates(&self.repo_root) {
            Ok(candidates) => candidates,
            Err(reason) => {
                push_diag(
                    &mut pack.diagnostics.messages,
                    format!("git status unavailable: {reason}"),
                );
                return;
            }
        };

        let mut modified = Vec::new();
        let mut deleted = Vec::new();
        let mut untracked = Vec::new();
        let mut git_seen = HashSet::new();

        for candidate in git_candidates {
            let (path, target) = match candidate {
                GitStatusCandidate::Modified { path } => {
                    (normalize_display_path(path.as_str()), &mut modified)
                }
                GitStatusCandidate::Deleted { path } => {
                    (normalize_display_path(path.as_str()), &mut deleted)
                }
                GitStatusCandidate::Untracked { path } => {
                    (normalize_display_path(path.as_str()), &mut untracked)
                }
            };

            if seen.contains(&path) || !git_seen.insert(path.clone()) {
                continue;
            }

            target.push(path);
        }

        deleted.sort();
        for path in deleted {
            push_diag(
                &mut pack.diagnostics.messages,
                ContextCandidateSource::GitModified.omitted_message(path.as_str(), "deleted file"),
            );
        }

        untracked.sort();
        for path in untracked {
            push_diag(
                &mut pack.diagnostics.messages,
                ContextCandidateSource::GitModified
                    .omitted_message(path.as_str(), "untracked file"),
            );
        }

        modified.sort();
        for (index, path) in modified.iter().enumerate() {
            if index >= MAX_GIT_CONTEXT_FILES {
                push_diag(
                    &mut pack.diagnostics.messages,
                    ContextCandidateSource::GitModified
                        .omitted_message(path.as_str(), "max git context files exceeded"),
                );
                continue;
            }

            self.pack_file_candidate(
                path.as_str(),
                ContextCandidateSource::GitModified,
                pack,
                seen,
                remaining,
            );
        }
    }

    fn resolve_path(&self, requested_path: &str) -> PathResolution {
        let requested = Path::new(requested_path);
        if requested.as_os_str().is_empty() {
            return PathResolution::Missing {
                display_path: requested_path.to_string(),
            };
        }

        if is_path_traversal(requested) {
            return PathResolution::OutsideRepo {
                display_path: format!("{requested_path} (path traversal)"),
            };
        }

        if requested.is_absolute() {
            return self.resolve_absolute_path(requested);
        }

        if is_denied_path(requested) {
            return PathResolution::Denied {
                display_path: requested_path.to_string(),
            };
        }

        let mut candidates = vec![self.cwd.join(requested)];
        let repo_relative = self.repo_root.join(requested);
        if repo_relative != candidates[0] {
            candidates.push(repo_relative);
        }

        for candidate in &candidates {
            if !self.is_inside_repo_lexically(candidate) {
                continue;
            }
            if !candidate.exists() {
                continue;
            }

            let display_path = self.display_path(candidate);
            if is_denied_path(Path::new(&display_path)) {
                return PathResolution::Denied { display_path };
            }

            let canonical = dunce::canonicalize(candidate).unwrap_or_else(|_| candidate.clone());
            if !canonical.starts_with(&self.repo_root_canonical) {
                return PathResolution::OutsideRepo { display_path };
            }

            return PathResolution::Resolved {
                absolute_path: candidate.clone(),
                display_path,
            };
        }

        if candidates
            .iter()
            .any(|candidate| self.is_inside_repo_lexically(candidate))
        {
            return PathResolution::Missing {
                display_path: requested_path.to_string(),
            };
        }

        PathResolution::OutsideRepo {
            display_path: requested_path.to_string(),
        }
    }

    fn resolve_absolute_path(&self, requested: &Path) -> PathResolution {
        let display_path = self.display_path(requested);
        if is_denied_path(requested) || is_denied_path(Path::new(&display_path)) {
            return PathResolution::Denied { display_path };
        }

        if !self.is_inside_repo_lexically(requested) {
            return PathResolution::OutsideRepo { display_path };
        }

        if !requested.exists() {
            return PathResolution::Missing { display_path };
        }

        let canonical = dunce::canonicalize(requested).unwrap_or_else(|_| requested.to_path_buf());
        if !canonical.starts_with(&self.repo_root_canonical) {
            return PathResolution::OutsideRepo { display_path };
        }

        PathResolution::Resolved {
            absolute_path: requested.to_path_buf(),
            display_path,
        }
    }

    fn display_path(&self, path: &Path) -> String {
        let preferred = path
            .strip_prefix(&self.repo_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf());
        normalize_display_path(preferred.to_string_lossy().as_ref())
    }

    fn is_inside_repo_lexically(&self, path: &Path) -> bool {
        dunce::simplified(path).starts_with(&self.repo_root)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathResolution {
    Resolved {
        absolute_path: PathBuf,
        display_path: String,
    },
    Denied {
        display_path: String,
    },
    OutsideRepo {
        display_path: String,
    },
    Missing {
        display_path: String,
    },
}

impl PathResolution {
    fn display_path(&self) -> Option<&str> {
        match self {
            Self::Resolved { display_path, .. }
            | Self::Denied { display_path }
            | Self::OutsideRepo { display_path }
            | Self::Missing { display_path } => Some(display_path),
        }
    }
}

fn add_context_entry(
    pack: &mut ContextPack,
    remaining: &mut usize,
    path: String,
    content: String,
    per_file_budget_bytes: usize,
    source: ContextCandidateSource,
) -> bool {
    if *remaining == 0 {
        mark_total_budget_exhausted(pack);
        return false;
    }

    let (redacted_content, redacted) = redact_secrets(content.as_str());
    if redacted {
        push_diag(
            &mut pack.diagnostics.messages,
            format!("redacted potential secret in {path}"),
        );
    }

    let per_file_content = codex_utils_string::take_bytes_at_char_boundary(
        redacted_content.as_str(),
        per_file_budget_bytes,
    );
    let mut truncated = per_file_content.len() < redacted_content.len();
    if truncated {
        push_diag(
            &mut pack.diagnostics.messages,
            format!("context file {path} truncated at {per_file_budget_bytes} bytes"),
        );
        pack.truncated = true;
    }

    let total_content =
        codex_utils_string::take_bytes_at_char_boundary(per_file_content, *remaining);
    if total_content.len() < per_file_content.len() {
        truncated = true;
        mark_total_budget_exhausted(pack);
    }

    let consumed = total_content.len();
    *remaining = remaining.saturating_sub(consumed);
    if consumed == 0 {
        return false;
    }

    pack.files.push(PackedContextFile {
        path,
        content: total_content.to_string(),
        truncated,
        redacted,
        source: source.context_file_source(),
    });
    true
}

fn mentioned_task_paths(task_text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    for raw in task_text.split_whitespace() {
        let token = raw
            .trim_matches(|ch| {
                matches!(
                    ch,
                    '`' | '"'
                        | '\''
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | ','
                        | ':'
                        | ';'
                        | '<'
                        | '>'
                )
            })
            .trim_end_matches('?')
            .trim_end_matches('!')
            .trim_end_matches('.');
        if token.is_empty() || token.contains("://") {
            continue;
        }
        if !looks_like_path(token) {
            continue;
        }

        let normalized = normalize_display_path(token);
        if seen.insert(normalized.clone()) {
            paths.push(normalized);
        }
    }

    paths
}

fn looks_like_path(token: &str) -> bool {
    static PATH_RE: OnceLock<Regex> = OnceLock::new();
    PATH_RE
        .get_or_init(|| compile_regex(r"^\.?/?[A-Za-z0-9._-]+(?:/[A-Za-z0-9._-]+)+$"))
        .is_match(token)
}

fn is_path_traversal(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn is_denied_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(value) => {
            let component = value.to_string_lossy().to_lowercase();
            matches!(
                component.as_str(),
                ".git" | "target" | "node_modules" | "vendor"
            ) || component == ".env"
                || component.starts_with(".env.")
                || component == "auth.json"
                || component.contains("credentials")
                || component == "id_rsa"
                || component == "id_ed25519"
                || component.ends_with(".pem")
                || component.ends_with(".key")
                || component.contains("private_key")
                || component.contains("private-key")
        }
        _ => false,
    })
}

fn redact_secrets(content: &str) -> (String, bool) {
    let mut redacted = false;
    let mut updated = content.to_string();

    for (regex, replacement) in redaction_patterns() {
        let next = regex
            .replace_all(updated.as_str(), *replacement)
            .into_owned();
        if next != updated {
            redacted = true;
            updated = next;
        }
    }

    (updated, redacted)
}

fn redaction_patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            (
                compile_regex(r"(Authorization:\s*Bearer\s+)[A-Za-z0-9._-]+"),
                "${1}[REDACTED]",
            ),
            (
                compile_regex(r#"\b(MINIMAX_API_KEY|OPENAI_API_KEY)\s*=\s*[^\s"']+"#),
                "$1=[REDACTED]",
            ),
            (
                compile_regex(
                    r#"(\bapi[_-]?key\b\s*[:=]\s*)(?:"[^"]{8,}"|'[^']{8,}'|[A-Za-z0-9._-]{8,})"#,
                ),
                "${1}[REDACTED]",
            ),
            (
                compile_regex(r"\bsk-[A-Za-z0-9_-]{8,}\b"),
                "[REDACTED]",
            ),
            (
                compile_regex(
                    r#"(\b(?:token|secret|access_token|refresh_token)\b\s*[:=]\s*)(?:"[^"]{8,}"|'[^']{8,}'|[A-Za-z0-9._-]{16,})"#,
                ),
                "${1}[REDACTED]",
            ),
        ]
    })
}

fn compile_regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => panic!("invalid context packer regex `{pattern}`: {err}"),
    }
}

fn normalize_display_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn push_diag(diagnostics: &mut Vec<String>, message: String) {
    if !diagnostics.iter().any(|existing| existing == &message) {
        diagnostics.push(message);
    }
}

fn mark_total_budget_exhausted(pack: &mut ContextPack) {
    pack.truncated = true;
    push_diag(
        &mut pack.diagnostics.messages,
        "context truncated: exceeded total budget".to_string(),
    );
}

#[cfg(test)]
mod tests {
    use super::ContextPackRequest;
    use super::ContextPacker;
    use super::DEFAULT_CONTEXT_FILE_MAX_BYTES;
    use super::ExplicitContextSnippet;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn seed_repo() -> TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(repo.path().join(".git")).expect("create git dir");
        repo
    }

    fn seed_git_repo() -> TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test User"]);
        repo
    }

    fn run_git(repo: &TempDir, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} should succeed");
    }

    fn commit_all(repo: &TempDir, message: &str) {
        run_git(repo, &["add", "."]);
        run_git(repo, &["commit", "-qm", message]);
    }

    fn packer(repo: &TempDir) -> ContextPacker<'_> {
        ContextPacker::new(repo.path(), 256, 128)
    }

    fn packer_with_cwd(cwd: &Path) -> ContextPacker<'_> {
        ContextPacker::new(cwd, 256, 128)
    }

    #[test]
    fn includes_explicit_valid_file() {
        let repo = seed_repo();
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(repo.path().join("src/lib.rs"), "pub fn add() {}\n").expect("write source");

        let pack = packer(&repo).pack(&["src/lib.rs".to_string()], &[], "");

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/lib.rs");
        assert_eq!(pack.files[0].content, "pub fn add() {}\n");
        assert_eq!(pack.diagnostics.messages, Vec::<String>::new());
    }

    #[test]
    fn includes_path_mentioned_in_task() {
        let repo = seed_repo();
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(repo.path().join("src/lib.rs"), "pub fn add() {}\n").expect("write source");

        let pack = packer(&repo).pack(&[], &[], "Please update `src/lib.rs` only.");

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/lib.rs");
    }

    #[test]
    fn resolves_repo_root_relative_path_from_nested_cwd() {
        let repo = seed_repo();
        let nested = repo.path().join("packages/app");
        fs::create_dir_all(&nested).expect("create nested cwd");
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(repo.path().join("src/lib.rs"), "pub fn add() {}\n").expect("write source");

        let pack = packer_with_cwd(&nested).pack(&["src/lib.rs".to_string()], &[], "");

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/lib.rs");
    }

    #[test]
    fn omits_file_outside_repo() {
        let repo = seed_repo();
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").expect("write outside file");

        let pack = packer(&repo).pack(&[outside_file.to_string_lossy().to_string()], &[], "");

        assert_eq!(pack.files, Vec::new());
        assert_eq!(
            pack.diagnostics.messages,
            vec![format!(
                "omitted {}: outside repo",
                outside_file.to_string_lossy()
            )]
        );
    }

    #[test]
    fn omits_path_traversal() {
        let repo = seed_repo();

        let pack = packer(&repo).pack(&["../../secret.txt".to_string()], &[], "");

        assert_eq!(pack.files, Vec::new());
        assert_eq!(
            pack.diagnostics.messages,
            vec!["omitted ../../secret.txt (path traversal): outside repo".to_string()]
        );
    }

    #[test]
    fn omits_denied_paths() {
        let repo = seed_repo();
        fs::write(repo.path().join(".env"), "OPENAI_API_KEY=abc").expect("write env");
        fs::write(repo.path().join("auth.json"), "{}").expect("write auth");
        fs::write(repo.path().join(".git").join("config"), "[core]").expect("write git config");

        let pack = packer(&repo).pack(
            &[
                ".env".to_string(),
                "auth.json".to_string(),
                ".git/config".to_string(),
            ],
            &[],
            "",
        );

        assert_eq!(pack.files, Vec::new());
        assert_eq!(
            pack.diagnostics.messages,
            vec![
                "omitted .env: denied path".to_string(),
                "omitted auth.json: denied path".to_string(),
                "omitted .git/config: denied path".to_string(),
            ]
        );
    }

    #[test]
    fn omits_binary_or_non_utf8_files() {
        let repo = seed_repo();
        fs::write(repo.path().join("blob.bin"), vec![0, 159, 146, 150]).expect("write blob");

        let pack = packer(&repo).pack(&["blob.bin".to_string()], &[], "");

        assert_eq!(pack.files, Vec::new());
        assert_eq!(
            pack.diagnostics.messages,
            vec!["omitted blob.bin: binary/non-utf8".to_string()]
        );
    }

    #[test]
    fn truncates_large_file() {
        let repo = seed_repo();
        fs::write(repo.path().join("big.txt"), "x".repeat(300)).expect("write big file");

        let pack = packer(&repo).pack(&["big.txt".to_string()], &[], "");

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "big.txt");
        assert_eq!(pack.files[0].content.len(), 128);
        assert!(pack.files[0].truncated);
        assert_eq!(
            pack.diagnostics.messages,
            vec!["context file big.txt truncated at 128 bytes".to_string()]
        );
    }

    #[test]
    fn respects_total_budget() {
        let repo = seed_repo();
        fs::write(repo.path().join("a.txt"), "a".repeat(120)).expect("write file a");
        fs::write(repo.path().join("b.txt"), "b".repeat(120)).expect("write file b");
        fs::write(repo.path().join("c.txt"), "c".repeat(120)).expect("write file c");

        let pack = packer(&repo).pack(
            &[
                "a.txt".to_string(),
                "b.txt".to_string(),
                "c.txt".to_string(),
            ],
            &[],
            "",
        );

        assert_eq!(pack.files.len(), 3);
        assert_eq!(pack.files[0].content.len(), 120);
        assert_eq!(pack.files[1].content.len(), 120);
        assert_eq!(pack.files[2].content.len(), 16);
        assert!(pack.files[2].truncated);
        assert_eq!(
            pack.diagnostics.messages,
            vec!["context truncated: exceeded total budget".to_string()]
        );
    }

    #[test]
    fn deduplicates_repeated_files() {
        let repo = seed_repo();
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(repo.path().join("src/lib.rs"), "pub fn add() {}\n").expect("write source");

        let pack = packer(&repo).pack(
            &["src/lib.rs".to_string(), "src/lib.rs".to_string()],
            &[],
            "Change src/lib.rs and keep src/lib.rs compiling.",
        );

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/lib.rs");
    }

    #[test]
    fn redacts_obvious_secrets() {
        let repo = seed_repo();
        fs::write(
            repo.path().join("config.txt"),
            "OPENAI_API_KEY=sk-secret-12345678\nAuthorization: Bearer tokenvalue\napi_key = \"abcdefgh12345678\"\n",
        )
        .expect("write config");

        let pack = packer(&repo).pack(&["config.txt".to_string()], &[], "");

        assert_eq!(pack.files.len(), 1);
        assert!(!pack.files[0].content.contains("sk-secret-12345678"));
        assert!(!pack.files[0].content.contains("tokenvalue"));
        assert!(pack.files[0].content.contains("[REDACTED]"));
        assert_eq!(
            pack.diagnostics.messages,
            vec!["redacted potential secret in config.txt".to_string()]
        );
    }

    #[test]
    fn produces_file_missing_diagnostics() {
        let repo = seed_repo();

        let pack = packer(&repo).pack(&["missing.rs".to_string()], &[], "");

        assert_eq!(pack.files, Vec::new());
        assert_eq!(
            pack.diagnostics.messages,
            vec!["omitted missing.rs: file missing".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn omits_symlink_that_resolves_outside_repo() {
        use std::os::unix::fs::symlink;

        let repo = seed_repo();
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").expect("write outside file");
        symlink(&outside_file, repo.path().join("linked.txt")).expect("create symlink");

        let pack = packer(&repo).pack(&["linked.txt".to_string()], &[], "");

        assert_eq!(pack.files, Vec::new());
        assert_eq!(
            pack.diagnostics.messages,
            vec!["omitted linked.txt: outside repo".to_string()]
        );
    }

    #[test]
    fn explicit_snippets_are_prioritized_and_budgeted() {
        let repo = seed_repo();
        let snippets = vec![ExplicitContextSnippet {
            path: "src/lib.rs".to_string(),
            content: "sk-secret-abcdefghi".to_string(),
        }];

        let pack = ContextPacker::new(repo.path(), DEFAULT_CONTEXT_FILE_MAX_BYTES, 8).pack(
            &[],
            &snippets,
            "",
        );

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/lib.rs");
        assert_eq!(pack.files[0].content, "[REDACTE");
        assert!(pack.files[0].truncated);
        assert_eq!(
            pack.diagnostics.messages,
            vec![
                "redacted potential secret in src/lib.rs".to_string(),
                "context file src/lib.rs truncated at 8 bytes".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_git_modified_files_when_disabled() {
        let repo = seed_git_repo();
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(
            repo.path().join("src/lib.rs"),
            "pub fn add() -> i32 {\n    1\n}\n",
        )
        .expect("write initial source");
        commit_all(&repo, "init");
        fs::write(
            repo.path().join("src/lib.rs"),
            "pub fn add() -> i32 {\n    2\n}\n",
        )
        .expect("write modified source");

        let pack = packer(&repo).pack(&[], &[], "Adjust the helper.");

        assert_eq!(pack.files, Vec::new());
        assert_eq!(pack.git_modified_paths, Vec::<String>::new());
        assert_eq!(pack.diagnostics.messages, Vec::<String>::new());
    }

    #[test]
    fn includes_git_modified_file_when_enabled() {
        let repo = seed_git_repo();
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(
            repo.path().join("src/lib.rs"),
            "pub fn add() -> i32 {\n    1\n}\n",
        )
        .expect("write initial source");
        commit_all(&repo, "init");
        fs::write(
            repo.path().join("src/lib.rs"),
            "pub fn add() -> i32 {\n    2\n}\n",
        )
        .expect("write modified source");

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &[],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/lib.rs");
        assert_eq!(pack.files[0].content, "pub fn add() -> i32 {\n    2\n}\n");
        assert_eq!(pack.git_modified_paths, vec!["src/lib.rs".to_string()]);
        assert_eq!(pack.diagnostics.messages, Vec::<String>::new());
    }

    #[test]
    fn includes_staged_added_git_modified_file_when_enabled() {
        let repo = seed_git_repo();
        fs::write(repo.path().join("added.txt"), "hello\n").expect("write added file");
        run_git(&repo, &["add", "added.txt"]);

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &[],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "added.txt");
        assert_eq!(pack.files[0].content, "hello\n");
        assert_eq!(pack.git_modified_paths, vec!["added.txt".to_string()]);
        assert_eq!(pack.diagnostics.messages, Vec::<String>::new());
    }

    #[test]
    fn includes_git_renamed_file_using_new_path() {
        let repo = seed_git_repo();
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(
            repo.path().join("src/old.rs"),
            "pub fn add() -> i32 {\n    1\n}\n",
        )
        .expect("write initial source");
        commit_all(&repo, "init");
        run_git(&repo, &["mv", "src/old.rs", "src/new.rs"]);

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &[],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/new.rs");
        assert_eq!(pack.files[0].content, "pub fn add() -> i32 {\n    1\n}\n");
        assert_eq!(pack.git_modified_paths, vec!["src/new.rs".to_string()]);
        assert!(
            pack.diagnostics
                .messages
                .iter()
                .all(|message| !message.contains("old.rs"))
        );
    }

    #[test]
    fn keeps_explicit_priority_when_git_reports_the_same_file() {
        let repo = seed_git_repo();
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(
            repo.path().join("src/lib.rs"),
            "pub fn add() -> i32 {\n    1\n}\n",
        )
        .expect("write initial source");
        commit_all(&repo, "init");
        fs::write(
            repo.path().join("src/lib.rs"),
            "pub fn add() -> i32 {\n    2\n}\n",
        )
        .expect("write modified source");

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &["src/lib.rs".to_string()],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/lib.rs");
        assert_eq!(pack.git_modified_paths, Vec::<String>::new());
        assert_eq!(pack.diagnostics.messages, Vec::<String>::new());
    }

    #[test]
    fn omits_denied_git_modified_paths() {
        let repo = seed_git_repo();
        fs::write(repo.path().join(".env"), "OPENAI_API_KEY=sk-test-secret\n")
            .expect("write initial env");
        commit_all(&repo, "init");
        fs::write(repo.path().join(".env"), "OPENAI_API_KEY=sk-other-secret\n")
            .expect("write modified env");

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &[],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files, Vec::new());
        assert_eq!(pack.git_modified_paths, Vec::<String>::new());
        assert_eq!(
            pack.diagnostics.messages,
            vec!["omitted git modified file .env: denied path".to_string()]
        );
    }

    #[test]
    fn omits_deleted_git_modified_paths() {
        let repo = seed_git_repo();
        fs::write(repo.path().join("obsolete.txt"), "hello\n").expect("write initial file");
        commit_all(&repo, "init");
        fs::remove_file(repo.path().join("obsolete.txt")).expect("remove file");

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &[],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files, Vec::new());
        assert_eq!(pack.git_modified_paths, Vec::<String>::new());
        assert_eq!(
            pack.diagnostics.messages,
            vec!["omitted git modified file obsolete.txt: deleted file".to_string()]
        );
    }

    #[test]
    fn omits_untracked_git_modified_paths_by_default() {
        let repo = seed_git_repo();
        fs::write(repo.path().join("draft.txt"), "hello\n").expect("write draft file");

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &[],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files, Vec::new());
        assert_eq!(pack.git_modified_paths, Vec::<String>::new());
        assert_eq!(
            pack.diagnostics.messages,
            vec!["omitted git modified file draft.txt: untracked file".to_string()]
        );
    }

    #[test]
    fn explicit_untracked_file_is_still_included_when_requested() {
        let repo = seed_git_repo();
        fs::write(repo.path().join("draft.txt"), "hello\n").expect("write draft file");

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &["draft.txt".to_string()],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "draft.txt");
        assert_eq!(pack.files[0].content, "hello\n");
        assert_eq!(pack.git_modified_paths, Vec::<String>::new());
        assert_eq!(pack.diagnostics.messages, Vec::<String>::new());
    }

    #[test]
    fn git_modified_files_respect_max_and_budget_limits() {
        let repo = seed_git_repo();
        for name in [
            "a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "f.txt", "g.txt",
        ] {
            fs::write(repo.path().join(name), "seed\n").expect("write initial file");
        }
        commit_all(&repo, "init");
        for name in [
            "a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "f.txt", "g.txt",
        ] {
            fs::write(repo.path().join(name), "x".repeat(200)).expect("write modified file");
        }

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &[],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files.len(), 2);
        assert_eq!(pack.files[0].path, "a.txt");
        assert_eq!(pack.files[0].content.len(), 128);
        assert!(pack.files[0].truncated);
        assert_eq!(pack.files[1].path, "b.txt");
        assert_eq!(pack.files[1].content.len(), 128);
        assert!(pack.files[1].truncated);
        assert_eq!(
            pack.git_modified_paths,
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
        assert_eq!(
            pack.diagnostics.messages,
            vec![
                "context file a.txt truncated at 128 bytes".to_string(),
                "context file b.txt truncated at 128 bytes".to_string(),
                "context truncated: exceeded total budget".to_string(),
                "omitted git modified file c.txt: budget exhausted".to_string(),
                "omitted git modified file d.txt: budget exhausted".to_string(),
                "omitted git modified file e.txt: budget exhausted".to_string(),
                "omitted git modified file f.txt: max git context files exceeded".to_string(),
                "omitted git modified file g.txt: max git context files exceeded".to_string(),
            ]
        );
    }

    #[test]
    fn include_modified_files_outside_git_repo_keeps_explicit_context() {
        let repo = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(repo.path().join("src/lib.rs"), "pub fn add() {}\n").expect("write source");

        let pack = packer_with_cwd(repo.path()).pack_with_request(ContextPackRequest {
            explicit_files: &["src/lib.rs".to_string()],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/lib.rs");
        assert_eq!(pack.git_modified_paths, Vec::<String>::new());
        assert_eq!(pack.diagnostics.messages.len(), 1);
        assert!(pack.diagnostics.messages[0].starts_with("git status unavailable: "));
    }

    #[test]
    fn git_status_failure_keeps_explicit_context() {
        let repo = seed_repo();
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(repo.path().join("src/lib.rs"), "pub fn add() {}\n").expect("write source");

        let pack = packer(&repo).pack_with_request(ContextPackRequest {
            explicit_files: &["src/lib.rs".to_string()],
            explicit_snippets: &[],
            task_text: "Adjust the helper.",
            include_modified_files: true,
        });

        assert_eq!(pack.files.len(), 1);
        assert_eq!(pack.files[0].path, "src/lib.rs");
        assert_eq!(pack.git_modified_paths, Vec::<String>::new());
        assert_eq!(pack.diagnostics.messages.len(), 1);
        assert!(pack.diagnostics.messages[0].starts_with("git status unavailable: "));
    }
}
