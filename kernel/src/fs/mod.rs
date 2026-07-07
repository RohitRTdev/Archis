mod file;
mod mount;
mod module_fs;
mod root;
mod utils;
mod vfs;

pub use file::FileInstance;
pub use mount::MountSource;
pub use utils::FileBuffer;
pub use vfs::{FileAttrs, FileStat, HandleStatType, make_absolute, normalize_path, MODE_FILE, MODE_DIR, MODE_SYMLINK};

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use alloc::string::String;
use alloc::sync::Arc;
use kernel_intf::{info, KError};
use crate::sched::OPEN_CREATE_FLAG;
use crate::sync::{KEvent, Once};
use vfs::ProbeStep;

static STOP_FS: AtomicBool = AtomicBool::new(false);
static FS_OP_COUNT: AtomicUsize = AtomicUsize::new(0);
static FS_STOP_EVENT: Once<KEvent> = Once::new();

struct FsOpGuard;

impl FsOpGuard {
    fn enter() -> Result<FsOpGuard, KError> {
        FS_OP_COUNT.fetch_add(1, Ordering::AcqRel);
        if STOP_FS.load(Ordering::Acquire) {
            Self::leave();
            crate::fs_log!("fs operation aborted, fs is stopped");
            return Err(KError::FsStopped);
        }
        Ok(FsOpGuard)
    }

    fn leave() {
        if FS_OP_COUNT.fetch_sub(1, Ordering::AcqRel) == 1 {
            FS_STOP_EVENT.get().expect("fs::init not called").signal();
        }
    }
}

impl Drop for FsOpGuard {
    fn drop(&mut self) { Self::leave(); }
}

pub fn stop_fs() {
    STOP_FS.store(true, Ordering::Release);
    if FS_OP_COUNT.load(Ordering::Acquire) != 0 {
        let _ = FS_STOP_EVENT.get().expect("fs::init not called").wait(false);
    }
}

// A fresh, empty in-memory mount source, for callers outside this module that
// want to mount a scratch backend without needing the private `Vfs` type.
pub fn new_memory_source() -> MountSource {
    MountSource::Memory(vfs::Vfs::new())
}

fn cwd() -> String {
    crate::sched::get_cwd()
}

fn abs(path: &str) -> String {
    if path.starts_with('/') {
        normalize_path(path)
    } else {
        make_absolute(&cwd(), path)
    }
}

fn join_rel(parent_rel: &str, leaf: &str) -> String {
    if parent_rel == "/" { alloc::format!("/{}", leaf) } else { alloc::format!("{}/{}", parent_rel, leaf) }
}

// Resolves all symlinks in path
fn resolve(path: &str, follow_final: bool) -> Result<(mount::MountBackend, String, String, FileAttrs, Option<String>), KError> {
    let mut full = abs(path);
    let mut depth = 0usize;
    loop {
        if depth > vfs::MAX_SYMLINK_DEPTH {
            return Err(KError::TooManySymlinks);
        }
        let (backend, rel, mount_point) = mount::lookup_mp(&full).ok_or(KError::NotFound)?;
        crate::fs_log!("resolve hop full={} mount_point={} rel={} follow_final={}", full, mount_point, rel, follow_final);
        let step = match &backend {
            mount::MountBackend::Memory(vfs_arc) => vfs_arc.lock().probe(&rel, follow_final)?,
            mount::MountBackend::Module(m) => m.probe(&rel, follow_final)?
        };
        match step {
            ProbeStep::Found { attrs, symlink_target } => {
                crate::fs_log!("resolve found mount_point={} rel={}", mount_point, rel);
                return Ok((backend, mount_point, rel, attrs, symlink_target));
            }
            ProbeStep::Symlink { dir, target, remaining } => {
                let abs_dir = mount::to_absolute(&mount_point, &dir);
                full = vfs::Vfs::join_symlink_target(&abs_dir, &target, &remaining);
                crate::fs_log!("resolve symlink dir={} target={} remaining={} -> next full={}", dir, target, remaining, full);
                depth += 1;
            }
        }
    }
}

pub fn mount(path: &str, source: MountSource) -> Result<(), KError> {
    mount::mount(path, source)
}

pub fn unmount(path: &str) -> Result<(), KError> {
    mount::unmount(path)
}

pub fn open(path: &str) -> Result<FileInstance, KError> {
    let _guard = FsOpGuard::enter()?;
    let full = abs(path);
    let (backend, _, rel, attrs, _) = resolve(path, true)?;
    match backend {
        mount::MountBackend::Memory(vfs_arc) => {
            let vfs = vfs_arc.lock();
            let (node, ancestors, is_dir) = vfs.open_at(&rel)?;
            drop(vfs);
            Ok(file::make_handle(full, node, ancestors, is_dir))
        }
        mount::MountBackend::Module(backend) => {
            let (handle, is_dir) = backend.open_at(&rel)?;
            Ok(file::make_module_handle(full, backend, handle, is_dir, attrs.size))
        }
    }
}

pub fn stat(path: &str) -> Result<FileAttrs, KError> {
    let _guard = FsOpGuard::enter()?;
    resolve(path, true).map(|(_, _, _, attrs, _)| attrs)
}

pub fn lstat(path: &str) -> Result<(FileAttrs, Option<String>), KError> {
    let _guard = FsOpGuard::enter()?;
    resolve(path, false).map(|(_, _, _, attrs, target)| (attrs, target))
}

pub fn create_file(path: &str, mode: u16) -> Result<(), KError> {
    let _guard = FsOpGuard::enter()?;
    let full = abs(path);
    let (parent_path, leaf) = vfs::split_parent(&full);
    if leaf.is_empty() { return Err(KError::InvalidArgument); }
    let (backend, _, parent_rel, _, _) = resolve(parent_path, true)?;
    match backend {
        mount::MountBackend::Memory(vfs_arc) => vfs_arc.lock().create_file_in(&parent_rel, leaf, mode),
        mount::MountBackend::Module(backend) => backend.create_file(&join_rel(&parent_rel, leaf), mode)
    }
}

pub fn mkdir(path: &str, mode: u16) -> Result<(), KError> {
    let _guard = FsOpGuard::enter()?;
    let full = abs(path);
    let (parent_path, leaf) = vfs::split_parent(&full);
    if leaf.is_empty() { return Err(KError::InvalidArgument); }
    let (backend, _, parent_rel, _, _) = resolve(parent_path, true)?;
    match backend {
        mount::MountBackend::Memory(vfs_arc) => vfs_arc.lock().mkdir_in(&parent_rel, leaf, mode),
        mount::MountBackend::Module(backend) => backend.create_dir(&join_rel(&parent_rel, leaf), mode)
    }
}

pub fn create_symlink(path: &str, target: &str) -> Result<(), KError> {
    let _guard = FsOpGuard::enter()?;
    let full = abs(path);
    let (parent_path, leaf) = vfs::split_parent(&full);
    if leaf.is_empty() { return Err(KError::InvalidArgument); }
    let (backend, _, parent_rel, _, _) = resolve(parent_path, true)?;
    match backend {
        mount::MountBackend::Memory(vfs_arc) => vfs_arc.lock().create_symlink_in(&parent_rel, leaf, target),
        mount::MountBackend::Module(backend) => backend.create_symlink(&join_rel(&parent_rel, leaf), target)
    }
}

pub fn delete(path: &str) -> Result<(), KError> {
    let _guard = FsOpGuard::enter()?;
    // follow_final=false: deleting a symlink deletes the symlink itself.
    let (backend, mount_point, rel, _, _) = resolve(path, false)?;
    let target_full = mount::to_absolute(&mount_point, &rel);
    // A directory that is itself a mount point, or has one somewhere in its
    // subtree, can't be removed 
    if mount::is_mount_point(&target_full) || mount::has_mount_within(&target_full) {
        crate::fs_log!("delete rejected, {} is/contains a mount point", target_full);
        return Err(KError::FileBusy);
    }
    let (parent_rel, leaf) = vfs::split_parent(&rel);
    match backend {
        mount::MountBackend::Memory(vfs_arc) => vfs_arc.lock().delete_in(parent_rel, leaf),
        mount::MountBackend::Module(backend) => {
            if backend.is_path_open(&rel) {
                crate::fs_log!("delete rejected, {} is open (module backend)", rel);
                return Err(KError::FileBusy);
            }
            backend.delete(&rel)
        }
    }
}

pub fn rename(from: &str, to: &str) -> Result<(), KError> {
    let _guard = FsOpGuard::enter()?;
    let full_to = abs(to);
    let (to_parent_path, to_leaf) = vfs::split_parent(&full_to);
    if to_leaf.is_empty() { return Err(KError::InvalidArgument); }

    // follow_final=false on `from`: renaming a symlink renames the symlink itself.
    let (backend_from, mp_from, rel_from, _, _) = resolve(from, false)?;
    let (backend_to, mp_to, to_parent_rel, _, _) = resolve(to_parent_path, true)?;
    let rel_to = join_rel(&to_parent_rel, to_leaf);

    let from_full = mount::to_absolute(&mp_from, &rel_from);
    let to_full = mount::to_absolute(&mp_to, &rel_to);

    // Neither the source nor the destination may be, or contain, a mount point.
    if mount::is_mount_point(&from_full) {
        crate::fs_log!("rename rejected, source {} is a mount point", from_full);
        return Err(KError::FileBusy);
    }
    if mount::is_mount_point(&to_full) {
        crate::fs_log!("rename rejected, destination {} is a mount point", to_full);
        return Err(KError::FileBusy);
    }

    match (&backend_from, &backend_to) {
        (mount::MountBackend::Memory(a), mount::MountBackend::Memory(b)) if Arc::ptr_eq(a, b) => {
            let (from_parent_rel, from_leaf) = vfs::split_parent(&rel_from);
            a.lock().rename_in(from_parent_rel, from_leaf, &to_parent_rel, to_leaf)
        }
        (mount::MountBackend::Module(a), mount::MountBackend::Module(b)) if Arc::ptr_eq(a, b) => {
            if a.is_path_open(&rel_from) {
                crate::fs_log!("rename rejected, {} is open (module backend)", rel_from);
                return Err(KError::FileBusy);
            }
            a.rename(&rel_from, &rel_to)
        }
        _ => Err(KError::Unsupported)
    }
}

pub fn resolve_symlink(path: &str) -> Result<String, KError> {
    let _guard = FsOpGuard::enter()?;
    resolve(path, true).map(|(_, mp, rel, _, _)| mount::to_absolute(&mp, &rel))
}

pub fn chdir(path: &str) -> Result<(), KError> {
    let handle = open(path)?;
    if !handle.is_dir() {
        return Err(KError::NotADirectory);
    }
    crate::sched::set_cwd(handle);
    Ok(())
}

pub fn create_or_open(path: &str, file_exist_only: bool) -> Result<FileInstance, KError> {
    if file_exist_only {
        return open(path);
    }
    let _ = delete(path);
    create_file(path, 0)?;
    open(path)
}

fn open_fs_handler(name: &str, flags: u64) -> Result<crate::sched::Handle, KError> {
    let res = if flags & OPEN_CREATE_FLAG != 0 {
        create_or_open(name, false)?
    }
    else {
        open(name)?
    };

    Ok(crate::sched::Handle::FileHandle(res))
}

pub fn init() {
    FS_STOP_EVENT.call_once(|| KEvent::new(true));

    let init_fs = crate::INIT_FS.get().expect("fs::init called before INIT_FS is ready");
    let mut vfs_instance = vfs::Vfs::new();
    vfs_instance.populate(&init_fs.fs, &init_fs.symlinks);

    mount::mount("/", MountSource::Memory(vfs_instance)).expect("fs::init: failed to mount VFS at /");

    let root = open("/").expect("fs::init: failed to open /");
    crate::sched::set_init_cwd(root);

    crate::object::register_object_type("fs", open_fs_handler)
        .expect("fs: failed to register object type");

    info!("VFS mounted at /");
}
