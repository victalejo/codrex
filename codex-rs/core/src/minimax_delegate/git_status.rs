use std::path::Path;
use std::process::Command;

pub(crate) const MAX_GIT_CONTEXT_FILES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitStatusCandidate {
    Modified { path: String },
    Deleted { path: String },
    Untracked { path: String },
}

pub(crate) fn collect_git_status_candidates(
    repo_root: &Path,
) -> Result<Vec<GitStatusCandidate>, String> {
    // TODO: add a bounded timeout here once this sync helper can share a small process-timeout
    // utility without forcing ContextPacker into a larger async refactor.
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z"])
        .current_dir(repo_root)
        .output()
        .map_err(|err| err.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let reason = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("git exited with status {}", output.status)
        };
        return Err(reason);
    }

    parse_status_porcelain_v1_z(&output.stdout)
}

fn parse_status_porcelain_v1_z(output: &[u8]) -> Result<Vec<GitStatusCandidate>, String> {
    let mut records = output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    let mut candidates = Vec::new();

    while let Some(record) = records.next() {
        if record.len() < 3 || record[2] != b' ' {
            return Err(format!(
                "unparseable git status entry: {:?}",
                String::from_utf8_lossy(record)
            ));
        }

        let index_status = char::from(record[0]);
        let worktree_status = char::from(record[1]);
        let path = String::from_utf8(record[3..].to_vec())
            .map_err(|_| "git status returned a non-utf8 path".to_string())?;
        let path = path.replace('\\', "/");

        if index_status == '?' && worktree_status == '?' {
            candidates.push(GitStatusCandidate::Untracked { path });
            continue;
        }

        if index_status == '!' && worktree_status == '!' {
            continue;
        }

        if index_status == 'D' || worktree_status == 'D' {
            candidates.push(GitStatusCandidate::Deleted { path });
        } else {
            candidates.push(GitStatusCandidate::Modified { path });
        }

        if matches!(index_status, 'R' | 'C') || matches!(worktree_status, 'R' | 'C') {
            // In `git status --porcelain=v1 -z`, rename/copy entries report the destination path
            // in the current record and the source path in the following NUL-delimited record.
            let Some(_) = records.next() else {
                return Err("git status rename/copy entry was missing the source path".to_string());
            };
        }
    }

    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::GitStatusCandidate;
    use super::collect_git_status_candidates;
    use super::parse_status_porcelain_v1_z;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

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

    #[test]
    fn parses_staged_added_entry() {
        let candidates =
            parse_status_porcelain_v1_z(b"A  src/lib.rs\0").expect("staged add should parse");

        assert_eq!(
            candidates,
            vec![GitStatusCandidate::Modified {
                path: "src/lib.rs".to_string(),
            }]
        );
    }

    #[test]
    fn rename_porcelain_v1_z_reports_new_path_before_old_path() {
        let repo = seed_git_repo();
        fs::create_dir_all(repo.path().join("src")).expect("create src");
        fs::write(
            repo.path().join("src/old.rs"),
            "pub fn value() -> i32 { 1 }\n",
        )
        .expect("write old file");
        run_git(&repo, &["add", "src/old.rs"]);
        run_git(&repo, &["commit", "-qm", "init"]);
        run_git(&repo, &["mv", "src/old.rs", "src/new.rs"]);

        let output = Command::new("git")
            .args(["status", "--porcelain=v1", "-z"])
            .current_dir(repo.path())
            .output()
            .expect("collect git status");
        assert!(output.status.success(), "git status should succeed");
        assert_eq!(output.stdout, b"R  src/new.rs\0src/old.rs\0");

        let candidates =
            collect_git_status_candidates(repo.path()).expect("rename status should parse");
        assert_eq!(
            candidates,
            vec![GitStatusCandidate::Modified {
                path: "src/new.rs".to_string(),
            }]
        );
    }

    #[test]
    fn parses_copy_entry_fixture_using_destination_path() {
        // `git status --porcelain=v1 -z` does not reliably emit `C` with normal local status
        // flows, so cover copy handling with a raw porcelain fixture.
        let candidates = parse_status_porcelain_v1_z(b"C  src/copy.rs\0src/original.rs\0")
            .expect("copy entry should parse");

        assert_eq!(
            candidates,
            vec![GitStatusCandidate::Modified {
                path: "src/copy.rs".to_string(),
            }]
        );
    }
}
