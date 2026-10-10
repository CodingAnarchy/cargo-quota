use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Result, bail};

use crate::config::{self, Opts, Settings};
use crate::disk;
use crate::scan::{Scan, Unit};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `cargo quota prune`: prune and report.
    Prune,
    /// `cargo quota status`: report in detail, change nothing.
    Status,
    /// `cargo quota <cargo args>`: prune quietly unless something is removed.
    BeforeBuild,
}

pub fn run(opts: &Opts, cargo_args: &[OsString], mode: Mode) -> Result<()> {
    let Some(settings) = config::resolve(opts, cargo_args)? else {
        if mode == Mode::BeforeBuild {
            return Ok(());
        }
        bail!("no cargo project found here (try --manifest-path or --target-dir)");
    };
    let out = Output {
        quiet: opts.quiet
            || cargo_args
                .iter()
                .take_while(|a| *a != "--")
                .any(|a| a == "-q" || a == "--quiet"),
        verbose: opts.verbose,
        color: io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };
    let dry_run = opts.dry_run || mode == Mode::Status;

    let capacity = disk::total_space(&settings.target_dir, settings.basis)?;
    let limit = (capacity as f64 * settings.percent / 100.0) as u64;
    let goal = (limit as f64 * settings.low_water) as u64;
    let mut scan = Scan::measure(&settings.dirs);
    let over_budget = scan.total_bytes > limit;
    if over_budget || mode == Mode::Status {
        scan.find_units();
    }

    if mode == Mode::Status {
        print_status(&out, &settings, &scan, capacity, limit, goal);
    }
    if !over_budget {
        if mode != Mode::BeforeBuild || out.verbose {
            out.status(
                "Quota",
                &format!(
                    "{} of {} budget used; nothing to prune",
                    bytes(scan.total_bytes),
                    bytes(limit)
                ),
            );
        }
        return Ok(());
    }

    // Holding each profile's `.cargo-lock` keeps a concurrent cargo from
    // building into a directory we're deleting from, and vice versa. A dry run
    // takes no locks, so it never makes a running build wait.
    let locks: Vec<Option<File>> = if dry_run {
        scan.profiles.iter().map(|_| None).collect()
    } else {
        scan.profiles.iter().map(|p| try_lock(p)).collect()
    };
    let busy = |unit: &Unit| !dry_run && locks[unit.profile].is_none();
    for (profile, lock) in scan.profiles.iter().zip(&locks) {
        if !dry_run && lock.is_none() {
            out.warn(&format!(
                "{} is in use by another cargo process; skipping it",
                profile.display()
            ));
        }
    }

    let cutoff = SystemTime::now()
        .checked_sub(settings.min_age)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let plan = plan(&scan.units, scan.total_bytes, goal, cutoff, busy);

    let over = match scan.roots.as_slice() {
        [(root, _)] => format!(
            "{} is {}, over its {} budget",
            root.display(),
            bytes(scan.total_bytes),
            bytes(limit)
        ),
        roots => format!(
            "{} target dirs total {}, over their shared {} budget",
            roots.len(),
            bytes(scan.total_bytes),
            bytes(limit)
        ),
    };
    out.status(
        if dry_run { "Over quota" } else { "Pruning" },
        &format!("{over} ({}% of {})", settings.percent, bytes(capacity)),
    );

    let mut freed = 0;
    let mut removed = 0;
    for &i in &plan.remove {
        let unit = &scan.units[i];
        if out.verbose {
            let verb = if dry_run { "Would remove" } else { "Removing" };
            out.status(
                verb,
                &format!(
                    "{} ({}, last used {} ago)",
                    unit.label,
                    bytes(unit.bytes),
                    ago(unit.last_used)
                ),
            );
        }
        if dry_run || remove_all(&unit.paths, &out) {
            freed += unit.bytes;
            removed += 1;
        }
    }
    drop(locks);

    if removed > 0 {
        let newest = plan
            .remove
            .last()
            .map(|&i| scan.units[i].last_used)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.status(
            if dry_run { "Would remove" } else { "Removed" },
            &format!(
                "{removed} artifacts ({}) unused for {} or longer, leaving {}",
                bytes(freed),
                ago(newest),
                bytes(scan.total_bytes.saturating_sub(freed))
            ),
        );
    }

    let remaining = scan.total_bytes.saturating_sub(freed);
    if remaining > limit {
        let unmanaged = scan
            .total_bytes
            .saturating_sub(scan.units.iter().map(|u| u.bytes).sum());
        let reasons: Vec<String> = [
            (
                plan.recent_bytes,
                format!(
                    "used within the last {}",
                    humantime::format_duration(settings.min_age)
                ),
            ),
            (
                plan.busy_bytes,
                "in profiles another cargo process is using".to_owned(),
            ),
            (
                unmanaged,
                "outside build profiles (doc/, package/, final binaries, ...)".to_owned(),
            ),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, why)| format!("{} {why}", bytes(n)))
        .collect();
        out.warn(&format!(
            "still {} over budget; kept {}",
            bytes(remaining - limit),
            reasons.join(", ")
        ));
    }
    Ok(())
}

#[derive(Debug, Default)]
struct Plan {
    /// Indices into the unit list, least recently used first.
    remove: Vec<usize>,
    /// Bytes left in place because they were used after the cutoff.
    recent_bytes: u64,
    /// Bytes left in place because their profile is locked.
    busy_bytes: u64,
}

/// Picks units to remove, least recently used first, until `total` reaches
/// `goal`. Units used after `cutoff` are never picked.
fn plan(
    units: &[Unit],
    total: u64,
    goal: u64,
    cutoff: SystemTime,
    busy: impl Fn(&Unit) -> bool,
) -> Plan {
    let mut order: Vec<usize> = (0..units.len()).collect();
    order.sort_by_key(|&i| units[i].last_used);

    let mut plan = Plan::default();
    let mut remaining = total;
    for i in order {
        let unit = &units[i];
        if remaining <= goal {
            break;
        }
        if unit.last_used > cutoff {
            plan.recent_bytes += unit.bytes;
        } else if busy(unit) {
            plan.busy_bytes += unit.bytes;
        } else {
            remaining = remaining.saturating_sub(unit.bytes);
            plan.remove.push(i);
        }
    }
    plan
}

/// Takes cargo's per-profile build lock without waiting. `None` means another
/// cargo process holds it.
fn try_lock(profile: &Path) -> Option<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(profile.join(".cargo-lock"))
        .ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(TryLockError::WouldBlock) => None,
        // Filesystems without lock support: proceed, as cargo itself does.
        Err(TryLockError::Error(_)) => Some(file),
    }
}

/// Removes every path, returning whether all of them are gone.
fn remove_all(paths: &[PathBuf], out: &Output) -> bool {
    let mut ok = true;
    for path in paths {
        let result = match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
            Ok(_) => fs::remove_file(path),
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                out.warn(&format!("failed to remove {}: {e}", path.display()));
                ok = false;
            }
        }
    }
    ok
}

fn print_status(
    out: &Output,
    settings: &Settings,
    scan: &Scan,
    capacity: u64,
    limit: u64,
    goal: u64,
) {
    match scan.roots.as_slice() {
        [] => out.status(
            "Target dir",
            &format!("{} (not built yet)", settings.target_dir.display()),
        ),
        [(root, _)] => out.status("Target dir", &root.display().to_string()),
        roots => {
            for (root, size) in roots {
                out.status(
                    "Target dir",
                    &format!("{} ({})", root.display(), bytes(*size)),
                );
            }
        }
    }
    if settings.worktrees && scan.roots.len() <= 1 {
        out.status("Worktrees", "no other worktrees have their own target dir");
    }
    let oldest = scan.units.iter().map(|u| u.last_used).min();
    out.status(
        "Size",
        &format!(
            "{} in {} artifacts across {} profile dir(s){}",
            bytes(scan.total_bytes),
            scan.units.len(),
            scan.profiles.len(),
            oldest
                .map(|t| format!(", oldest unused for {}", ago(t)))
                .unwrap_or_default()
        ),
    );
    let basis = match settings.basis {
        config::Basis::Volume => "this volume",
        config::Basis::System => "all disks",
    };
    out.status(
        "Budget",
        &format!(
            "{} ({}% of {basis}, {}); prunes down to {}, keeping anything used within {}",
            bytes(limit),
            settings.percent,
            bytes(capacity),
            bytes(goal),
            humantime::format_duration(settings.min_age)
        ),
    );
}

struct Output {
    quiet: bool,
    verbose: bool,
    color: bool,
}

impl Output {
    /// Prints a line styled like cargo's own status output.
    fn status(&self, verb: &str, message: &str) {
        if self.quiet {
            return;
        }
        if self.color {
            eprintln!("\x1b[1;32m{verb:>12}\x1b[0m {message}");
        } else {
            eprintln!("{verb:>12} {message}");
        }
    }

    fn warn(&self, message: &str) {
        if self.color {
            eprintln!("\x1b[1;33mwarning\x1b[0m: cargo-quota: {message}");
        } else {
            eprintln!("warning: cargo-quota: {message}");
        }
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn ago(time: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(time)
        .unwrap_or_default()
        .as_secs();
    match secs {
        s if s < 60 * 60 => format!("{}m", s / 60),
        s if s < 2 * 24 * 60 * 60 => format!("{}h", s / (60 * 60)),
        s => format!("{}d", s / (24 * 60 * 60)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn unit(label: &str, bytes: u64, days_ago: u64, profile: usize) -> Unit {
        Unit {
            label: label.into(),
            paths: Vec::new(),
            bytes,
            last_used: SystemTime::now() - Duration::from_secs(days_ago * 24 * 60 * 60),
            profile,
        }
    }

    fn labels(units: &[Unit], plan: &Plan) -> Vec<String> {
        plan.remove
            .iter()
            .map(|&i| units[i].label.clone())
            .collect()
    }

    #[test]
    fn removes_oldest_first_until_goal() {
        let units = vec![
            unit("new", 10, 1, 0),
            unit("oldest", 10, 30, 0),
            unit("old", 10, 10, 0),
            unit("mid", 10, 5, 0),
        ];
        let cutoff = SystemTime::now();
        let plan = plan(&units, 40, 20, cutoff, |_| false);
        assert_eq!(labels(&units, &plan), ["oldest", "old"]);
    }

    #[test]
    fn keeps_recent_units_even_if_over_goal() {
        let units = vec![unit("old", 10, 10, 0), unit("recent", 100, 0, 0)];
        let cutoff = SystemTime::now() - Duration::from_secs(60 * 60);
        let plan = plan(&units, 110, 0, cutoff, |_| false);
        assert_eq!(labels(&units, &plan), ["old"]);
        assert_eq!(plan.recent_bytes, 100);
    }

    #[test]
    fn skips_busy_profiles() {
        let units = vec![unit("locked", 10, 30, 1), unit("free", 10, 20, 0)];
        let plan = plan(&units, 20, 0, SystemTime::now(), |u| u.profile == 1);
        assert_eq!(labels(&units, &plan), ["free"]);
        assert_eq!(plan.busy_bytes, 10);
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(10 * 1024 * 1024 * 1024), "10.0 GiB");
    }
}
