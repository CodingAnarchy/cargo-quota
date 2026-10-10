use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use sysinfo::Disks;

use crate::config::Basis;

/// Total capacity, in bytes, that the budget percentage is taken of.
pub fn total_space(target_dir: &Path, basis: Basis) -> Result<u64> {
    let disks = Disks::new_with_refreshed_list();
    match basis {
        Basis::Volume => {
            // The target dir may not exist yet; measure the nearest ancestor that does.
            let path = target_dir
                .ancestors()
                .find_map(|p| p.canonicalize().ok())
                .context("cannot resolve target directory")?;
            disks
                .list()
                .iter()
                .filter(|d| path.starts_with(d.mount_point()))
                .max_by_key(|d| d.mount_point().as_os_str().len())
                .map(|d| d.total_space())
                .with_context(|| format!("no mounted volume contains {}", path.display()))
        }
        Basis::System => {
            // APFS (and btrfs, ZFS, ...) volumes sharing a pool each report the
            // whole pool's size, so count each filesystem/size pair once.
            let mut seen = HashSet::new();
            let total = disks
                .list()
                .iter()
                .filter(|d| !d.is_removable())
                .filter(|d| seen.insert((d.file_system().to_owned(), d.total_space())))
                .map(|d| d.total_space())
                .sum();
            Ok(total)
        }
    }
}
