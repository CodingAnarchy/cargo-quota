//! End-to-end: build real crates, make some artifacts look stale, prune, and
//! check that cargo still builds afterward.

use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

/// A workspace-free app with a path dependency `oldlib`, so building needs no network.
fn create_project(dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("oldlib/src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\noldlib = { path = \"oldlib\" }\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/main.rs"),
        "fn main() { println!(\"{}\", oldlib::answer()); }\n",
    )
    .unwrap();
    fs::write(
        dir.join("oldlib/Cargo.toml"),
        "[package]\nname = \"oldlib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("oldlib/src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .unwrap();
}

fn cargo(dir: &Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO"))
        .args(args)
        .current_dir(dir)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cargo {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn quota(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-quota"))
        .args(args)
        .current_dir(dir)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO", env!("CARGO"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "cargo-quota {args:?} failed: {stderr}"
    );
    stderr
}

/// Backdates every file and directory under `root` whose path mentions `name`.
fn backdate(root: &Path, name: &str, age: Duration) {
    let when = SystemTime::now() - age;
    let times = FileTimes::new().set_accessed(when).set_modified(when);
    for path in walk(root) {
        if relative_mentions(root, &path, name) {
            open_for_times(&path).set_times(times).unwrap();
        }
    }
}

/// Opens a file or directory so its timestamps can be set.
fn open_for_times(path: &Path) -> File {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        // Required to open a directory handle at all.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap()
    }
    #[cfg(not(windows))]
    File::open(path).unwrap()
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            paths.extend(walk(&path));
        }
        paths.push(path);
    }
    paths
}

/// Matches below `root` only, so the random temp dir name can't match.
fn relative_mentions(root: &Path, path: &Path, name: &str) -> bool {
    path.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .contains(name)
}

fn mentions(root: &Path, name: &str) -> usize {
    walk(root)
        .iter()
        .filter(|p| relative_mentions(root, p, name))
        .count()
}

const LONG_AGO: Duration = Duration::from_secs(90 * 24 * 60 * 60);
/// Small enough that any real build is over budget.
const TINY_BUDGET: &str = "0.0000001";

#[test]
fn prunes_only_stale_units_and_cargo_rebuilds_them() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("app");
    create_project(&project);
    cargo(&project, &["build", "-q"]);

    let target = project.join("target");
    backdate(&target, "oldlib", LONG_AGO);
    assert!(mentions(&target.join("debug/deps"), "oldlib") > 0);

    let stderr = quota(&project, &["prune", "-p", TINY_BUDGET, "--min-age", "1h"]);
    assert!(stderr.contains("Removed"), "{stderr}");

    assert_eq!(
        mentions(&target.join("debug/deps"), "oldlib"),
        0,
        "stale unit left behind"
    );
    assert_eq!(
        mentions(&target.join("debug/.fingerprint"), "oldlib"),
        0,
        "stale fingerprint left behind"
    );
    assert!(
        mentions(&target.join("debug/.fingerprint"), "app-") > 0,
        "recent unit was removed"
    );

    // Cargo notices the missing unit and rebuilds it.
    let rebuild = cargo(&project, &["build"]);
    assert!(String::from_utf8_lossy(&rebuild.stderr).contains("Compiling oldlib"));
    let run = cargo(&project, &["run", "-q"]);
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "42");
}

#[test]
fn does_nothing_under_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("app");
    create_project(&project);
    cargo(&project, &["build", "-q"]);
    backdate(&project.join("target"), "oldlib", LONG_AGO);

    let stderr = quota(&project, &["prune", "-p", "100"]);
    assert!(stderr.contains("nothing to prune"), "{stderr}");
    assert!(mentions(&project.join("target/debug/deps"), "oldlib") > 0);
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn worktrees_share_a_budget_and_stale_worktrees_go_first() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let stale = tmp.path().join("stale");
    create_project(&main);
    fs::write(main.join(".gitignore"), "target/\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "init"]);
    git(&main, &["worktree", "add", "-q", stale.to_str().unwrap()]);

    cargo(&main, &["build", "-q"]);
    cargo(&stale, &["build", "-q"]);
    // Everything in the other worktree is stale, including its `app` units.
    backdate(&stale.join("target"), "", LONG_AGO);

    // Without --worktrees, the stale worktree's target dir isn't considered.
    quota(&main, &["prune", "-p", TINY_BUDGET]);
    assert!(mentions(&stale.join("target/debug/deps"), "app") > 0);

    let status = quota(&main, &["status", "--worktrees", "-p", TINY_BUDGET]);
    assert!(status.contains("2 target dirs total"), "{status}");

    quota(&main, &["prune", "--worktrees", "-p", TINY_BUDGET]);
    assert_eq!(mentions(&stale.join("target/debug/deps"), "app"), 0);
    assert!(
        mentions(&main.join("target/debug/deps"), "app") > 0,
        "recent worktree was pruned"
    );
}
