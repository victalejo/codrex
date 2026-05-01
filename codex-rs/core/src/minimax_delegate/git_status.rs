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
            let Some(_) = records.next() else {
                return Err("git status rename/copy entry was missing the source path".to_string());
            };
        }
    }

    Ok(candidates)
}
