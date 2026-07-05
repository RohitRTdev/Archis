#![cfg_attr(not(test), no_std)]

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;

use common::{MemoryRegion, StrRef};
use kernel_intf::{
    E_FILE_BUSY, E_FILE_EXISTS, E_INVALID, E_IS_DIR, E_IS_SYMLINK, E_NOT_DIR, E_NOT_EMPTY, E_NOT_FOUND,
    E_NO_DIR_ENTRIES, E_SUCCESS, debug, info, sync_wait_semaphore, sync_signal_semaphore
};
use kernel_intf::driver::DeviceObject;
use kernel_intf::fs::{FS_ENTRY_DIR, FS_ENTRY_FILE, FS_ENTRY_SYMLINK, FS_MODE_DIR, FS_MODE_FILE, FS_MODE_SYMLINK, FsDirEntry, FsFileStat};

mod bpb;
mod dir;
mod fat;
mod file_io;
mod format;
mod handle;
mod io_util;
mod sync_state;

use bpb::Bpb;
use dir::{ATTR_ARCHIVE, ATTR_DIRECTORY, ATTR_SYMLINK, DirEntryView};
use handle::{FileKind, OpenFile};

#[kmod::init]
fn module_init() {
    sync_state::init();
    info!("fat32: module initialized");
}

fn get_bpb(dev: *const DeviceObject) -> Result<Bpb, i64> {
    let mut sector = [0u8; bpb::SECTOR_SIZE];
    io_util::read_sectors(dev, 0, &mut sector)?;
    Bpb::decode(&sector).ok_or(E_INVALID)
}

fn entry_mode(entry: &DirEntryView) -> u16 {
    if entry.is_symlink() {
        FS_MODE_SYMLINK
    } else if entry.is_dir() {
        FS_MODE_DIR
    } else {
        FS_MODE_FILE
    }
}

unsafe fn region_as_mut_slice<'a>(region: MemoryRegion) -> &'a mut [u8] {
    if region.base_address == 0 || region.size == 0 {
        &mut []
    } else {
        unsafe { core::slice::from_raw_parts_mut(region.base_address as *mut u8, region.size) }
    }
}

fn patch_dir_entry(
    dev: *const DeviceObject,
    bpb: &Bpb,
    parent_cluster: u32,
    slot_start: usize,
    slot_count: usize,
    first_cluster: u32,
    size: u64
) -> Result<(), i64> {
    let mut buf = fat::read_chain(dev, bpb, parent_cluster)?;
    // slot_start is the LFN chain's start; the short slot holding
    // size/cluster is the last of the slot_count slots.
    let short_slot_start = slot_start + slot_count.saturating_sub(1) * 32;
    let slot = &mut buf[short_slot_start..short_slot_start + 32];
    slot[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    slot[26..28].copy_from_slice(&((first_cluster & 0xFFFF) as u16).to_le_bytes());
    slot[28..32].copy_from_slice(&(size as u32).to_le_bytes());
    fat::write_chain(dev, bpb, parent_cluster, &buf)
}

// Resolves a path without exposing the symlink-retry out-params
fn resolve_simple(dev: *const DeviceObject, bpb: &Bpb, path: &str, follow_final: bool, hold_lock: bool) -> Result<dir::ResolveOk, i64> {
    let mut symlink_buf = [0u8; 256];
    let mut symlink_len = 0usize;
    let mut remaining_buf = [0u8; 256];
    let mut remaining_len = 0usize;
    dir::resolve(dev, bpb, path, follow_final, hold_lock, &mut symlink_buf, &mut symlink_len, &mut remaining_buf, &mut remaining_len)
}

// Resolves `parent_path` (the directory to act within) and hands back that
// directory's own dir_lock held, via hand-over-hand locking: resolve_simple
// finds it while holding the grandparent's lock, we acquire the parent's own
// lock before releasing the grandparent's -- closing the gap where the
// parent directory could otherwise be deleted between being found and being
// locked for our own use.
fn resolve_and_lock_dir(dev: *const DeviceObject, bpb: &Bpb, parent_path: &str) -> Result<(dir::DirEntryView, kernel_intf::KSyncHandle), i64> {
    let res = resolve_simple(dev, bpb, parent_path, true, true)?;
    if !res.entry.is_dir() {
        if let Some(lock) = res.parent_lock {
            sync_signal_semaphore(lock);
        }
        return Err(E_NOT_DIR);
    }
    let own_lock = sync_state::dir_lock(dev, res.entry.first_cluster);
    sync_wait_semaphore(own_lock);
    if let Some(lock) = res.parent_lock {
        sync_signal_semaphore(lock);
    }
    Ok((res.entry, own_lock))
}

fn create_entry(dev: *const DeviceObject, bpb: &Bpb, path: &str, attr: u8, initial_cluster: u32, initial_size: u32) -> i64 {
    let (parent_path, leaf) = dir::split_parent(path);
    if leaf.is_empty() {
        return E_INVALID;
    }
    let (parent, lock) = match resolve_and_lock_dir(dev, bpb, parent_path) {
        Ok(r) => r,
        Err(e) => return e
    };

    // The whole read -> duplicate-name check -> append is one atomic
    // section under the parent's own lock: if the check ran outside this
    // lock, two concurrent creates of the same new name could both pass it
    // before either writes.
    let mut buf = match fat::read_chain(dev, bpb, parent.first_cluster) {
        Ok(b) => b,
        Err(e) => { sync_signal_semaphore(lock); return e; }
    };
    let entries = dir::decode_dir_entries(&buf);
    if entries.iter().any(|e| e.long_name.eq_ignore_ascii_case(leaf)) {
        sync_signal_semaphore(lock);
        return E_FILE_EXISTS;
    }
    let short_name = dir::unique_short_name(leaf, &entries);
    let slots = dir::encode_entry_slots(leaf, short_name, attr, initial_cluster, initial_size);
    let result = dir::append_entries(dev, bpb, parent.first_cluster, &mut buf, &slots);
    sync_signal_semaphore(lock);
    match result {
        Ok(()) => E_SUCCESS,
        Err(e) => e
    }
}

#[kmod::export]
fn identify_fs(dev: *const DeviceObject) -> bool {
    match get_bpb(dev) {
        Ok(_) => true,
        Err(_) => false
    }
}

#[kmod::export]
fn format(dev: *const DeviceObject) -> i64 {
    match format::do_format(dev) {
        Ok(()) => E_SUCCESS,
        Err(e) => e
    }
}

#[kmod::export]
fn fs_open(
    dev: *const DeviceObject,
    path: StrRef,
    follow_final: bool,
    out_handle: *mut usize,
    out_is_dir: *mut bool,
    out_symlink: MemoryRegion,
    out_symlink_len: *mut usize,
    out_remaining: MemoryRegion,
    out_remaining_len: *mut usize
) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let path_str = unsafe { path.as_str() };
    let symlink_buf = unsafe { region_as_mut_slice(out_symlink) };
    let mut symlink_len = 0usize;
    let remaining_buf = unsafe { region_as_mut_slice(out_remaining) };
    let mut remaining_len = 0usize;

    match dir::resolve(dev, &bpb, path_str, follow_final, true, symlink_buf, &mut symlink_len, remaining_buf, &mut remaining_len) {
        Ok(res) => {
            let is_dir = res.entry.is_dir();
            let kind = if is_dir {
                FileKind::Dir { first_cluster: res.entry.first_cluster }
            } else {
                let shared = sync_state::acquire_open_file(
                    dev, res.parent_cluster, res.entry.slot_start,
                    res.entry.first_cluster, res.entry.size as u64
                );
                FileKind::File { shared }
            };
            if let Some(lock) = res.parent_lock {
                sync_signal_semaphore(lock);
            }
            let handle = Box::new(OpenFile {
                kind,
                parent_cluster: res.parent_cluster,
                slot_start: res.entry.slot_start,
                slot_count: res.entry.slot_count
            });
            let ptr = Box::into_raw(handle) as usize;
            unsafe {
                *out_handle = ptr;
                *out_is_dir = is_dir;
            }
            E_SUCCESS
        },
        Err(e) => {
            if e == E_IS_SYMLINK {
                unsafe {
                    *out_symlink_len = symlink_len;
                    *out_remaining_len = remaining_len;
                }
            }
            e
        }
    }
}

#[kmod::export]
fn fs_close(dev: *const DeviceObject, handle: usize) -> i64 {
    if handle != 0 {
        let h = unsafe { Box::from_raw(handle as *mut OpenFile) };
        if let FileKind::File { .. } = h.kind {
            sync_state::release_open_file(dev, h.parent_cluster, h.slot_start);
        }
        drop(h);
    }
    E_SUCCESS
}

#[kmod::export]
fn fs_read(dev: *const DeviceObject, handle: usize, buf: MemoryRegion, file_offset: usize, out_len: *mut usize) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let h = unsafe { &*(handle as *const OpenFile) };
    let shared = match h.kind {
        FileKind::Dir { .. } => return E_IS_DIR,
        FileKind::File { shared } => shared
    };

    // Read the file's current cluster/size out of the shared, refcounted
    // state every handle to this file points at -- always up to date, since
    // a concurrent write from another handle updates this same struct, not
    // a private per-handle copy.
    let file_sem = unsafe { (*shared).sem };
    sync_wait_semaphore(file_sem);
    let first_cluster = unsafe { (*shared).first_cluster };
    let size = unsafe { (*shared).size };
    let out = unsafe { region_as_mut_slice(buf) };
    let result = file_io::read_file(dev, &bpb, first_cluster, size, file_offset as u64, out);
    sync_signal_semaphore(file_sem);

    match result {
        Ok(n) => {
            unsafe { *out_len = n; }
            E_SUCCESS
        }
        Err(e) => e
    }
}

#[kmod::export]
fn fs_write(
    dev: *const DeviceObject,
    handle: usize,
    buf: MemoryRegion,
    len: usize,
    buf_offset: usize,
    file_offset: usize,
    out_len: *mut usize
) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let h = unsafe { &*(handle as *const OpenFile) };
    let shared = match h.kind {
        FileKind::Dir { .. } => return E_IS_DIR,
        FileKind::File { shared } => shared
    };
    let data = unsafe { core::slice::from_raw_parts((buf.base_address + buf_offset) as *const u8, len) };

    // Guard the whole write with this file's own semaphore: every handle to
    // this file shares the same first_cluster/size, so reading/writing it
    // must be mutually exclusive across handles, not just within one.
    let file_sem = unsafe { (*shared).sem };
    sync_wait_semaphore(file_sem);

    let mut first_cluster = unsafe { (*shared).first_cluster };
    let mut size = unsafe { (*shared).size };
    let write_result = file_io::write_file(dev, &bpb, &mut first_cluster, &mut size, file_offset as u64, data);

    let code = match write_result {
        Ok(n) => {
            let changed = unsafe { first_cluster != (*shared).first_cluster || size != (*shared).size };
            unsafe {
                (*shared).first_cluster = first_cluster;
                (*shared).size = size;
            }
            if changed {
                // Since cluster and size have changed, update the parent directory entry
                // Lock order: File sem -> Dir sem
                let dir_sem = sync_state::dir_lock(dev, h.parent_cluster);
                sync_wait_semaphore(dir_sem);
                let patch_result = patch_dir_entry(dev, &bpb, h.parent_cluster, h.slot_start, h.slot_count, first_cluster, size);
                sync_signal_semaphore(dir_sem);
                match patch_result {
                    Ok(()) => { unsafe { *out_len = n; } E_SUCCESS }
                    Err(e) => e
                }
            } else {
                unsafe { *out_len = n; }
                E_SUCCESS
            }
        }
        Err(e) => e
    };

    sync_signal_semaphore(file_sem);
    code
}

#[kmod::export]
fn fs_stat(
    dev: *const DeviceObject,
    path: StrRef,
    follow_final: bool,
    out: *mut FsFileStat,
    out_symlink: MemoryRegion,
    out_symlink_len: *mut usize,
    out_remaining: MemoryRegion,
    out_remaining_len: *mut usize
) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let path_str = unsafe { path.as_str() };
    let symlink_buf = unsafe { region_as_mut_slice(out_symlink) };
    let mut symlink_len = 0usize;
    let remaining_buf = unsafe { region_as_mut_slice(out_remaining) };
    let mut remaining_len = 0usize;

    match dir::resolve(dev, &bpb, path_str, follow_final, true, symlink_buf, &mut symlink_len, remaining_buf, &mut remaining_len) {
        Ok(res) => {
            let mode = entry_mode(&res.entry);
            unsafe { *out = FsFileStat { size: res.entry.size as u64, mode }; }
            // lstat (follow_final == false) on a symlink itself needs the target
            // text too — fill out_symlink here even though this is a success
            // return, not the chase-needed error path.
            if res.entry.is_symlink() {
                let target_result = file_io::read_symlink_target(dev, &bpb, &res.entry);
                if let Ok(target) = target_result {
                    let n = target.len().min(symlink_buf.len());
                    symlink_buf[..n].copy_from_slice(&target.as_bytes()[..n]);
                    unsafe { *out_symlink_len = n; }
                }
            }
            if let Some(lock) = res.parent_lock {
                sync_signal_semaphore(lock);
            }
            E_SUCCESS
        },
        Err(e) => {
            if e == E_IS_SYMLINK {
                unsafe {
                    *out_symlink_len = symlink_len;
                    *out_remaining_len = remaining_len;
                }
            }
            e
        }
    }
}

#[kmod::export]
fn fs_create_file(dev: *const DeviceObject, path: StrRef, _mode: u16) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let path_str = unsafe { path.as_str() };
    create_entry(dev, &bpb, path_str, ATTR_ARCHIVE, 0, 0)
}

const DOT_NAME: [u8; 11] = [b'.', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' '];
const DOTDOT_NAME: [u8; 11] = [b'.', b'.', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' '];

fn make_dot_entry(name: &[u8; 11], attr: u8, first_cluster: u32) -> [u8; 32] {
    let mut slot = [0u8; 32];
    slot[0..11].copy_from_slice(name);
    slot[11] = attr;
    slot[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    slot[26..28].copy_from_slice(&((first_cluster & 0xFFFF) as u16).to_le_bytes());
    slot
}

#[kmod::export]
fn fs_mkdir(dev: *const DeviceObject, path: StrRef, _mode: u16) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let path_str = unsafe { path.as_str() };
    let (parent_path, leaf) = dir::split_parent(path_str);
    if leaf.is_empty() {
        return E_INVALID;
    }
    let (parent, lock) = match resolve_and_lock_dir(dev, &bpb, parent_path) {
        Ok(r) => r,
        Err(e) => return e
    };

    let mut buf = match fat::read_chain(dev, &bpb, parent.first_cluster) {
        Ok(b) => b,
        Err(e) => { sync_signal_semaphore(lock); return e; }
    };
    let entries = dir::decode_dir_entries(&buf);
    if entries.iter().any(|e| e.long_name.eq_ignore_ascii_case(leaf)) {
        sync_signal_semaphore(lock);
        return E_FILE_EXISTS;
    }

    let new_cluster = match fat::alloc_cluster(dev, &bpb) {
        Ok(c) => c,
        Err(e) => { sync_signal_semaphore(lock); return e; }
    };

    let cluster_size = bpb.cluster_size() as usize;
    let mut new_buf = vec![0u8; cluster_size];
    let dotdot_target = parent.first_cluster;
    new_buf[0..32].copy_from_slice(&make_dot_entry(&DOT_NAME, ATTR_DIRECTORY, new_cluster));
    new_buf[32..64].copy_from_slice(&make_dot_entry(&DOTDOT_NAME, ATTR_DIRECTORY, dotdot_target));
    if let Err(e) = fat::write_chain(dev, &bpb, new_cluster, &new_buf) {
        sync_signal_semaphore(lock);
        return e;
    }

    let short_name = dir::unique_short_name(leaf, &entries);
    let slots = dir::encode_entry_slots(leaf, short_name, ATTR_DIRECTORY, new_cluster, 0);
    let result = dir::append_entries(dev, &bpb, parent.first_cluster, &mut buf, &slots);
    sync_signal_semaphore(lock);
    match result {
        Ok(()) => E_SUCCESS,
        Err(e) => e
    }
}

#[kmod::export]
fn fs_create_symlink(dev: *const DeviceObject, path: StrRef, target: StrRef) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let path_str = unsafe { path.as_str() };
    let target_str = unsafe { target.as_str() };
    let (parent_path, leaf) = dir::split_parent(path_str);
    if leaf.is_empty() {
        return E_INVALID;
    }
    let (parent, lock) = match resolve_and_lock_dir(dev, &bpb, parent_path) {
        Ok(r) => r,
        Err(e) => return e
    };

    let mut buf = match fat::read_chain(dev, &bpb, parent.first_cluster) {
        Ok(b) => b,
        Err(e) => { sync_signal_semaphore(lock); return e; }
    };
    let entries = dir::decode_dir_entries(&buf);
    if entries.iter().any(|e| e.long_name.eq_ignore_ascii_case(leaf)) {
        sync_signal_semaphore(lock);
        return E_FILE_EXISTS;
    }

    // Allocate and write the target text before the entry is ever
    // appended, so first_cluster/size are already correct the moment the
    // entry becomes visible to a concurrent resolver -- no window where a
    // symlink exists with placeholder 0/0 metadata.
    let mut first_cluster = 0u32;
    let mut size = 0u64;
    if let Err(e) = file_io::write_file(dev, &bpb, &mut first_cluster, &mut size, 0, target_str.as_bytes()) {
        sync_signal_semaphore(lock);
        return e;
    }
    debug!("fat32: fs_create_symlink path={} target_len={} first_cluster={} size={}", path_str, target_str.len(), first_cluster, size);

    let short_name = dir::unique_short_name(leaf, &entries);
    let slots = dir::encode_entry_slots(leaf, short_name, ATTR_ARCHIVE | ATTR_SYMLINK, first_cluster, size as u32);
    let result = dir::append_entries(dev, &bpb, parent.first_cluster, &mut buf, &slots);
    sync_signal_semaphore(lock);
    match result {
        Ok(()) => E_SUCCESS,
        Err(e) => e
    }
}

#[kmod::export]
fn fs_delete(dev: *const DeviceObject, path: StrRef) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let path_str = unsafe { path.as_str() };
    let res = match resolve_simple(dev, &bpb, path_str, false, true) {
        Ok(r) => r,
        Err(e) => return e
    };
    // parent_lock is None only for the root ("/") path itself (no containing
    // directory to lock) -- slot_count == 0 the same way -- so this also
    // rejects deleting "/" itself.
    let lock = match res.parent_lock {
        Some(l) => l,
        None => return E_INVALID
    };

    if res.entry.is_dir() {
        // Hand-over-hand: briefly lock the child directory's own content to
        // check emptiness
        let child_lock = sync_state::dir_lock(dev, res.entry.first_cluster);
        sync_wait_semaphore(child_lock);
        let non_dot_result = fat::read_chain(dev, &bpb, res.entry.first_cluster).map(|child_buf| {
            let child_entries = dir::decode_dir_entries(&child_buf);
            child_entries.iter().filter(|e| e.long_name != "." && e.long_name != "..").count()
        });
        sync_signal_semaphore(child_lock);
        match non_dot_result {
            Ok(0) => {}
            Ok(_) => { sync_signal_semaphore(lock); return E_NOT_EMPTY; }
            Err(e) => { sync_signal_semaphore(lock); return e; }
        }
    }

    // Checked while still holding the parent's lock: fs_open registers a
    // file's SharedFileState before releasing this same lock, so this check
    // fully serializes against a concurrent open regardless of the kernel
    // side's own (separately timed) is_path_open bookkeeping.
    if !res.entry.is_dir() && sync_state::is_open(dev, res.parent_cluster, res.entry.slot_start) {
        sync_signal_semaphore(lock);
        return E_FILE_BUSY;
    }

    // mark_deleted + write_chain + free_chain run as one atomic section
    // under the parent's dir_lock
    let mut buf = match fat::read_chain(dev, &bpb, res.parent_cluster) {
        Ok(b) => b,
        Err(e) => { sync_signal_semaphore(lock); return e; }
    };
    dir::mark_deleted(&mut buf, res.entry.slot_start, res.entry.slot_count);
    if let Err(e) = fat::write_chain(dev, &bpb, res.parent_cluster, &buf) {
        sync_signal_semaphore(lock);
        return e;
    }
    let result = if res.entry.first_cluster >= 2 {
        fat::free_chain(dev, &bpb, res.entry.first_cluster)
    } else {
        Ok(())
    };
    sync_signal_semaphore(lock);
    match result {
        Ok(()) => E_SUCCESS,
        Err(e) => e
    }
}

#[kmod::export]
fn fs_rename(dev: *const DeviceObject, from: StrRef, to: StrRef) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let from_str = unsafe { from.as_str() };
    let to_str = unsafe { to.as_str() };

    let (from_parent_path, from_leaf) = dir::split_parent(from_str);
    let (to_parent_path, to_leaf) = dir::split_parent(to_str);
    if from_leaf.is_empty() || to_leaf.is_empty() {
        return E_INVALID;
    }

    // Case-insensitive: paths reaching the module are already canonical (the
    // kernel resolves ".."/"." and chases symlinks before calling in), so two
    // path strings denote the same directory iff they're equal ignoring case.
    if from_parent_path.eq_ignore_ascii_case(to_parent_path) {
        return fs_rename_same_dir(dev, &bpb, from_parent_path, from_leaf, to_leaf);
    }
    fs_rename_cross_dir(dev, &bpb, from_parent_path, from_leaf, to_parent_path, to_leaf)
}

fn fs_rename_same_dir(dev: *const DeviceObject, bpb: &Bpb, parent_path: &str, from_leaf: &str, to_leaf: &str) -> i64 {
    let (parent, lock) = match resolve_and_lock_dir(dev, bpb, parent_path) {
        Ok(r) => r,
        Err(e) => return e
    };
    let mut buf = match fat::read_chain(dev, bpb, parent.first_cluster) {
        Ok(b) => b,
        Err(e) => { sync_signal_semaphore(lock); return e; }
    };
    let entries = dir::decode_dir_entries(&buf);
    let src = match entries.iter().find(|e| e.long_name.eq_ignore_ascii_case(from_leaf)) {
        Some(e) => e.clone(),
        None => { sync_signal_semaphore(lock); return E_NOT_FOUND; }
    };
    if sync_state::is_open(dev, parent.first_cluster, src.slot_start) {
        sync_signal_semaphore(lock);
        return E_FILE_BUSY;
    }
    // Exclude the source's own slot, so a same-name/case-only rename of a
    // file to itself is never rejected as "already exists".
    if entries.iter().any(|e| e.long_name.eq_ignore_ascii_case(to_leaf) && e.slot_start != src.slot_start) {
        sync_signal_semaphore(lock);
        return E_FILE_EXISTS;
    }
    dir::mark_deleted(&mut buf, src.slot_start, src.slot_count);
    let short_name = dir::unique_short_name(to_leaf, &entries);
    let slots = dir::encode_entry_slots(to_leaf, short_name, src.attr, src.first_cluster, src.size);
    let result = dir::append_entries(dev, bpb, parent.first_cluster, &mut buf, &slots);
    sync_signal_semaphore(lock);
    match result {
        Ok(()) => E_SUCCESS,
        Err(e) => e
    }
}

// Longer paths must lock first: Otherwise
// we risk deadlock (if shorter path is prefix
// of longer path and we get lock on shorter path
// first, then traversal of longer path fails 
// since we won't be able to lock the final directory 
// in the shorter path) 
fn path_order_key(path: &str) -> (usize, String) {
    (path.len(), path.to_ascii_lowercase())
}

// Both parent directory locks are held for the
// whole operation (in path_order_key order, see above), so the destination
// is appended before the source is deleted with no window for either a
// data-loss-on-failure bug or a concurrent replace of the source in between.
fn fs_rename_cross_dir(
    dev: *const DeviceObject,
    bpb: &Bpb,
    from_parent_path: &str,
    from_leaf: &str,
    to_parent_path: &str,
    to_leaf: &str
) -> i64 {
    let from_first = path_order_key(from_parent_path) >= path_order_key(to_parent_path);
    let (first_path, second_path) = if from_first {
        (from_parent_path, to_parent_path)
    } else {
        (to_parent_path, from_parent_path)
    };
    let (first_entry, first_lock) = match resolve_and_lock_dir(dev, bpb, first_path) {
        Ok(r) => r,
        Err(e) => return e
    };
    let (second_entry, second_lock) = match resolve_and_lock_dir(dev, bpb, second_path) {
        Ok(r) => r,
        Err(e) => { sync_signal_semaphore(first_lock); return e; }
    };
    let (from_parent, from_lock, to_parent, to_lock) = if from_first {
        (first_entry, first_lock, second_entry, second_lock)
    } else {
        (second_entry, second_lock, first_entry, first_lock)
    };

    let mut from_buf = match fat::read_chain(dev, bpb, from_parent.first_cluster) {
        Ok(b) => b,
        Err(e) => { sync_signal_semaphore(from_lock); sync_signal_semaphore(to_lock); return e; }
    };
    let from_entries = dir::decode_dir_entries(&from_buf);
    let src = match from_entries.iter().find(|e| e.long_name.eq_ignore_ascii_case(from_leaf)) {
        Some(e) => e.clone(),
        None => { sync_signal_semaphore(from_lock); sync_signal_semaphore(to_lock); return E_NOT_FOUND; }
    };
    if sync_state::is_open(dev, from_parent.first_cluster, src.slot_start) {
        sync_signal_semaphore(from_lock);
        sync_signal_semaphore(to_lock);
        return E_FILE_BUSY;
    }

    let mut to_buf = match fat::read_chain(dev, bpb, to_parent.first_cluster) {
        Ok(b) => b,
        Err(e) => { sync_signal_semaphore(from_lock); sync_signal_semaphore(to_lock); return e; }
    };
    let to_entries = dir::decode_dir_entries(&to_buf);
    if to_entries.iter().any(|e| e.long_name.eq_ignore_ascii_case(to_leaf)) {
        sync_signal_semaphore(from_lock);
        sync_signal_semaphore(to_lock);
        return E_FILE_EXISTS;
    }

    // Destination first: if this fails, from_buf hasn't been touched yet, so
    // the source is guaranteed to still be intact.
    let short_name = dir::unique_short_name(to_leaf, &to_entries);
    let slots = dir::encode_entry_slots(to_leaf, short_name, src.attr, src.first_cluster, src.size);
    if let Err(e) = dir::append_entries(dev, bpb, to_parent.first_cluster, &mut to_buf, &slots) {
        sync_signal_semaphore(from_lock);
        sync_signal_semaphore(to_lock);
        return e;
    }

    dir::mark_deleted(&mut from_buf, src.slot_start, src.slot_count);
    let result = fat::write_chain(dev, bpb, from_parent.first_cluster, &from_buf);
    sync_signal_semaphore(from_lock);
    sync_signal_semaphore(to_lock);
    match result {
        Ok(()) => E_SUCCESS,
        Err(e) => e
    }
}

// fs_readdir synthesizes "./.." entries here so listing
// is uniform between root and every other directory.
fn synth_dot_entry(name: &str, size: usize) -> FsDirEntry {
    let mut fs_entry = FsDirEntry::empty();
    let name_bytes = name.as_bytes();
    let n = name_bytes.len().min(fs_entry.name.len());
    fs_entry.name[..n].copy_from_slice(&name_bytes[..n]);
    fs_entry.name_len = n;
    fs_entry.mode = FS_MODE_DIR;
    fs_entry.kind = FS_ENTRY_DIR;
    fs_entry.size = size as u64;
    fs_entry
}

#[kmod::export]
fn fs_readdir(dev: *const DeviceObject, handle: usize, offset: usize, out: *mut FsDirEntry) -> i64 {
    let bpb = match get_bpb(dev) {
        Ok(b) => b,
        Err(e) => return e
    };
    let h = unsafe { &*(handle as *const OpenFile) };
    let dir_cluster = match h.kind {
        FileKind::Dir { first_cluster } => first_cluster,
        FileKind::File { .. } => return E_NOT_DIR
    };

    let is_root = dir_cluster == bpb.root_cluster;
    let lock = sync_state::dir_lock(dev, dir_cluster);
    sync_wait_semaphore(lock);
    let buf = match fat::read_chain(dev, &bpb, dir_cluster) {
        Ok(b) => b,
        Err(e) => { sync_signal_semaphore(lock); return e; }
    };
    
    let entries = dir::decode_dir_entries(&buf);

    if is_root && offset == 0 {
        sync_signal_semaphore(lock);
        unsafe { *out = synth_dot_entry(".", entries.len() * 32); }
        return E_SUCCESS;
    }
    if is_root && offset == 1 {
        sync_signal_semaphore(lock);
        unsafe { *out = synth_dot_entry("..", entries.len() * 32); }
        return E_SUCCESS;
    }

    let real_offset = if is_root { offset - 2 } else { offset };
    let entry = match entries.get(real_offset) {
        Some(e) => e.clone(),
        None => { sync_signal_semaphore(lock); return E_NO_DIR_ENTRIES; }
    };

    let mut fs_entry = FsDirEntry::empty();
    let name_bytes = entry.long_name.as_bytes();
    let n = name_bytes.len().min(fs_entry.name.len());
    fs_entry.name[..n].copy_from_slice(&name_bytes[..n]);
    fs_entry.name_len = n;
    fs_entry.mode = entry_mode(&entry);
    fs_entry.kind = if entry.is_symlink() { FS_ENTRY_SYMLINK } else if entry.is_dir() { FS_ENTRY_DIR } else { FS_ENTRY_FILE };
    fs_entry.size = entry.size as u64;

    if entry.is_symlink() {
        if let Ok(target) = file_io::read_symlink_target(dev, &bpb, &entry) {
            let tb = target.as_bytes();
            let tn = tb.len().min(fs_entry.target.len());
            fs_entry.target[..tn].copy_from_slice(&tb[..tn]);
            fs_entry.target_len = tn;
        }
    }
    sync_signal_semaphore(lock);

    unsafe { *out = fs_entry; }
    E_SUCCESS
}

#[kmod::export]
fn fs_unmount(dev: *const DeviceObject) -> i64 {
    sync_state::teardown_device(dev);
    E_SUCCESS
}
