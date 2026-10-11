//! Remembers the last measurement, so that builds in quick succession don't
//! each walk the whole target dir.
//!
//! The stamp lives in the target dir as `.cargo-quota-stamp`:
//!
//! ```text
//! <unix seconds> <bytes>
//! <each measured dir, one per line>
//! ```
//!
//! The dir list makes a stamp written without `--worktrees` (or for another
//! build dir) not count for a run that would measure something else.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const FILE: &str = ".cargo-quota-stamp";

pub struct Stamp {
    pub age: Duration,
    pub bytes: u64,
}

/// The last measurement of `dirs`, if one was recorded.
pub fn read(target_dir: &Path, dirs: &[PathBuf]) -> Option<Stamp> {
    let text = fs::read_to_string(target_dir.join(FILE)).ok()?;
    let (header, measured) = text.split_once('\n').unwrap_or((&text, ""));
    if measured != key(dirs) {
        return None;
    }
    let (secs, bytes) = header.split_once(' ')?;
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(secs.parse().ok()?);
    Some(Stamp {
        // A stamp from the future (clock changes) is treated as brand new,
        // which at worst delays one check by the interval.
        age: SystemTime::now().duration_since(when).unwrap_or_default(),
        bytes: bytes.parse().ok()?,
    })
}

/// Records that `dirs` now total `bytes`. Best effort: a target dir that
/// doesn't exist yet, or can't be written, just means measuring next time.
pub fn write(target_dir: &Path, dirs: &[PathBuf], bytes: u64) {
    if !target_dir.is_dir() {
        return;
    }
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _ = fs::write(
        target_dir.join(FILE),
        format!("{secs} {bytes}\n{}", key(dirs)),
    );
}

fn key(dirs: &[PathBuf]) -> String {
    dirs.iter().map(|d| format!("{}\n", d.display())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_only_for_the_same_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = [tmp.path().to_owned(), PathBuf::from("/other/target")];
        write(tmp.path(), &dirs, 1234);

        let stamp = read(tmp.path(), &dirs).unwrap();
        assert_eq!(stamp.bytes, 1234);
        assert!(stamp.age < Duration::from_secs(60));
        assert!(read(tmp.path(), &dirs[..1]).is_none());
    }

    #[test]
    fn skips_missing_target_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("target");
        write(&missing, std::slice::from_ref(&missing), 1);
        assert!(!missing.exists());
    }
}
