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
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
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
    /// Distinct top-level directories scanned, with their on-disk sizes.
    pub roots: Vec<(PathBuf, u64)>,
    /// Profile directories, e.g. `target/debug`, `target/aarch64-apple-darwin/release`.
    pub profiles: Vec<PathBuf>,
    pub units: Vec<Unit>,
    /// On-disk size of all roots, counting hard links once.
    pub total_bytes: u64,
}

impl Scan {
    /// Measures `dirs` (target dirs, build dirs, other worktrees' target dirs)
    /// as one pool. Duplicates and directories nested inside another count
    /// once. Units aren't grouped until [`Scan::find_units`].
    pub fn measure(dirs: &[PathBuf]) -> Scan {
        let seen = Mutex::new(HashSet::new());
        let mut total_bytes = 0;
        let mut roots = Vec::new();
        for root in distinct_roots(dirs) {
            let bytes = measure_parallel(&root, &seen);
            total_bytes += bytes;
            roots.push((root, bytes));
        }
        Scan {
            roots,
            profiles: Vec::new(),
            units: Vec::new(),
            total_bytes,
        }
    }

    /// Groups the roots' contents into units. This walks everything again, so
    /// it's skipped when the pool is under budget.
    pub fn find_units(&mut self) {
        // Labels are relative to whatever the roots have in common: `debug/...`
        // for one target dir, `proj-feature/target/debug/...` across worktrees.
        let base = common_ancestor(self.roots.iter().map(|(r, _)| r.as_path()));
        self.profiles = self
            .roots
            .iter()
            .flat_map(|(r, _)| find_profiles(r))
            .collect();
        self.units.clear();
        for (index, profile) in self.profiles.iter().enumerate() {
            let name = profile
                .strip_prefix(&base)
                .unwrap_or(profile)
                .display()
                .to_string();
            self.units.extend(scan_profile(profile, &name, index));
        }
    }
}

/// Canonicalizes and dedups `dirs`, dropping missing ones and any nested
/// inside another (which scanning the outer one already covers).
fn distinct_roots(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = dirs.iter().filter_map(|d| d.canonicalize().ok()).collect();
    // Sorting puts each directory right before anything nested inside it.
    roots.sort();
    roots.dedup();
    let mut distinct: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !distinct.iter().any(|outer| root.starts_with(outer)) {
            distinct.push(root);
        }
    }
    distinct
}

fn common_ancestor<'a>(mut paths: impl Iterator<Item = &'a Path>) -> PathBuf {
    let Some(first) = paths.next() else {
        return PathBuf::new();
    };
    let mut base = first.to_owned();
    for path in paths {
        while !path.starts_with(&base) && base.pop() {}
    }
    base
}

/// How far below a root to look for profile dirs.
const MAX_PROFILE_DEPTH: usize = 6;

/// Profile dirs are recognized by their `.fingerprint/` subdirectory. They're
/// usually `<target>/<profile>` or, when cross-compiling,
/// `<target>/<triple>/<profile>`, but people also point `CARGO_TARGET_DIR`
/// at subdirectories of `target/`, nesting whole target dirs inside it.
fn find_profiles(root: &Path) -> Vec<PathBuf> {
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
    for sub in subdirs(root) {
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
    let unhashed = unhashed_outputs(profile, &by_key);
    for sub in ["build", "deps", "examples"] {
        for (name, path) in entries(&profile.join(sub)) {
            if let Some(hash) = unit_hash(&name) {
                add(
                    &mut by_key,
                    fingerprints.get(hash).map_or(hash, String::as_str),
                    path,
                );
            } else if let Some(key) = name.split('.').next().and_then(|stem| unhashed.get(stem)) {
                add(&mut by_key, key, path);
            }
        }
    }
    for (name, path) in entries(&profile.join("incremental")) {
        add(&mut by_key, &format!("incremental/{name}"), path);
    }

    by_key.into_values().collect()
}

/// Some binaries are written without the hash: on Windows MSVC (so `.pdb`
/// names match) and wasm targets, `deps/app.exe` rather than
/// `deps/app-<hash>.exe`. Their unit's fingerprint dir still records the
/// target name (`bin-app`, `example-demo`), so map file stems back to units.
/// If several units claim a name, the most recently used one owns the file,
/// since it's the one that wrote it last.
fn unhashed_outputs(
    profile: &Path,
    fingerprint_units: &HashMap<String, Unit>,
) -> HashMap<String, String> {
    let mut owners: HashMap<String, (&String, SystemTime)> = HashMap::new();
    for (key, unit) in fingerprint_units {
        for (file, _) in entries(&profile.join(".fingerprint").join(key)) {
            let Some(target) = file
                .strip_prefix("bin-")
                .or_else(|| file.strip_prefix("example-"))
            else {
                continue;
            };
            // Skip `bin-app.json` and the like; the bare `bin-app` names the target.
            if target.contains('.') {
                continue;
            }
            // rustc names outputs after the crate name, which uses underscores.
            let stem = target.replace('-', "_");
            if owners
                .get(&stem)
                .is_none_or(|(_, used)| unit.last_used > *used)
            {
                owners.insert(stem, (key, unit.last_used));
            }
        }
    }
    owners
        .into_iter()
        .map(|(stem, (key, _))| (stem, key.clone()))
        .collect()
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

/// [`measure`]s `root`'s on-disk size using all cores. Target dirs hold
/// hundreds of thousands of files, and this runs before every build.
fn measure_parallel(root: &Path, seen: &Mutex<HashSet<(u64, u64)>>) -> u64 {
    // Two levels down (`target/debug/deps`, ...) there's enough independent
    // work to spread around. Directories themselves don't count toward size.
    let mut jobs = Vec::new();
    for (_, child) in entries(root) {
        if child.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            jobs.extend(entries(&child).map(|(_, grandchild)| grandchild));
        } else {
            jobs.push(child);
        }
    }

    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism().map_or(4, |n| n.get().min(8));
    thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut usage = Usage::default();
                    while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                        measure(job, &mut usage, Some(seen));
                    }
                    usage.bytes
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
    })
}

#[derive(Default)]
struct Usage {
    bytes: u64,
    newest: Option<SystemTime>,
}

/// Recursively adds up on-disk size and the newest use time under `path`,
/// without following symlinks. With `seen`, hard-linked files count once.
fn measure(path: &Path, usage: &mut Usage, seen: Option<&Mutex<HashSet<(u64, u64)>>>) {
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
            measure(&child, usage, seen);
        }
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1
            && seen.is_some_and(|s| !s.lock().unwrap().insert((meta.dev(), meta.ino())))
        {
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
    fn collapses_duplicate_and_nested_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = |p: &str| {
            let path = tmp.path().join(p);
            fs::create_dir_all(&path).unwrap();
            path
        };
        let main = dir("main/target");
        let nested = dir("main/target/wt-a");
        let other = dir("feature/target");
        let missing = tmp.path().join("gone/target");
        let roots = distinct_roots(&[nested, other.clone(), main.clone(), main.clone(), missing]);
        assert_eq!(
            roots,
            [other.canonicalize().unwrap(), main.canonicalize().unwrap()]
        );
    }

    #[test]
    fn finds_common_ancestor() {
        let paths = [Path::new("/p/main/target"), Path::new("/p/feature/target")];
        assert_eq!(common_ancestor(paths.into_iter()), Path::new("/p"));
        assert_eq!(
            common_ancestor([Path::new("/p/main/target")].into_iter()),
            Path::new("/p/main/target")
        );
    }

    #[test]
    fn claims_unhashed_binaries_for_their_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("debug");
        let write = |p: &str| {
            let path = profile.join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        };
        // A Windows MSVC build of bin `my-app` depending on lib `dep`.
        write(".fingerprint/my-app-0123456789abcdef/bin-my-app");
        write(".fingerprint/my-app-0123456789abcdef/bin-my-app.json");
        write(".fingerprint/dep-fedcba9876543210/lib-dep");
        write("deps/my_app.exe");
        write("deps/my_app.pdb");
        write("deps/my_app.d");
        write("deps/libdep-fedcba9876543210.rlib");
        write("deps/unrelated.exe");

        let units = scan_profile(&profile, "debug", 0);
        let files = |label: &str| -> Vec<String> {
            let unit = units.iter().find(|u| u.label == label).unwrap();
            let mut names: Vec<String> = unit
                .paths
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            files("debug/my-app-0123456789abcdef"),
            [
                "my-app-0123456789abcdef",
                "my_app.d",
                "my_app.exe",
                "my_app.pdb"
            ]
        );
        assert_eq!(
            files("debug/dep-fedcba9876543210"),
            ["dep-fedcba9876543210", "libdep-fedcba9876543210.rlib"]
        );
        assert_eq!(units.len(), 2, "unrelated.exe should belong to no unit");
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
