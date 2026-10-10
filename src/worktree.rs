//! Finds a repository's other git worktrees, so their target directories can
//! share one budget.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Worktrees {
    /// Root of the worktree containing the directory we were asked about.
    pub current: PathBuf,
    /// Roots of every other worktree that still exists on disk.
    pub others: Vec<PathBuf>,
}

/// Returns `None` when `dir` isn't in a git repository or git isn't installed.
pub fn find(dir: &Path) -> Option<Worktrees> {
    let current = canonical(Path::new(
        git(dir, &["rev-parse", "--show-toplevel"])?.trim(),
    ));
    let list = git(dir, &["worktree", "list", "--porcelain"])?;
    let others = parse_list(&list)
        .iter()
        .map(|p| canonical(p))
        .filter(|p| *p != current && p.is_dir())
        .collect();
    Some(Worktrees { current, others })
}

/// Extracts worktree paths from `git worktree list --porcelain`, skipping the
/// bare repository entry, which has no checkout.
fn parse_list(porcelain: &str) -> Vec<PathBuf> {
    porcelain
        .split("\n\n")
        .filter(|block| !block.lines().any(|l| l == "bare"))
        .filter_map(|block| block.lines().find_map(|l| l.strip_prefix("worktree ")))
        .map(PathBuf::from)
        .collect()
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_list() {
        let porcelain = "worktree /repo.git\nbare\n\n\
                         worktree /src/main\nHEAD abc\nbranch refs/heads/main\n\n\
                         worktree /src/feature\nHEAD def\nbranch refs/heads/feature\nprunable gitdir file points to non-existent location\n\n\
                         worktree /src/detached\nHEAD 123\ndetached\n";
        assert_eq!(
            parse_list(porcelain),
            [
                PathBuf::from("/src/main"),
                PathBuf::from("/src/feature"),
                PathBuf::from("/src/detached")
            ]
        );
    }
}
