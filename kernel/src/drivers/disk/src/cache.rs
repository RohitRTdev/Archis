use alloc::vec::Vec;

use kernel_intf::{acquire_spinlock, io_send_request, release_spinlock};
use kernel_intf::driver::{IrpMajor, IrpMinor, IrpResult};

use crate::{RawDiskCtx, CacheBlock, SECTOR_SIZE, MAX_CACHE_BLOCKS, write_bytes};

pub fn cache_lookup(ctx: &mut RawDiskCtx, lba: u64) -> Option<[u8; SECTOR_SIZE]> {
    acquire_spinlock(&mut ctx.lock);
    let r = ctx.cache.get_mut(&lba).map(|b| {
        ctx.cache_clock += 1;
        b.seq = ctx.cache_clock;
        b.data
    });
    release_spinlock(&mut ctx.lock);
    r
}

// Insert/overwrite, evicting (and flushing if the victim is dirty) as needed.
// If `dirty` is false (read-fill) and an entry already exists for `lba` and
// is dirty, skip the insert entirely -- the in-cache dirty copy is newer
// than whatever was just read from the device (guards against a stale
// concurrent read clobbering a fresher write).
pub fn cache_put(ctx: &mut RawDiskCtx, lba: u64, data: &[u8; SECTOR_SIZE], dirty: bool) {
    loop {
        acquire_spinlock(&mut ctx.lock);
        if let Some(existing) = ctx.cache.get(&lba) {
            if existing.dirty && !dirty {
                release_spinlock(&mut ctx.lock);
                return;
            }
        } else if ctx.cache.len() >= MAX_CACHE_BLOCKS {
            if let Some((&victim_lba, victim)) = ctx.cache.iter().min_by_key(|(_, b)| b.seq) {
                if victim.dirty {
                    let victim_data = victim.data;
                    release_spinlock(&mut ctx.lock);
                    write_bytes(ctx, victim_lba as usize * SECTOR_SIZE, &victim_data);
                    acquire_spinlock(&mut ctx.lock);
                }
                ctx.cache.remove(&victim_lba);
            }
        }
        ctx.cache_clock += 1;
        let seq = ctx.cache_clock;
        ctx.cache.insert(lba, CacheBlock { data: *data, dirty, seq });
        release_spinlock(&mut ctx.lock);
        return;
    }
}

// Flush every dirty block synchronously. Clears dirty flags on success;
// leaves them dirty (logs) on failure so a later flush can retry.
pub fn flush_all_dirty(ctx: &mut RawDiskCtx) -> bool {
    let dirty: Vec<(u64, [u8; SECTOR_SIZE])> = {
        acquire_spinlock(&mut ctx.lock);
        let v = ctx.cache.iter().filter(|(_, b)| b.dirty).map(|(&l, b)| (l, b.data)).collect();
        release_spinlock(&mut ctx.lock);
        v
    };
    let mut all_ok = true;
    for (lba, data) in dirty {
        if write_bytes(ctx, lba as usize * SECTOR_SIZE, &data) {
            acquire_spinlock(&mut ctx.lock);
            if let Some(b) = ctx.cache.get_mut(&lba) { b.dirty = false; }
            release_spinlock(&mut ctx.lock);
        } else {
            all_ok = false;
        }
    }
    all_ok
}

pub fn cache_invalidate_all(ctx: &mut RawDiskCtx) {
    acquire_spinlock(&mut ctx.lock);
    ctx.cache.clear();
    release_spinlock(&mut ctx.lock);
}

extern "C" fn discard_write_completion(_result: *const IrpResult, _ctx: *mut core::ffi::c_void) {}

pub fn cache_fill_no_evict(ctx: &mut RawDiskCtx, base_lba: u64, buf_addr: usize, num_lba: u64) {
    for i in 0..num_lba {
        let lba = base_lba + i;
        acquire_spinlock(&mut ctx.lock);

        if let Some(existing) = ctx.cache.get(&lba) {
            if existing.dirty {
                release_spinlock(&mut ctx.lock);
                continue; // don't clobber a dirty entry with stale read data
            }
        } else if ctx.cache.len() >= MAX_CACHE_BLOCKS {
            // Not enough space in cache, evict some blocks
            if let Some((&victim_lba, victim)) = ctx.cache.iter().min_by_key(|(_, b)| b.seq) {
                if victim.dirty {
                    let victim_data = victim.data;
                    let child_dev = ctx.child_dev;
                    release_spinlock(&mut ctx.lock);

                    // We do a best effort write here. Any evicted cache lines that are ditry is written back to
                    // disk. However, we don't check if the completion went through or not.
                    io_send_request(
                        child_dev, IrpMajor::Write as usize, IrpMinor::None as usize,
                        victim_data.as_ptr() as usize, SECTOR_SIZE, victim_lba as usize,
                        core::ptr::null(), Some(discard_write_completion), core::ptr::null_mut()
                    );
                    acquire_spinlock(&mut ctx.lock);
                }
                ctx.cache.remove(&victim_lba);
            }
        }

        let mut data = [0u8; SECTOR_SIZE];
        let src = (buf_addr + i as usize * SECTOR_SIZE) as *const u8;
        unsafe { core::ptr::copy_nonoverlapping(src, data.as_mut_ptr(), SECTOR_SIZE); }
        ctx.cache_clock += 1;
        let seq = ctx.cache_clock;
        ctx.cache.insert(lba, CacheBlock { data, dirty: false, seq });
        release_spinlock(&mut ctx.lock);
    }
}
