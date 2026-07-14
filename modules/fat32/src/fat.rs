// FAT32 file-allocation-table access: entry read/write (mirrored across every
// FAT copy), cluster chain walking, and free-cluster allocation.

use alloc::vec;
use kernel_intf::{E_OOM, E_INTERNAL_FAILURE, sync_wait_semaphore, sync_signal_semaphore};
use kernel_intf::driver::DeviceObject;

use crate::bpb::{Bpb, SECTOR_SIZE};
use crate::io_util::{read_sectors, write_sectors};
use crate::sync_state;

pub const EOC: u32 = 0x0FFF_FFFF;
const FREE: u32 = 0;

// Only bottom 28 bits are used for a fat chain entry
const FAT_ENTRY_MASK: u32 = 0x0FFF_FFFF;

pub fn is_eoc(v: u32) -> bool {
    v >= 0x0FFF_FFF8
}

pub fn is_free(v: u32) -> bool {
    v == FREE
}

pub fn cluster_to_lba(bpb: &Bpb, cluster: u32) -> u64 {
    // The first 2 clusters are reserved in FAT32
    bpb.reserved_sector_count as u64
        + bpb.num_fats as u64 * bpb.fat_size_32 as u64
        + (cluster as u64 - 2) * bpb.sectors_per_cluster as u64
}

fn fat_sector_and_offset(bpb: &Bpb, fat_copy: u64, cluster: u32) -> (u64, usize) {
    // Each fat chain entry is 4 bytes and linearly arranged as per clusters
    // So cluster x would be at sector fat_chain_start(in sector) + 4x / SECTOR_SIZE 
    let fat_byte = cluster as u64 * 4;
    let sector = bpb.reserved_sector_count as u64
        + fat_copy * bpb.fat_size_32 as u64
        + fat_byte / SECTOR_SIZE as u64;
    let byte_in_sector = (fat_byte % SECTOR_SIZE as u64) as usize;
    (sector, byte_in_sector)
}

pub fn read_fat_entry(dev: *const DeviceObject, bpb: &Bpb, cluster: u32) -> Result<u32, i64> {
    let (sector, off) = fat_sector_and_offset(bpb, 0, cluster);

    // A fat entry would never straddle 2 sectors
    let mut buf = [0u8; SECTOR_SIZE];
    read_sectors(dev, sector, &mut buf)?;
    Ok(u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) & FAT_ENTRY_MASK)
}

pub fn write_fat_entry(dev: *const DeviceObject, bpb: &Bpb, cluster: u32, value: u32) -> Result<(), i64> {
    // Update the entry in both fat chains
    for copy in 0..bpb.num_fats as u64 {
        let (sector, off) = fat_sector_and_offset(bpb, copy, cluster);
        let mut buf = [0u8; SECTOR_SIZE];
        read_sectors(dev, sector, &mut buf)?;
        let existing = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        let new_val = (value & FAT_ENTRY_MASK) | (existing & !FAT_ENTRY_MASK);
        buf[off..off + 4].copy_from_slice(&new_val.to_le_bytes());
        write_sectors(dev, sector, &buf)?;
    }
    Ok(())
}

// One-time, best-effort estimate of where the free-cluster boundary roughly
// is, via binary search 
fn estimate_free_hint(dev: *const DeviceObject, bpb: &Bpb, total: u32) -> u32 {
    let mut lo = 2u32;
    let mut hi = total + 2;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match read_fat_entry(dev, bpb, mid) {
            Ok(v) if is_free(v) => hi = mid,
            Ok(_) => lo = mid + 1,
            Err(_) => return lo
        }
    }
    lo
}

// Linear scan for a free cluster
fn alloc_cluster_raw(dev: *const DeviceObject, bpb: &Bpb) -> Result<u32, i64> {
    let total = bpb.num_clusters() as u32;

    if sync_state::take_bootstrap(dev) {
        let estimate = estimate_free_hint(dev, bpb, total);
        sync_state::set_free_hint(dev, estimate);
    }

    let hint = sync_state::get_free_hint(dev).clamp(2, total + 2);
    for cluster in (hint..(total + 2)).chain(2..hint) {
        let v = read_fat_entry(dev, bpb, cluster)?;
        if is_free(v) {
            write_fat_entry(dev, bpb, cluster, EOC)?;
            sync_state::set_free_hint(dev, cluster + 1);
            return Ok(cluster);
        }
    }
    Err(E_OOM)
}

// Locked entry point for direct callers (fs_mkdir, file_io::write_file's
// initial allocation) -- the scan-then-claim must be atomic against any
// other concurrent allocation on this device, or two callers can both see
// the same cluster as free and claim it.
pub fn alloc_cluster(dev: *const DeviceObject, bpb: &Bpb) -> Result<u32, i64> {
    let lock = sync_state::fat_lock(dev);
    sync_wait_semaphore(lock);
    let r = alloc_cluster_raw(dev, bpb);
    sync_signal_semaphore(lock);
    r
}

pub fn free_chain(dev: *const DeviceObject, bpb: &Bpb, start_cluster: u32) -> Result<(), i64> {
    if start_cluster < 2 {
        return Ok(());
    }
    let lock = sync_state::fat_lock(dev);
    sync_wait_semaphore(lock);
    let mut cluster = start_cluster;
    let r = loop {
        let next = match read_fat_entry(dev, bpb, cluster) {
            Ok(n) => n,
            Err(e) => break Err(e)
        };
        if let Err(e) = write_fat_entry(dev, bpb, cluster, FREE) {
            break Err(e);
        }
        if is_eoc(next) || is_free(next) {
            break Ok(());
        }
        cluster = next;
    };
    sync_signal_semaphore(lock);
    r
}

// Read a whole cluster chain into one buffer (used for directories, which we
// always fully buffer for simplicity)
pub fn read_chain(dev: *const DeviceObject, bpb: &Bpb, first_cluster: u32) -> Result<alloc::vec::Vec<u8>, i64> {
    let cluster_size = bpb.cluster_size() as usize;
    let mut out = vec![0u8; 0];
    if first_cluster < 2 {
        return Ok(out);
    }
    let mut cluster = first_cluster;
    loop {
        let lba = cluster_to_lba(bpb, cluster);
        let mut buf = vec![0u8; cluster_size];
        read_sectors(dev, lba, &mut buf)?;
        out.extend_from_slice(&buf);
        let next = read_fat_entry(dev, bpb, cluster)?;
        if is_eoc(next) {
            break;
        }
        if is_free(next) {
            return Err(E_INTERNAL_FAILURE);
        }
        cluster = next;
    }
    Ok(out)
}

// Write a buffer whose length is an exact multiple of the chain's total
// cluster capacity back to the same chain 
pub fn write_chain(dev: *const DeviceObject, bpb: &Bpb, first_cluster: u32, data: &[u8]) -> Result<(), i64> {
    let cluster_size = bpb.cluster_size() as usize;
    let mut cluster = first_cluster;
    let mut offset = 0;
    while offset < data.len() {
        let lba = cluster_to_lba(bpb, cluster);
        write_sectors(dev, lba, &data[offset..offset + cluster_size])?;
        offset += cluster_size;
        if offset < data.len() {
            cluster = read_fat_entry(dev, bpb, cluster)?;
        }
    }
    Ok(())
}

// Append one freshly-allocated, zeroed cluster to the end of `first_cluster`'s
// chain, returning the new cluster number. Walk-to-tail-then-extend must be
// one atomic unit against other concurrent extenders of the same chain, so
// the whole thing runs under the device's fat_lock.
pub fn grow_chain(dev: *const DeviceObject, bpb: &Bpb, first_cluster: u32) -> Result<u32, i64> {
    let lock = sync_state::fat_lock(dev);
    sync_wait_semaphore(lock);
    let r = (|| {
        let mut cluster = first_cluster;
        loop {
            let next = read_fat_entry(dev, bpb, cluster)?;
            if is_eoc(next) {
                break;
            }
            cluster = next;
        }
        let new_cluster = alloc_cluster_raw(dev, bpb)?;
        write_fat_entry(dev, bpb, cluster, new_cluster)?;
        let cluster_size = bpb.cluster_size() as usize;
        let zero = vec![0u8; cluster_size];
        write_sectors(dev, cluster_to_lba(bpb, new_cluster), &zero)?;
        Ok(new_cluster)
    })();
    sync_signal_semaphore(lock);
    r
}

// Walk the chain to the cluster holding logical cluster index `target_index`
// (0-based), extending the chain with fresh clusters if it's shorter. A
// pure walk (extend=false) never mutates the FAT table, so it needs no
// lock; extending does, for the same reason grow_chain does.
pub fn cluster_at_index(dev: *const DeviceObject, bpb: &Bpb, first_cluster: u32, target_index: u64, extend: bool) -> Result<u32, i64> {
    if !extend {
        return cluster_at_index_walk(dev, bpb, first_cluster, target_index, false);
    }
    let lock = sync_state::fat_lock(dev);
    sync_wait_semaphore(lock);
    let r = cluster_at_index_walk(dev, bpb, first_cluster, target_index, true);
    sync_signal_semaphore(lock);
    r
}

fn cluster_at_index_walk(dev: *const DeviceObject, bpb: &Bpb, first_cluster: u32, target_index: u64, extend: bool) -> Result<u32, i64> {
    let mut cluster = first_cluster;
    let mut idx = 0u64;
    while idx < target_index {
        let next = read_fat_entry(dev, bpb, cluster)?;
        if is_eoc(next) {
            if !extend {
                return Err(E_INTERNAL_FAILURE);
            }
            let new_cluster = alloc_cluster_raw(dev, bpb)?;

            // Update current cluster entry to point to newly allocated cluster
            write_fat_entry(dev, bpb, cluster, new_cluster)?;
            let cluster_size = bpb.cluster_size() as usize;
            let zero = vec![0u8; cluster_size];

            // Zero extend the new cluster
            write_sectors(dev, cluster_to_lba(bpb, new_cluster), &zero)?;
            cluster = new_cluster;
        } else {
            cluster = next;
        }
        idx += 1;
    }
    Ok(cluster)
}
