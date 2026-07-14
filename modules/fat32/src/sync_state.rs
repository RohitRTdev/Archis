use core::cell::UnsafeCell;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;

use kernel_intf::{
    Lock, KSyncHandle, acquire_spinlock, create_spinlock, release_spinlock,
    sync_create_semaphore, sync_destroy_semaphore
};
use kernel_intf::driver::DeviceObject;

// Shared, refcounted state for every open handle pointing at the same
// on-disk file (identified by where its directory entry currently lives).
// first_cluster changes at most once per file (0 -> a real cluster, on the
// first write to a previously-empty file); size changes on every growing
// write. Both must be shared, not per-handle, so two handles to the same
// file can't silently clobber each other's growth with stale cached copies.
pub struct SharedFileState {
    pub first_cluster: u32,
    pub size: u64,
    refcount: usize,
    pub sem: KSyncHandle
}

struct DeviceState {
    fat_lock: KSyncHandle,
    dir_locks: BTreeMap<u32, KSyncHandle>,
    open_files: BTreeMap<(u32, usize), Box<SharedFileState>>,
    next_free_hint: u32,
    bootstrapped: bool
}

impl DeviceState {
    fn new() -> Self {
        Self {
            fat_lock: sync_create_semaphore(1, 1),
            dir_locks: BTreeMap::new(),
            open_files: BTreeMap::new(),
            next_free_hint: 2,
            bootstrapped: false
        }
    }
}

// Simple convenience wrapper across normal acquire/release lock style
struct GLock<T> {
    lock: UnsafeCell<Lock>,
    data: UnsafeCell<T>
}

unsafe impl<T> Sync for GLock<T> {}

impl<T> GLock<T> {
    const fn new(data: T) -> Self {
        Self {
            lock: UnsafeCell::new(Lock::new()),
            data: UnsafeCell::new(data)
        }
    }

    // Must run exactly once, before any other method -- called from
    // module_init(), which the loader now guarantees runs once at load time.
    fn init(&self) {
        unsafe { create_spinlock(&mut *self.lock.get()) }
    }

    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        unsafe {
            acquire_spinlock(&mut *self.lock.get());
            let r = f(&mut *self.data.get());
            release_spinlock(&mut *self.lock.get());
            r
        }
    }
}

static REGISTRY: GLock<BTreeMap<usize, Box<DeviceState>>> = GLock::new(BTreeMap::new());

pub fn init() {
    REGISTRY.init();
}

fn device_key(dev: *const DeviceObject) -> usize {
    dev as usize
}

fn with_device_state<R>(dev: *const DeviceObject, f: impl FnOnce(&mut DeviceState) -> R) -> R {
    REGISTRY.with(|map| {
        let key = device_key(dev);
        let state = map.entry(key).or_insert_with(|| Box::new(DeviceState::new()));
        f(state)
    })
}

// Guards all FAT-table mutations (alloc_cluster/grow_chain/free_chain/
// cluster_at_index-extend) for this device.
pub fn fat_lock(dev: *const DeviceObject) -> KSyncHandle {
    with_device_state(dev, |s| s.fat_lock)
}

// Cluster to resume alloc_cluster_raw's free-cluster scan from. Callers must
// already hold fat_lock for the duration of both the read and the matching
// write.
pub fn get_free_hint(dev: *const DeviceObject) -> u32 {
    with_device_state(dev, |s| s.next_free_hint)
}

pub fn set_free_hint(dev: *const DeviceObject, cluster: u32) {
    with_device_state(dev, |s| s.next_free_hint = cluster);
}

pub fn take_bootstrap(dev: *const DeviceObject) -> bool {
    with_device_state(dev, |s| {
        let first = !s.bootstrapped;
        s.bootstrapped = true;
        first
    })
}

// Guards one directory's own entry-buffer read-modify-write (create/mkdir/
// delete/rename/patch_dir_entry) and symlink-content reads through it (see
// dir::resolve/fs_stat/fs_readdir) -- created lazily, per cluster.
pub fn dir_lock(dev: *const DeviceObject, cluster: u32) -> KSyncHandle {
    with_device_state(dev, |s| {
        *s.dir_locks.entry(cluster).or_insert_with(|| sync_create_semaphore(1, 1))
    })
}

// Look up-or-create the shared state for (parent_cluster, slot_start),
// bumping its refcount. Called once per fs_open on a file.
pub fn acquire_open_file(
    dev: *const DeviceObject,
    parent_cluster: u32,
    slot_start: usize,
    first_cluster: u32,
    size: u64
) -> *mut SharedFileState {
    with_device_state(dev, |s| {
        let key = (parent_cluster, slot_start);
        if let Some(existing) = s.open_files.get_mut(&key) {
            existing.refcount += 1;
            return &mut **existing as *mut SharedFileState;
        }
        let mut boxed = Box::new(SharedFileState {
            first_cluster,
            size,
            refcount: 1,
            sem: sync_create_semaphore(1, 1)
        });
        let ptr = &mut *boxed as *mut SharedFileState;
        s.open_files.insert(key, boxed);
        ptr
    })
}

// Decrement the refcount for (parent_cluster, slot_start); once it reaches
// 0, remove the entry, destroy its semaphore, and free it. Called once per
// fs_close on a file.
pub fn release_open_file(dev: *const DeviceObject, parent_cluster: u32, slot_start: usize) {
    with_device_state(dev, |s| {
        let key = (parent_cluster, slot_start);
        if let Some(existing) = s.open_files.get_mut(&key) {
            existing.refcount -= 1;
            if existing.refcount == 0 {
                if let Some(boxed) = s.open_files.remove(&key) {
                    sync_destroy_semaphore(boxed.sem);
                }
            }
        }
    });
}

pub fn is_open(dev: *const DeviceObject, parent_cluster: u32, slot_start: usize) -> bool {
    with_device_state(dev, |s| s.open_files.contains_key(&(parent_cluster, slot_start)))
}

// Tears down everything this device's registry holds 
// Destroys fat_lock, every dir_lock, and (defensively; should
// already be empty if unmount's busy-check did its job) any remaining
// open_files semaphores.
pub fn teardown_device(dev: *const DeviceObject) {
    REGISTRY.with(|map| {
        if let Some(state) = map.remove(&device_key(dev)) {
            sync_destroy_semaphore(state.fat_lock);
            for (_, lock) in state.dir_locks {
                sync_destroy_semaphore(lock);
            }
            for (_, shared) in state.open_files {
                sync_destroy_semaphore(shared.sem);
            }
        }
    });
}
