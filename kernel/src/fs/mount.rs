use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use kernel_intf::KError;
use kernel_intf::driver::{DeviceObject, DeviceType};
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::fs::mount::MountBackend::Module;
use crate::io::DeviceHandleK;
use crate::sync::Spinlock;
use super::module_fs::ModuleBackedFs;
use super::vfs::{MODE_DIR, Vfs};

pub enum MountSource {
    Device(DeviceHandleK),
    Memory(Vfs)
}

#[derive(Clone)]
pub enum MountBackend {
    Memory(Arc<Spinlock<Vfs>, PoolAllocatorGlobal>),
    Module(Arc<ModuleBackedFs, PoolAllocatorGlobal>)
}

struct MountEntry {
    mount_point: String,
    backend: MountBackend
}

static MOUNTS: Spinlock<Vec<MountEntry>> = Spinlock::new(Vec::new());

fn build_backend(source: MountSource) -> Result<MountBackend, KError> {
    match source {
        MountSource::Memory(vfs) => Ok(MountBackend::Memory(Arc::new_in(Spinlock::new(vfs), PoolAllocatorGlobal))),
        MountSource::Device(dev) => {
            if dev.device_type() != DeviceType::Partition {
                return Err(KError::InvalidArgument);
            }
            ModuleBackedFs::identify_and_open(dev).map(MountBackend::Module)
        }
    }
}

// Mount point must not already exist, there must be no mount points 
// within this point and (except for the bootstrap "/"
// mount into an empty table) must be an existing directory as seen through
// the current mount table.
fn validate_mount_point_and_source(path: &str, source: &MountSource) -> Result<(), KError> {
    let is_empty = MOUNTS.lock().is_empty();
    if is_empty {
        return if path == "/" { Ok(()) } else { Err(KError::InvalidArgument) };
    }
    if is_mount_point(path) {
        return Err(KError::FileExists);
    }
    if has_mount_within(path) {
        return Err(KError::FileBusy);
    }
    let attrs = crate::fs::stat(path)?;
    if attrs.mode & MODE_DIR == 0 {
        return Err(KError::NotADirectory);
    }

    if let MountSource::Device(dev) = source {
        if MOUNTS.lock().iter().any(|m| {
            if let Module(mount_dev) = &m.backend {
                return mount_dev.dev_ptr() == dev.device_ptr();
            }
            false
        }) {
            return Err(KError::DeviceMounted);
        }
    }


    Ok(())
}

pub fn mount(path: &str, source: MountSource) -> Result<(), KError> {
    // Canonicalize before storing
    let canonical = if MOUNTS.lock().is_empty() {
        super::vfs::normalize_path(path)
    } else {
        crate::fs::resolve_symlink(path)?
    };
    crate::fs_log!("mount request path={} canonical={}", path, canonical);
    validate_mount_point_and_source(&canonical, &source)?;
    let backend = build_backend(source)?;
    MOUNTS.lock().push(MountEntry { mount_point: canonical, backend });
    Ok(())
}

// Fetches a clone of the backend currently mounted at `path`, so a caller
// can restore it later 
pub fn backend_of(path: &str) -> Option<MountBackend> {
    MOUNTS.lock().iter().find(|m| m.mount_point == path).map(|m| m.backend.clone())
}

// Restores a backend fetched via `backend_of` at `path`. Only meant for the
// rollback case above: `path` must currently be unmounted.
pub fn remount(path: &str, backend: MountBackend) {
    let canonical = super::vfs::normalize_path(path);
    MOUNTS.lock().push(MountEntry { mount_point: canonical, backend });
}

pub fn unmount(path: &str) -> Result<(), KError> {
    if MOUNTS.lock().is_empty() {
        return Err(KError::NotFound);
    } 
    let canonical = crate::fs::resolve_symlink(path)?;

    if has_mount_within(&canonical) {
        return Err(KError::FileBusy);
    }
    let mut mounts = MOUNTS.lock();
    let idx = mounts.iter().position(|m| m.mount_point == canonical).ok_or(KError::NotFound)?;
    let busy = match &mounts[idx].backend {
        MountBackend::Memory(vfs) => vfs.lock().root_busy(),
        MountBackend::Module(fs) => fs.is_busy()
    };
    if busy {
        return Err(KError::FileBusy);
    }
    if let MountBackend::Module(fs) = &mounts[idx].backend {
        fs.unmount();
    }
    mounts.remove(idx);
    Ok(())
}

pub fn is_mount_point(path: &str) -> bool {
    MOUNTS.lock().iter().any(|m| m.mount_point == path)
}

// True if some other mount point sits strictly inside `path`'s subtree —
// used to refuse removing/unmounting a directory that still has a nested
// mount somewhere below it.
pub fn has_mount_within(path: &str) -> bool {
    let prefix = if path == "/" { "/".to_string() } else { format!("{}/", path) };
    MOUNTS.lock().iter().any(|m| m.mount_point != path && m.mount_point.starts_with(&prefix))
}

// Returns (backend, path_relative_to_mount_point, mount_point) for the
// best-matching (i.e. longest matching prefix) mount.
pub fn lookup_mp(abs_path: &str) -> Option<(MountBackend, String, String)> {
    let mounts = MOUNTS.lock();
    let mut best: Option<&MountEntry> = None;
    for entry in mounts.iter() {
        let mp = entry.mount_point.as_str();
        let is_prefix = if mp == "/" {
            true
        } else {
            abs_path == mp || abs_path.starts_with(&format!("{}/", mp))
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
        (e.backend.clone(), rel, e.mount_point.clone())
    })
}

// Inverse of lookup_mp's rel computation: rebuild the absolute path given a
// mount point and a path relative to it (rel is always "/" or "/a/b/...").
pub fn to_absolute(mount_point: &str, rel: &str) -> String {
    if mount_point == "/" {
        rel.to_string()
    } else if rel == "/" {
        mount_point.to_string()
    } else {
        format!("{}{}", mount_point, rel)
    }
}
