// Cluster-chain-based file read/write, seeking by walking (and, for writes,
// extending) the FAT chain to the cluster containing the requested offset.

use alloc::string::String;
use alloc::vec;

use kernel_intf::E_INTERNAL_FAILURE;
use kernel_intf::driver::DeviceObject;

use crate::bpb::Bpb;
use crate::dir::DirEntryView;
use crate::fat;
use crate::io_util::{read_sectors, write_sectors};

pub fn read_file(
    dev: *const DeviceObject,
    bpb: &Bpb,
    first_cluster: u32,
    file_size: u64,
    offset: u64,
    out: &mut [u8]
) -> Result<usize, i64> {
    if first_cluster < 2 || offset >= file_size || out.is_empty() {
        return Ok(0);
    }
    let cluster_size = bpb.cluster_size();
    let to_read = (out.len() as u64).min(file_size - offset) as usize;

    let mut cluster_index = offset / cluster_size;
    let mut cluster = fat::cluster_at_index(dev, bpb, first_cluster, cluster_index, false)?;

    let mut done = 0usize;
    let mut cur_offset = offset;
    while done < to_read {
        let target_index = cur_offset / cluster_size;
        if target_index > cluster_index {
            cluster = fat::cluster_at_index(dev, bpb, cluster, target_index - cluster_index, false)?;
            cluster_index = target_index;
        }
        let in_cluster_off = (cur_offset % cluster_size) as usize;

        let mut cluster_buf = vec![0u8; cluster_size as usize];
        read_sectors(dev, fat::cluster_to_lba(bpb, cluster), &mut cluster_buf)?;

        let chunk = (cluster_size as usize - in_cluster_off).min(to_read - done);
        out[done..done + chunk].copy_from_slice(&cluster_buf[in_cluster_off..in_cluster_off + chunk]);

        done += chunk;
        cur_offset += chunk as u64;
    }
    Ok(done)
}

pub fn write_file(
    dev: *const DeviceObject,
    bpb: &Bpb,
    first_cluster: &mut u32,
    file_size: &mut u64,
    offset: u64,
    data: &[u8]
) -> Result<usize, i64> {
    if data.is_empty() {
        return Ok(0);
    }

    // Could happen if the file is newly created (we set first cluster = 0)
    if *first_cluster < 2 {
        *first_cluster = fat::alloc_cluster(dev, bpb)?;
    }
    let cluster_size = bpb.cluster_size();

    let mut cluster_index = offset / cluster_size;
    let mut cluster = fat::cluster_at_index(dev, bpb, *first_cluster, cluster_index, true)?;

    let mut written = 0usize;
    let mut cur_offset = offset;
    while written < data.len() {
        let target_index = cur_offset / cluster_size;
        if target_index > cluster_index {
            cluster = fat::cluster_at_index(dev, bpb, cluster, target_index - cluster_index, true)?;
            cluster_index = target_index;
        }
        let in_cluster_off = (cur_offset % cluster_size) as usize;

        let mut cluster_buf = vec![0u8; cluster_size as usize];
        read_sectors(dev, fat::cluster_to_lba(bpb, cluster), &mut cluster_buf)?;

        let chunk = (cluster_size as usize - in_cluster_off).min(data.len() - written);
        cluster_buf[in_cluster_off..in_cluster_off + chunk].copy_from_slice(&data[written..written + chunk]);
        write_sectors(dev, fat::cluster_to_lba(bpb, cluster), &cluster_buf)?;

        written += chunk;
        cur_offset += chunk as u64;
    }

    let end_offset = offset + data.len() as u64;
    if end_offset > *file_size {
        *file_size = end_offset;
    }
    Ok(written)
}

pub fn read_symlink_target(dev: *const DeviceObject, bpb: &Bpb, entry: &DirEntryView) -> Result<String, i64> {
    let mut buf = vec![0u8; entry.size as usize];
    if entry.size > 0 {
        let n = read_file(dev, bpb, entry.first_cluster, entry.size as u64, 0, &mut buf)?;
        buf.truncate(n);
    }

    // Symlink stores the path for the actual target it points to
    String::from_utf8(buf).map_err(|_| E_INTERNAL_FAILURE)
}
