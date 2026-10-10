//! Settings resolution. Precedence, highest first:
//!
//! 1. command-line flags
//! 2. `[package.metadata.quota]` in the root package's Cargo.toml
//! 3. `[workspace.metadata.quota]` in the workspace Cargo.toml
//! 4. `CARGO_QUOTA_*` environment variables (handy as a user-wide default)
//! 5. built-in defaults
//!
//! The `--worktrees` flag can only turn worktree sharing on; set
//! `worktrees = false` in Cargo.toml to override an environment default.
//!
//! Project config deliberately outranks the environment so that a global
//! default in your shell profile doesn't override a project's own budget.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde_json::Value;

use crate::worktree;

const DEFAULT_PERCENT: f64 = 5.0;
const DEFAULT_LOW_WATER: f64 = 0.8;
const DEFAULT_MIN_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(clap::Args, Debug, Default)]
pub struct Opts {
    /// Budget for the target directory, as a percentage of total disk space [default: 5]
    #[arg(long, short, global = true)]
    pub percent: Option<f64>,

    /// What "total disk space" is measured against [default: volume]
    #[arg(long, value_enum, global = true)]
    pub basis: Option<Basis>,

    /// When over budget, prune down to this fraction of it, so the next few
    /// builds don't each trigger another prune [default: 0.8]
    #[arg(long, global = true)]
    pub low_water: Option<f64>,

    /// Never remove artifacts used more recently than this, e.g. "30m", "2h", "1d" [default: 1h]
    #[arg(long, global = true)]
    pub min_age: Option<humantime::Duration>,

    /// Share the budget with this repository's other git worktrees, pruning
    /// the least recently used artifacts across all of their target dirs
    #[arg(long, global = true)]
    pub worktrees: bool,

    /// Report what would be removed without removing it
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// List each removed artifact
    #[arg(long, short, global = true)]
    pub verbose: bool,

    /// Print nothing except warnings and errors
    #[arg(long, short, global = true)]
    pub quiet: bool,

    /// Path to Cargo.toml
    #[arg(long, global = true)]
    pub manifest_path: Option<PathBuf>,

    /// Prune this target directory instead of the one cargo would use
    #[arg(long, global = true)]
    pub target_dir: Option<PathBuf>,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// Total size of the volume the target directory lives on
    Volume,
    /// Total size of all non-removable disks on the system
    System,
}

#[derive(Debug)]
pub struct Settings {
    pub percent: f64,
    pub basis: Basis,
    pub low_water: f64,
    pub min_age: Duration,
    /// The target dir cargo would use here. Disk capacity is measured on its volume.
    pub target_dir: PathBuf,
    /// Every directory that shares the budget: this project's target dir and
    /// build dir (`build.build-dir`, usually the same), plus those of other
    /// worktrees when `worktrees` is on. May overlap; the scanner dedups.
    pub dirs: Vec<PathBuf>,
    pub worktrees: bool,
}

/// Resolves settings for the project cargo would operate on. `cargo_args` are
/// the passthrough arguments, sniffed for `--manifest-path` and `--target-dir`.
///
/// Returns `None` when there's no project to prune (e.g. `cargo quota new foo`).
pub fn resolve(opts: &Opts, cargo_args: &[OsString]) -> Result<Option<Settings>> {
    let manifest_path = opts
        .manifest_path
        .clone()
        .or_else(|| flag_value(cargo_args, "--manifest-path"));
    let target_override = opts
        .target_dir
        .clone()
        .or_else(|| flag_value(cargo_args, "--target-dir"));

    let metadata = cargo_metadata(manifest_path.as_ref(), target_override.as_ref());
    let (target_dir, mut dirs, workspace_root, file) = match (metadata, target_override) {
        (Some(meta), _) => {
            let [target, build] =
                output_dirs(&meta).context("cargo metadata: no target_directory")?;
            let root = meta["workspace_root"].as_str().map(PathBuf::from);
            (
                target.clone(),
                vec![target, build],
                root,
                FileConfig::from_metadata(&meta),
            )
        }
        (None, Some(dir)) => (dir.clone(), vec![dir], None, FileConfig::default()),
        (None, None) => return Ok(None),
    };

    let percent = opts
        .percent
        .or(file.percent)
        .or(env_parse("CARGO_QUOTA_PERCENT")?)
        .unwrap_or(DEFAULT_PERCENT);
    ensure!(
        percent > 0.0 && percent <= 100.0,
        "percent must be in (0, 100], got {percent}"
    );

    let low_water = opts
        .low_water
        .or(file.low_water)
        .or(env_parse("CARGO_QUOTA_LOW_WATER")?)
        .unwrap_or(DEFAULT_LOW_WATER);
    ensure!(
        low_water > 0.0 && low_water <= 1.0,
        "low-water must be in (0, 1], got {low_water}"
    );

    let basis = match opts.basis.or(file.basis) {
        Some(b) => b,
        None => match env::var("CARGO_QUOTA_BASIS") {
            Ok(s) => parse_basis(&s)?,
            Err(_) => Basis::Volume,
        },
    };

    let min_age = match opts.min_age.map(Into::into).or(file.min_age) {
        Some(d) => d,
        None => match env::var("CARGO_QUOTA_MIN_AGE") {
            Ok(s) => {
                humantime::parse_duration(&s).with_context(|| format!("CARGO_QUOTA_MIN_AGE={s}"))?
            }
            Err(_) => DEFAULT_MIN_AGE,
        },
    };

    let worktrees = match opts.worktrees.then_some(true).or(file.worktrees) {
        Some(w) => w,
        None => match env::var("CARGO_QUOTA_WORKTREES") {
            Ok(s) => parse_bool(&s).with_context(|| format!("CARGO_QUOTA_WORKTREES={s}"))?,
            Err(_) => false,
        },
    };
    if worktrees && let Some(root) = &workspace_root {
        dirs.extend(worktree_dirs(root));
    }

    Ok(Some(Settings {
        percent,
        basis,
        low_water,
        min_age,
        target_dir,
        dirs,
        worktrees,
    }))
}

/// The target and build dirs from `cargo metadata` output.
fn output_dirs(meta: &Value) -> Option<[PathBuf; 2]> {
    let target = PathBuf::from(meta["target_directory"].as_str()?);
    let build = meta["build_directory"]
        .as_str()
        .map_or_else(|| target.clone(), PathBuf::from);
    Some([target, build])
}

/// Target and build dirs of the same workspace in every other worktree of
/// `workspace_root`'s repository. Each worktree is asked via `cargo metadata`,
/// in parallel, so per-worktree `.cargo/config.toml` settings are honored.
fn worktree_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let Some(worktrees) = worktree::find(workspace_root) else {
        return Vec::new();
    };
    // The workspace may be a subdirectory of the repository.
    let relative = workspace_root
        .canonicalize()
        .ok()
        .and_then(|r| r.strip_prefix(&worktrees.current).ok().map(Path::to_owned))
        .unwrap_or_default();

    thread::scope(|s| {
        let lookups: Vec<_> = worktrees
            .others
            .iter()
            .map(|other| {
                let manifest = other.join(&relative).join("Cargo.toml");
                s.spawn(move || {
                    // The workspace may not exist on that worktree's branch.
                    if !manifest.is_file() {
                        return None;
                    }
                    output_dirs(&cargo_metadata(Some(&manifest), None)?)
                })
            })
            .collect();
        lookups
            .into_iter()
            .filter_map(|l| l.join().ok().flatten())
            .flatten()
            .collect()
    })
}

#[derive(Default)]
struct FileConfig {
    percent: Option<f64>,
    basis: Option<Basis>,
    low_water: Option<f64>,
    min_age: Option<Duration>,
    worktrees: Option<bool>,
}

impl FileConfig {
    fn from_metadata(meta: &Value) -> Self {
        let workspace_root = meta["workspace_root"].as_str().map(PathBuf::from);
        let root_package = meta["packages"].as_array().into_iter().flatten().find(|p| {
            let manifest = p["manifest_path"].as_str().map(PathBuf::from);
            manifest.as_deref().and_then(|m| m.parent()) == workspace_root.as_deref()
        });

        let mut config = Self::default();
        config.merge(&meta["metadata"]["quota"], "[workspace.metadata.quota]");
        if let Some(pkg) = root_package {
            config.merge(&pkg["metadata"]["quota"], "[package.metadata.quota]");
        }
        config
    }

    fn merge(&mut self, table: &Value, origin: &str) {
        if table.is_null() {
            return;
        }
        let warn =
            |key: &str| eprintln!("warning: cargo-quota: ignoring invalid `{key}` in {origin}");
        if let Some(v) = table.get("percent") {
            match v.as_f64() {
                Some(p) => self.percent = Some(p),
                None => warn("percent"),
            }
        }
        if let Some(v) = table.get("low-water") {
            match v.as_f64() {
                Some(l) => self.low_water = Some(l),
                None => warn("low-water"),
            }
        }
        if let Some(v) = table.get("basis") {
            match v.as_str().map(parse_basis) {
                Some(Ok(b)) => self.basis = Some(b),
                _ => warn("basis"),
            }
        }
        if let Some(v) = table.get("min-age") {
            match v.as_str().map(humantime::parse_duration) {
                Some(Ok(d)) => self.min_age = Some(d),
                _ => warn("min-age"),
            }
        }
        if let Some(v) = table.get("worktrees") {
            match v.as_bool() {
                Some(w) => self.worktrees = Some(w),
                None => warn("worktrees"),
            }
        }
    }
}

fn cargo_metadata(manifest_path: Option<&PathBuf>, target_dir: Option<&PathBuf>) -> Option<Value> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(["metadata", "--format-version", "1", "--no-deps"]);
    if let Some(path) = manifest_path {
        cmd.arg("--manifest-path").arg(path);
    }
    if let Some(dir) = target_dir {
        // `cargo metadata` has no --target-dir flag, but honors the env var.
        cmd.env("CARGO_TARGET_DIR", dir);
    }
    let output = cmd.stderr(Stdio::null()).output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Finds `--flag value` or `--flag=value` in passthrough args, stopping at `--`.
pub fn flag_value(args: &[OsString], flag: &str) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let arg = arg.to_str()?;
        if arg == "--" {
            break;
        }
        if arg == flag {
            return iter.next().map(PathBuf::from);
        }
        if let Some(value) = arg
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(PathBuf::from(value));
        }
    }
    None
}

fn parse_basis(s: &str) -> Result<Basis> {
    match Basis::from_str(s, true) {
        Ok(b) => Ok(b),
        Err(_) => bail!("invalid basis {s:?} (expected \"volume\" or \"system\")"),
    }
}

fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("expected true or false"),
    }
}

fn env_parse(var: &str) -> Result<Option<f64>> {
    match env::var(var) {
        Ok(s) => Ok(Some(
            s.trim().parse().with_context(|| format!("{var}={s}"))?,
        )),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn sniffs_flags_in_both_forms() {
        let a = args(&["build", "--target-dir", "/tmp/t", "--release"]);
        assert_eq!(flag_value(&a, "--target-dir"), Some("/tmp/t".into()));
        let a = args(&["build", "--manifest-path=foo/Cargo.toml"]);
        assert_eq!(
            flag_value(&a, "--manifest-path"),
            Some("foo/Cargo.toml".into())
        );
    }

    #[test]
    fn ignores_flags_after_double_dash() {
        let a = args(&["run", "--", "--target-dir", "x"]);
        assert_eq!(flag_value(&a, "--target-dir"), None);
    }

    #[test]
    fn package_config_overrides_workspace_config() {
        let meta = serde_json::json!({
            "workspace_root": "/ws",
            "metadata": { "quota": { "percent": 3, "min-age": "2h", "worktrees": true } },
            "packages": [
                { "manifest_path": "/ws/member/Cargo.toml", "metadata": { "quota": { "percent": 50 } } },
                { "manifest_path": "/ws/Cargo.toml", "metadata": { "quota": { "percent": 7.5, "basis": "system" } } },
            ],
        });
        let config = FileConfig::from_metadata(&meta);
        assert_eq!(config.percent, Some(7.5));
        assert_eq!(config.basis, Some(Basis::System));
        assert_eq!(config.min_age, Some(Duration::from_secs(2 * 60 * 60)));
        assert_eq!(config.low_water, None);
        assert_eq!(config.worktrees, Some(true));
    }
}
