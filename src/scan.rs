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
//! sees no fingerprint and rebuilds that unit if it's ever needed again. The
//! copies cargo makes of final outputs (`debug/app`, `debug/libfoo.rlib`) go
//! with the unit that produced them.
//!
//! Incremental compilation caches (`incremental/<crate>-<id>/`) use a different
//! naming scheme, so each is its own unit. So are whole output directories
//! outside the profiles, like `doc/` and `package/` (see [`EXTRA_DIRS`]).
//!
//! Anything else (`.rustc_info.json`, `.cargo-lock`, ...) counts toward the
//! total size but is never removed.

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
    /// Indices into [`Scan::profiles`] whose `.cargo-lock` must be held to
    /// remove the unit.
    pub locks: Vec<usize>,
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
        let label = |path: &Path| {
            path.strip_prefix(&base)
                .unwrap_or(path)
                .display()
                .to_string()
        };
        // A unit holds hard links to its own files (`debug/app` and
        // `deps/app-<hash>`), and overcounting would stop pruning early.
        let seen = Mutex::new(HashSet::new());
        self.units.clear();
        for (index, profile) in self.profiles.iter().enumerate() {
            self.units
                .extend(scan_profile(profile, &label(profile), index, &seen));
        }
        // Cargo writes these while holding a profile's lock (`doc/` under
        // `debug/`'s, for instance), so require every lock in the root.
        for (root, _) in &self.roots {
            let locks: Vec<usize> = (0..self.profiles.len())
                .filter(|&i| self.profiles[i].starts_with(root))
                .collect();
            for dir in find_extras(root, &self.profiles) {
                let mut unit = Unit::new(label(&dir), locks.clone());
                unit.add(dir, &seen);
                self.units.push(unit);
            }
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

/// Output directories outside the profiles that are safe to remove whole:
/// `cargo doc`, `cargo package`, integration tests' `CARGO_TARGET_TMPDIR`,
/// and common tools' working directories.
const EXTRA_DIRS: [&str; 5] = ["doc", "package", "tmp", "criterion", "nextest"];

/// Finds [`EXTRA_DIRS`] in `root`, and in any directory there that holds
/// profiles (`<triple>/doc`, or a target dir nested inside this one).
fn find_extras(root: &Path, profiles: &[PathBuf]) -> Vec<PathBuf> {
    fn visit(dir: &Path, depth: usize, profiles: &[PathBuf], extras: &mut Vec<PathBuf>) {
        for sub in subdirs(dir) {
            if profiles.iter().any(|p| p.starts_with(&sub)) {
                if !profiles.contains(&sub) && depth < MAX_PROFILE_DEPTH {
                    visit(&sub, depth + 1, profiles, extras);
                }
            } else if sub
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| EXTRA_DIRS.contains(&n))
            {
                extras.push(sub);
            }
        }
    }
    let mut extras = Vec::new();
    visit(root, 1, profiles, &mut extras);
    extras.sort();
    extras
}

impl Unit {
    fn new(label: String, locks: Vec<usize>) -> Unit {
        Unit {
            label,
            paths: Vec::new(),
            bytes: 0,
            last_used: SystemTime::UNIX_EPOCH,
            locks,
        }
    }

    fn add(&mut self, path: PathBuf, seen: &Mutex<HashSet<(u64, u64)>>) {
        let mut usage = Usage::default();
        measure(&path, &mut usage, Some(seen));
        self.paths.push(path);
        self.bytes += usage.bytes;
        self.last_used = self
            .last_used
            .max(usage.newest.unwrap_or(SystemTime::UNIX_EPOCH));
    }
}

/// Subdirectories of a profile dir that hold units, rather than being one.
const PROFILE_SUBDIRS: [&str; 5] = [".fingerprint", "build", "deps", "examples", "incremental"];

fn scan_profile(
    profile: &Path,
    profile_name: &str,
    index: usize,
    seen: &Mutex<HashSet<(u64, u64)>>,
) -> Vec<Unit> {
    let mut by_key: HashMap<String, Unit> = HashMap::new();
    let add = |by_key: &mut HashMap<String, Unit>, key: &str, path: PathBuf| {
        by_key
            .entry(key.to_owned())
            .or_insert_with(|| Unit::new(format!("{profile_name}/{key}"), vec![index]))
            .add(path, seen);
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
            } else if let Some(key) = unhashed_owner(&unhashed, &name) {
                add(&mut by_key, key, path);
            }
        }
    }
    // Final outputs copied up into the profile dir: `app`, `app.d`, `libfoo.rlib`, ...
    for (name, path) in entries(profile) {
        if !PROFILE_SUBDIRS.contains(&name.as_str())
            && let Some(key) = unhashed_owner(&unhashed, &name)
        {
            add(&mut by_key, key, path);
        }
    }
    for (name, path) in entries(&profile.join("incremental")) {
        add(&mut by_key, &format!("incremental/{name}"), path);
    }

    by_key.into_values().collect()
}

/// Some outputs are written without the hash. Cargo copies final outputs up
/// into the profile dir (`debug/my-app`, `debug/libfoo.rlib`,
/// `examples/demo`), and on Windows MSVC (so `.pdb` names match) and wasm
/// targets, binaries are even built as `deps/my_app.exe`. Their unit's
/// fingerprint dir still records the target name (`bin-my-app`, `lib-foo`,
/// `example-demo`), so map file stems back to units. If several units claim
/// a name, the most recently used one owns the file, since it's the one that
/// wrote it last, and it's the last of them to be pruned.
fn unhashed_outputs(
    profile: &Path,
    fingerprint_units: &HashMap<String, Unit>,
) -> HashMap<String, String> {
    let mut owners: HashMap<String, (&String, SystemTime)> = HashMap::new();
    for (key, unit) in fingerprint_units {
        for (file, _) in entries(&profile.join(".fingerprint").join(key)) {
            // Skip `bin-app.json` and the like; the bare `bin-app` names the target.
            if file.contains('.') {
                continue;
            }
            // Copied-up binaries keep the target name; rustc's own output uses
            // the crate name, with underscores.
            let stems = if let Some(target) = file
                .strip_prefix("bin-")
                .or_else(|| file.strip_prefix("example-"))
            {
                vec![target.to_owned(), target.replace('-', "_")]
            } else if let Some(target) = file.strip_prefix("lib-") {
                vec![format!("lib{}", target.replace('-', "_"))]
            } else {
                continue;
            };
            for stem in stems {
                if owners
                    .get(&stem)
                    .is_none_or(|(_, used)| unit.last_used > *used)
                {
                    owners.insert(stem, (key, unit.last_used));
                }
            }
        }
    }
    owners
        .into_iter()
        .map(|(stem, (key, _))| (stem, key.clone()))
        .collect()
}

/// The unit that owns an unhashed file, by the file's stem (`my-app` for
/// `my-app.exe`, `my-app.dSYM`, ...).
fn unhashed_owner<'a>(owners: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    owners.get(name.split('.').next()?).map(String::as_str)
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
    #[cfg(windows)]
    {
        if seen
            .is_some_and(|s| windows_link_id(path).is_some_and(|id| !s.lock().unwrap().insert(id)))
        {
            return;
        }
        usage.bytes += meta.len();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = seen;
        usage.bytes += meta.len();
    }
}

/// Extensions of the files cargo hard-links when copying final outputs up
/// into the profile dir. Windows metadata doesn't include a link count, so
/// finding one means opening the file; doing that for only these keeps it
/// to a few thousand opens rather than every file in the target dir.
#[cfg(windows)]
const LINKED_EXTENSIONS: [&str; 6] = ["exe", "dll", "pdb", "lib", "rlib", "wasm"];

/// `(volume serial, file index)` for a file with more than one hard link.
#[cfg(windows)]
fn windows_link_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let ext = path.extension()?.to_str()?;
    if !LINKED_EXTENSIONS.contains(&ext) {
        return None;
    }
    // No access rights are needed to read a file's attributes, and asking
    // for none never conflicts with a running build's open handles.
    let file = fs::OpenOptions::new().access_mode(0).open(path).ok()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the duration of the call, and `info` is
    // a valid, writable BY_HANDLE_FILE_INFORMATION.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return None;
    }
    (info.nNumberOfLinks > 1).then(|| {
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    })
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
        // Copies cargo makes in the profile dir itself.
        write("my-app.exe");
        write("my-app.d");
        write("libdep.rlib");
        write(".cargo-lock");

        let units = scan_profile(&profile, "debug", 0, &Mutex::new(HashSet::new()));
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
                "my-app.d",
                "my-app.exe",
                "my_app.d",
                "my_app.exe",
                "my_app.pdb"
            ]
        );
        assert_eq!(
            files("debug/dep-fedcba9876543210"),
            [
                "dep-fedcba9876543210",
                "libdep-fedcba9876543210.rlib",
                "libdep.rlib"
            ]
        );
        assert_eq!(units.len(), 2, "unrelated.exe should belong to no unit");
    }

    #[test]
    fn finds_extra_dirs_beside_profiles_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for dir in [
            "debug/.fingerprint",
            "debug/doc",
            "doc/app",
            "package",
            "x86_64-unknown-linux-gnu/release/.fingerprint",
            "x86_64-unknown-linux-gnu/doc",
            "flycheck0",
            "nested/debug/.fingerprint",
            "nested/tmp",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let profiles = find_profiles(root);
        let extras: Vec<PathBuf> = find_extras(root, &profiles)
            .into_iter()
            .map(|p| p.strip_prefix(root).unwrap().to_owned())
            .collect();
        assert_eq!(
            extras,
            [
                PathBuf::from("doc"),
                PathBuf::from("nested/tmp"),
                PathBuf::from("package"),
                Path::new("x86_64-unknown-linux-gnu").join("doc"),
            ]
        );
    }

    #[test]
    fn counts_hard_links_once() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("deps/app-0123456789abcdef.exe");
        fs::create_dir_all(original.parent().unwrap()).unwrap();
        fs::write(&original, vec![1u8; 64 * 1024]).unwrap();
        let mut one = Usage::default();
        measure(tmp.path(), &mut one, Some(&Mutex::new(HashSet::new())));

        fs::hard_link(&original, tmp.path().join("app.exe")).unwrap();
        let mut both = Usage::default();
        measure(tmp.path(), &mut both, Some(&Mutex::new(HashSet::new())));
        assert_eq!(both.bytes, one.bytes);
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
