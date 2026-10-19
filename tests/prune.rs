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
        // CI turns incremental compilation off; build like a dev machine
        // does. The variable outranks any cargo config that disables it.
        .env("CARGO_INCREMENTAL", "1")
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
    assert!(target.join(APP).is_file(), "recent binary was removed");

    // Cargo notices the missing unit and rebuilds it.
    let rebuild = cargo(&project, &["build"]);
    assert!(String::from_utf8_lossy(&rebuild.stderr).contains("Compiling oldlib"));
    let run = cargo(&project, &["run", "-q"]);
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "42");
}

/// The binary cargo copies up into the profile dir.
const APP: &str = if cfg!(windows) {
    "debug/app.exe"
} else {
    "debug/app"
};

#[test]
fn prunes_docs_and_final_binaries_too() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("app");
    create_project(&project);
    cargo(&project, &["build", "-q"]);
    cargo(&project, &["doc", "-q"]);
    let target = project.join("target");
    assert!(target.join(APP).is_file());
    assert!(target.join("doc").is_dir());
    backdate(&target, "", LONG_AGO);

    let stderr = quota(&project, &["prune", "-v", "-p", TINY_BUDGET]);
    assert!(
        !target.join(APP).exists(),
        "stale binary left behind\n{stderr}"
    );
    assert!(
        !target.join("doc").exists(),
        "stale docs left behind: {:#?}\n{stderr}",
        walk(&target.join("doc"))
    );

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

#[test]
fn builds_soon_after_a_comfortable_check_skip_measuring() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("app");
    create_project(&project);
    cargo(&project, &["build"]);

    let first = quota(&project, &["-v", "-p", "100", "build"]);
    assert!(first.contains("nothing to prune"), "{first}");
    let second = quota(&project, &["-v", "-p", "100", "build"]);
    assert!(second.contains("not checking again yet"), "{second}");

    // An over-budget setting can't be satisfied from a stamp that's above its low-water mark.
    let tight = quota(&project, &["-v", "-p", TINY_BUDGET, "build"]);
    assert!(tight.contains("Pruning"), "{tight}");
    let never = quota(
        &project,
        &["-v", "--check-interval", "0s", "-p", "100", "build"],
    );
    assert!(never.contains("nothing to prune"), "{never}");
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

    let stderr = quota(&main, &["prune", "--worktrees", "-v", "-p", TINY_BUDGET]);
    let left: Vec<PathBuf> = walk(&stale.join("target/debug/deps"))
        .into_iter()
        .filter(|p| relative_mentions(&stale.join("target/debug/deps"), p, "app"))
        .collect();
    assert!(
        left.is_empty(),
        "stale worktree units left behind: {left:#?}\n{stderr}"
    );
    assert!(
        mentions(&main.join("target/debug/deps"), "app") > 0,
        "recent worktree was pruned"
    );
}

/// With macOS's default `split-debuginfo = "unpacked"`, every compile of a
/// crate leaves a new set of codegen-unit objects in `deps/`.
#[cfg(target_os = "macos")]
#[test]
fn removes_objects_that_rebuilds_replaced() {
    use std::collections::HashSet;

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("app");
    create_project(&project);
    let objects = |unit: &str| -> Vec<PathBuf> {
        walk(&project.join("target/debug/deps"))
            .into_iter()
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with(unit) && name.ends_with(".rcgu.o")
            })
            .collect()
    };
    cargo(&project, &["build", "-q"]);
    let first = objects("app-");
    assert!(!first.is_empty(), "rustc kept no object files");
    let mut rebuilds = String::new();
    for n in [1, 2] {
        fs::write(
            project.join("src/main.rs"),
            format!("fn main() {{ println!(\"{{}}\", oldlib::answer() + {n} - {n}); }}\n"),
        )
        .unwrap();
        let build = cargo(&project, &["build", "-v"]);
        rebuilds += &String::from_utf8_lossy(&build.stderr);
    }
    assert!(
        objects("app-").len() > first.len(),
        "rebuilds added no objects: {:#?}\n{rebuilds}",
        objects("app-")
    );

    // Recently used and well under budget: only the replaced objects go.
    let stderr = quota(&project, &["prune", "-p", "100"]);
    assert!(stderr.contains("that newer builds"), "{stderr}");
    // `app-<hash>.<cgu>.<invocation>.rcgu.o`: one compile's worth remains.
    let left = objects("app-");
    let invocations: HashSet<String> = left
        .iter()
        .map(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .split('.')
                .nth(2)
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(invocations.len(), 1, "{left:#?}\n{stderr}");
    assert!(
        first.iter().all(|p| !p.exists()),
        "objects from the first build left behind"
    );
    assert!(
        !objects("oldlib-").is_empty(),
        "unchanged crate lost its objects"
    );

    // Cargo considers everything fresh, and the binary still runs.
    let rebuild = cargo(&project, &["build"]);
    assert!(!String::from_utf8_lossy(&rebuild.stderr).contains("Compiling"));
    let run = cargo(&project, &["run", "-q"]);
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "42");

    let again = quota(&project, &["prune", "-p", "100"]);
    assert!(!again.contains("object files"), "{again}");
}
