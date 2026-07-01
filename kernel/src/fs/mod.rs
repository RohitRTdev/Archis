mod file;
mod mount;
mod utils;
mod vfs;

pub use file::{FileInst, FileInstance};
pub use utils::FileBuffer;
pub use vfs::{DirEntry, EntryType, FileAttrs, FileStat, make_absolute, normalize_path, MODE_FILE, MODE_DIR, MODE_SYMLINK};

use alloc::string::String;
use alloc::sync::Arc;
use kernel_intf::{info, KError};
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::sched::OPEN_CREATE_FLAG;
use crate::sync::Spinlock;

fn cwd() -> String {
    crate::sched::get_cwd()
}

fn abs(path: &str) -> String {
    make_absolute(&cwd(), path)
}

fn vfs_for(path: &str) -> Result<(Arc<Spinlock<vfs::Vfs>, PoolAllocatorGlobal>, String), KError> {
    mount::lookup(path).ok_or(KError::NotFound)
}

pub fn open(path: &str) -> Result<FileInstance, KError> {
    let full = abs(path);
    let (vfs_arc, rel) = vfs_for(&full)?;
    let vfs = vfs_arc.lock();
    let (node, ancestors, is_dir) = vfs.open(&rel)?;
    drop(vfs);
    Ok(file::make_handle(full, node, ancestors, is_dir))
}

pub fn stat(path: &str) -> Result<FileAttrs, KError> {
    let full = abs(path);
    let (vfs_arc, rel) = vfs_for(&full)?;
    vfs_arc.lock().stat(&rel)
}

pub fn lstat(path: &str) -> Result<(FileAttrs, Option<String>), KError> {
    let full = abs(path);
    let (vfs_arc, rel) = vfs_for(&full)?;
    vfs_arc.lock().lstat(&rel)
}

pub fn create_file(path: &str, mode: u16) -> Result<(), KError> {
    let full = abs(path);
    let (vfs_arc, rel) = vfs_for(&full)?;
    vfs_arc.lock().create_file(&rel, mode)
}

pub fn mkdir(path: &str, mode: u16) -> Result<(), KError> {
    let full = abs(path);
    let (vfs_arc, rel) = vfs_for(&full)?;
    vfs_arc.lock().create_dir(&rel, mode)
}

pub fn create_symlink(path: &str, target: &str) -> Result<(), KError> {
    let full = abs(path);
    let (vfs_arc, rel) = vfs_for(&full)?;
    vfs_arc.lock().create_symlink(&rel, target)
}

pub fn delete(path: &str) -> Result<(), KError> {
    let full = abs(path);
    let (vfs_arc, rel) = vfs_for(&full)?;
    vfs_arc.lock().delete(&rel)
}

pub fn rename(from: &str, to: &str) -> Result<(), KError> {
    let full_from = abs(from);
    let full_to = abs(to);
    let (vfs_arc, rel_from) = vfs_for(&full_from)?;
    let (vfs_arc2, rel_to) = vfs_for(&full_to)?;
    if !Arc::ptr_eq(&vfs_arc, &vfs_arc2) {
        return Err(KError::Unsupported);
    }
    vfs_arc.lock().rename(&rel_from, &rel_to)
}

pub fn resolve_symlink(path: &str) -> Result<String, KError> {
    let full = abs(path);
    let (vfs_arc, rel) = vfs_for(&full)?;
    vfs_arc.lock().resolve_canonical(&rel)
}

pub fn chdir(path: &str) -> Result<(), KError> {
    let full = abs(path);
    let (vfs_arc, rel) = vfs_for(&full)?;
    // Verify target is a directory by opening it and immediately closing.
    let (node, ancestors, is_dir) = vfs_arc.lock().open(&rel)?;
    vfs::Vfs::close(&node, &ancestors);
    if !is_dir {
        return Err(KError::NotADirectory);
    }
    crate::sched::set_cwd(full);
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
    let init_fs = crate::INIT_FS.get().expect("fs::init called before INIT_FS is ready");
    let mut vfs_instance = vfs::Vfs::new();
    vfs_instance.populate(&init_fs.fs, &init_fs.symlinks);

    mount::mount("/", vfs_instance).expect("fs::init: failed to mount VFS at /");

    crate::sched::set_init_cwd("/");

    crate::object::register_object_type("fs", open_fs_handler)
        .expect("fs: failed to register object type");

    info!("VFS mounted at /");
}
