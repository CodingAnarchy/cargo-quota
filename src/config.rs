//! Settings resolution. Precedence, highest first:
//!
//! 1. command-line flags
//! 2. `[package.metadata.quota]` in the root package's Cargo.toml
//! 3. `[workspace.metadata.quota]` in the workspace Cargo.toml
//! 4. `CARGO_QUOTA_*` environment variables (handy as a user-wide default)
//! 5. built-in defaults
//!
//! Project config deliberately outranks the environment so that a global
//! default in your shell profile doesn't override a project's own budget.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde_json::Value;

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
    /// Where final artifacts go (`target/`).
    pub target_dir: PathBuf,
    /// Where intermediate artifacts go. Same as `target_dir` unless `build.build-dir` is set.
    pub build_dir: PathBuf,
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
    let (target_dir, build_dir, file) = match (metadata, target_override) {
        (Some(meta), _) => {
            let target = PathBuf::from(
                meta["target_directory"]
                    .as_str()
                    .context("cargo metadata: no target_directory")?,
            );
            let build = meta["build_directory"]
                .as_str()
                .map_or_else(|| target.clone(), PathBuf::from);
            (target, build, FileConfig::from_metadata(&meta))
        }
        (None, Some(dir)) => (dir.clone(), dir, FileConfig::default()),
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

    Ok(Some(Settings {
        percent,
        basis,
        low_water,
        min_age,
        target_dir,
        build_dir,
    }))
}

#[derive(Default)]
struct FileConfig {
    percent: Option<f64>,
    basis: Option<Basis>,
    low_water: Option<f64>,
    min_age: Option<Duration>,
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
fn flag_value(args: &[OsString], flag: &str) -> Option<PathBuf> {
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
            "metadata": { "quota": { "percent": 3, "min-age": "2h" } },
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
    }
}
