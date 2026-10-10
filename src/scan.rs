//! Groups a target directory's contents into removable units.
//!
//! Cargo names every compilation unit's outputs with a 16-hex-digit metadata
//! hash, and uses the same hash across a profile directory:
//!
//! ```text
//! debug/.fingerprint/serde-0123456789abcdef/      freshness tracking
//! debug/build/serde-0123456789abcdef/             build script binary or output
//! debug/deps/libserde-0123456789abcdef.rlib       compiled outputs, .d files,
//! debug/deps/serde-0123456789abcdef.serde.x.rcgu.o  object files, test binaries, ...
//! debug/examples/demo-0123456789abcdef
//! ```
//!
//! Removing all of a unit's files together leaves cargo's state consistent: it
//! sees no fingerprint and rebuilds that unit if it's ever needed again.
//! Incremental compilation caches (`incremental/<crate>-<id>/`) use a different
//! naming scheme, so each is its own unit.
//!
//! Anything else (`doc/`, `package/`, uplifted binaries, `.cargo-lock`, ...)
//! counts toward the total size but is never removed.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug)]
pub struct Unit {
    /// For display, e.g. `debug/serde-0123456789abcdef`.
    pub label: String,
    pub paths: Vec<PathBuf>,
    pub bytes: u64,
    /// Newest access or modification time of anything in the unit.
    pub last_used: SystemTime,
    /// Index into [`Scan::profiles`].
    pub profile: usize,
}

#[derive(Debug)]
pub struct Scan {
    /// Profile directories, e.g. `target/debug`, `target/aarch64-apple-darwin/release`.
    pub profiles: Vec<PathBuf>,
    pub units: Vec<Unit>,
    /// On-disk size of the target and build directories, counting hard links once.
    pub total_bytes: u64,
}

pub fn scan(target_dir: &Path, build_dir: &Path) -> Scan {
    let mut seen = HashSet::new();
    let mut total = Usage::default();
    measure(target_dir, &mut total, Some(&mut seen));
    if build_dir != target_dir {
        measure(build_dir, &mut total, Some(&mut seen));
    }

    let profiles = find_profiles(build_dir);
    let mut units = Vec::new();
    for (index, profile) in profiles.iter().enumerate() {
        let name = profile
            .strip_prefix(build_dir)
            .unwrap_or(profile)
            .display()
            .to_string();
        units.extend(scan_profile(profile, &name, index));
    }

    Scan {
        profiles,
        units,
        total_bytes: total.bytes,
    }
}

/// How far below the build dir to look for profile dirs.
const MAX_PROFILE_DEPTH: usize = 6;

/// Profile dirs are recognized by their `.fingerprint/` subdirectory. They're
/// usually `<build_dir>/<profile>` or, when cross-compiling,
/// `<build_dir>/<triple>/<profile>`, but people also point `CARGO_TARGET_DIR`
/// at subdirectories of `target/`, nesting whole target dirs inside it.
fn find_profiles(build_dir: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, depth: usize, profiles: &mut Vec<PathBuf>) {
        if dir.join(".fingerprint").is_dir() {
            profiles.push(dir.to_owned());
        } else if depth < MAX_PROFILE_DEPTH {
            for sub in subdirs(dir) {
                visit(&sub, depth + 1, profiles);
            }
        }
    }
    let mut profiles = Vec::new();
    for sub in subdirs(build_dir) {
        visit(&sub, 1, &mut profiles);
    }
    profiles.sort();
    profiles
}

fn scan_profile(profile: &Path, profile_name: &str, index: usize) -> Vec<Unit> {
    let mut by_key: HashMap<String, Unit> = HashMap::new();
    let add = |by_key: &mut HashMap<String, Unit>, key: &str, path: PathBuf| {
        let mut usage = Usage::default();
        measure(&path, &mut usage, None);
        let unit = by_key.entry(key.to_owned()).or_insert_with(|| Unit {
            label: format!("{profile_name}/{key}"),
            paths: Vec::new(),
            bytes: 0,
            last_used: SystemTime::UNIX_EPOCH,
            profile: index,
        });
        unit.paths.push(path);
        unit.bytes += usage.bytes;
        unit.last_used = unit
            .last_used
            .max(usage.newest.unwrap_or(SystemTime::UNIX_EPOCH));
    };

    // .fingerprint goes first so its package-named entry (`proc-macro2-<hash>`)
    // labels the unit, rather than a bare hash.
    for (name, path) in entries(&profile.join(".fingerprint")) {
        if unit_hash(&name).is_some() {
            add(&mut by_key, &name, path);
        }
    }
    let fingerprints: HashMap<String, String> = by_key
        .keys()
        .map(|k| (unit_hash(k).unwrap().to_owned(), k.clone()))
        .collect();
    for sub in ["build", "deps", "examples"] {
        for (name, path) in entries(&profile.join(sub)) {
            if let Some(hash) = unit_hash(&name) {
                add(
                    &mut by_key,
                    fingerprints.get(hash).map_or(hash, String::as_str),
                    path,
                );
            }
        }
    }
    for (name, path) in entries(&profile.join("incremental")) {
        add(&mut by_key, &format!("incremental/{name}"), path);
    }

    by_key.into_values().collect()
}

/// Extracts cargo's metadata hash from an artifact name: the 16 lowercase hex
/// digits after a `-`, followed by the end of the name or a `.`.
pub fn unit_hash(name: &str) -> Option<&str> {
    name.match_indices('-').find_map(|(i, _)| {
        let hash = name.get(i + 1..i + 17)?;
        let rest = &name[i + 17..];
        let is_hex = hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        (is_hex && (rest.is_empty() || rest.starts_with('.'))).then_some(hash)
    })
}

#[derive(Default)]
struct Usage {
    bytes: u64,
    newest: Option<SystemTime>,
}

/// Recursively adds up on-disk size and the newest use time under `path`,
/// without following symlinks. With `seen`, hard-linked files count once.
fn measure(path: &Path, usage: &mut Usage, mut seen: Option<&mut HashSet<(u64, u64)>>) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };

    // A file's atime records cargo or rustc reading it. A directory's atime only
    // records someone listing it (including this scan), so it's ignored.
    let accessed = if meta.is_dir() {
        None
    } else {
        meta.accessed().ok()
    };
    for time in [meta.modified().ok(), accessed].into_iter().flatten() {
        usage.newest = Some(usage.newest.map_or(time, |t| t.max(time)));
    }

    if meta.is_dir() {
        for (_, child) in entries(path) {
            measure(&child, usage, seen.as_deref_mut());
        }
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 && seen.is_some_and(|s| !s.insert((meta.dev(), meta.ino()))) {
            return;
        }
        usage.bytes += meta.blocks() * 512;
    }
    #[cfg(not(unix))]
    {
        let _ = seen;
        usage.bytes += meta.len();
    }
}

fn entries(dir: &Path) -> impl Iterator<Item = (String, PathBuf)> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
}

fn subdirs(dir: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_unit_hashes() {
        let h = "0123456789abcdef";
        assert_eq!(unit_hash(&format!("serde-{h}")), Some(h));
        assert_eq!(unit_hash(&format!("libserde_json-{h}.rlib")), Some(h));
        assert_eq!(unit_hash(&format!("serde-{h}.d")), Some(h));
        assert_eq!(
            unit_hash(&format!(
                "agent-{h}.05ytcog19tiysey3layu8kuyi.010hqz0.rcgu.o"
            )),
            Some(h)
        );
        assert_eq!(unit_hash(&format!("proc-macro2-{h}")), Some(h));
        assert_eq!(unit_hash(&format!("my-tool-{h}.dSYM")), Some(h));
    }

    #[test]
    fn rejects_non_unit_names() {
        assert_eq!(unit_hash("libserde.rlib"), None);
        assert_eq!(unit_hash("agent-03y2e0vn0dpdt"), None); // incremental dir
        assert_eq!(unit_hash("serde-0123456789ABCDEF"), None);
        assert_eq!(unit_hash("serde-0123456789abcdef0"), None);
        assert_eq!(unit_hash(".cargo-lock"), None);
    }
}
