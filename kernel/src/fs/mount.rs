use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use kernel_intf::KError;
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::sync::Spinlock;
use super::vfs::Vfs;

struct MountEntry {
    mount_point: String,
    vfs: Arc<Spinlock<Vfs>, PoolAllocatorGlobal>
}

static MOUNTS: Spinlock<Vec<MountEntry>> = Spinlock::new(Vec::new());

pub fn mount(path: &str, vfs: Vfs) -> Result<(), KError> {
    let arc = Arc::new_in(Spinlock::new(vfs), PoolAllocatorGlobal);
    MOUNTS.lock().push(MountEntry { mount_point: path.into(), vfs: arc });
    Ok(())
}

// Returns (vfs_arc, path_relative_to_mount_point) for the best-matching mount.
pub fn lookup(abs_path: &str) -> Option<(Arc<Spinlock<Vfs>, PoolAllocatorGlobal>, String)> {
    let mounts = MOUNTS.lock();
    let mut best: Option<&MountEntry> = None;
    for entry in mounts.iter() {
        let mp = entry.mount_point.as_str();
        let is_prefix = if mp == "/" {
            true
        } else {
            abs_path == mp
            || abs_path.starts_with(&alloc::format!("{}/", mp))
        };
        if is_prefix {
            if best.map_or(true, |b| mp.len() > b.mount_point.len()) {
                best = Some(entry);
            }
        }
    }
    best.map(|e| {
        let rel = if e.mount_point == "/" {
            abs_path.to_string()
        } else {
            let stripped = &abs_path[e.mount_point.len()..];
            if stripped.is_empty() { "/".into() } else { stripped.into() }
        };
        (Arc::clone(&e.vfs), rel)
    })
}
