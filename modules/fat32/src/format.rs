// Fresh FAT32 filesystem creation on a partition device.

use alloc::vec;

use kernel_intf::driver::DeviceObject;

use crate::bpb::{Bpb, SECTOR_SIZE, encode_fsinfo};
use crate::io_util::{get_disk_info, write_sectors};

const SECTORS_PER_CLUSTER: u8 = 8; // 4 KiB clusters
const RESERVED_SECTOR_COUNT: u16 = 32;
const NUM_FATS: u8 = 2;

pub fn do_format(dev: *const DeviceObject) -> Result<(), i64> {
    let disk_info = get_disk_info(dev)?;
    let lba_count = disk_info.lba_count;

    // Single-pass refine of the FAT size 
    let approx_clusters = lba_count / SECTORS_PER_CLUSTER as u64;
    let mut fat_size_32 = (approx_clusters * 4 + SECTOR_SIZE as u64 - 1) / SECTOR_SIZE as u64;
    let fat_area = NUM_FATS as u64 * fat_size_32;
    let data_sectors = lba_count.saturating_sub(RESERVED_SECTOR_COUNT as u64).saturating_sub(fat_area);
    let num_clusters = data_sectors / SECTORS_PER_CLUSTER as u64;
    fat_size_32 = ((num_clusters + 2) * 4 + SECTOR_SIZE as u64 - 1) / SECTOR_SIZE as u64;

    let bpb = Bpb {
        bytes_per_sector: SECTOR_SIZE as u16,
        sectors_per_cluster: SECTORS_PER_CLUSTER,
        reserved_sector_count: RESERVED_SECTOR_COUNT,
        num_fats: NUM_FATS,
        fat_size_32: fat_size_32 as u32,
        root_cluster: 2,
        fs_info_sector: 1,
        total_sectors_32: lba_count as u32,
        volume_id: 0x4152_4331
    };

    let mut boot_sector = [0u8; SECTOR_SIZE];
    bpb.encode(&mut boot_sector);
    write_sectors(dev, 0, &boot_sector)?;

    let mut fsinfo = [0u8; SECTOR_SIZE];
    encode_fsinfo(&mut fsinfo);
    write_sectors(dev, bpb.fs_info_sector as u64, &fsinfo)?;

    // Zero both FAT copies.
    let zero_sector = [0u8; SECTOR_SIZE];
    for copy in 0..bpb.num_fats as u64 {
        let start = bpb.reserved_sector_count as u64 + copy * bpb.fat_size_32 as u64;
        for s in 0..bpb.fat_size_32 as u64 {
            write_sectors(dev, start + s, &zero_sector)?;
        }
    }

    // FAT[0]/FAT[1] are reserved (media descriptor + EOC marker); FAT[2]
    // (root directory's cluster) is a single-cluster EOC chain.
    crate::fat::write_fat_entry(dev, &bpb, 0, 0x0FFF_FFF8)?;
    crate::fat::write_fat_entry(dev, &bpb, 1, 0x0FFF_FFFF)?;
    crate::fat::write_fat_entry(dev, &bpb, 2, 0x0FFF_FFFF)?;

    // Zero the root directory's single cluster.
    let cluster_size = bpb.cluster_size() as usize;
    let zero_cluster = vec![0u8; cluster_size];
    write_sectors(dev, crate::fat::cluster_to_lba(&bpb, bpb.root_cluster), &zero_cluster)?;

    Ok(())
}
