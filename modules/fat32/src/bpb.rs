// FAT32 BIOS Parameter Block (boot sector, LBA0 of the partition).

pub const SECTOR_SIZE: usize = 512;

#[derive(Clone, Copy)]
pub struct Bpb {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sector_count: u16,
    pub num_fats: u8,
    pub fat_size_32: u32,
    pub root_cluster: u32,
    pub fs_info_sector: u16,
    pub total_sectors_32: u32,
    pub volume_id: u32
}

impl Bpb {
    pub fn cluster_size(&self) -> u64 {
        self.bytes_per_sector as u64 * self.sectors_per_cluster as u64
    }

    pub fn num_clusters(&self) -> u64 {
        let fat_area = self.num_fats as u64 * self.fat_size_32 as u64;
        let data_sectors = (self.total_sectors_32 as u64)
            .saturating_sub(self.reserved_sector_count as u64)
            .saturating_sub(fat_area);
        data_sectors / self.sectors_per_cluster as u64
    }

    pub fn decode(sector: &[u8; SECTOR_SIZE]) -> Option<Self> {
        if sector[510] != 0x55 || sector[511] != 0xAA {
            return None;
        }
        let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
        let sectors_per_cluster = sector[13];
        let reserved_sector_count = u16::from_le_bytes([sector[14], sector[15]]);
        let num_fats = sector[16];
        let fat_size_16 = u16::from_le_bytes([sector[22], sector[23]]);
        let total_sectors_32 = u32::from_le_bytes(sector[32..36].try_into().unwrap());
        let fat_size_32 = u32::from_le_bytes(sector[36..40].try_into().unwrap());
        let root_cluster = u32::from_le_bytes(sector[44..48].try_into().unwrap());
        let fs_info_sector = u16::from_le_bytes([sector[48], sector[49]]);
        let fs_type = &sector[82..90];
        let volume_id = u32::from_le_bytes(sector[67..71].try_into().unwrap());

        if bytes_per_sector as usize != SECTOR_SIZE {
            return None;
        }
        if fat_size_16 != 0 || fat_size_32 == 0 {
            return None;
        }
        if num_fats == 0 || sectors_per_cluster == 0 {
            return None;
        }
        if fs_type != b"FAT32   " {
            return None;
        }

        Some(Self {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sector_count,
            num_fats,
            fat_size_32,
            root_cluster,
            fs_info_sector,
            total_sectors_32,
            volume_id
        })
    }

    pub fn encode(&self, sector: &mut [u8; SECTOR_SIZE]) {
        sector.fill(0);
        sector[0] = 0xEB;
        sector[1] = 0x58;
        sector[2] = 0x90;
        sector[3..11].copy_from_slice(b"ARCHIS  ");
        sector[11..13].copy_from_slice(&self.bytes_per_sector.to_le_bytes());
        sector[13] = self.sectors_per_cluster;
        sector[14..16].copy_from_slice(&self.reserved_sector_count.to_le_bytes());
        sector[16] = self.num_fats;
        sector[21] = 0xF8;
        sector[24..26].copy_from_slice(&63u16.to_le_bytes());
        sector[26..28].copy_from_slice(&255u16.to_le_bytes());
        sector[32..36].copy_from_slice(&self.total_sectors_32.to_le_bytes());
        sector[36..40].copy_from_slice(&self.fat_size_32.to_le_bytes());
        sector[44..48].copy_from_slice(&self.root_cluster.to_le_bytes());
        sector[48..50].copy_from_slice(&self.fs_info_sector.to_le_bytes());
        sector[50..52].copy_from_slice(&0u16.to_le_bytes());
        sector[64] = 0x80;
        sector[66] = 0x29;
        sector[67..71].copy_from_slice(&self.volume_id.to_le_bytes());
        let mut label = [b' '; 11];
        label[0..6].copy_from_slice(b"ARCHIS");
        sector[71..82].copy_from_slice(&label);
        sector[82..90].copy_from_slice(b"FAT32   ");
        sector[510] = 0x55;
        sector[511] = 0xAA;
    }
}

pub fn encode_fsinfo(sector: &mut [u8; SECTOR_SIZE]) {
    sector.fill(0);
    sector[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    sector[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    sector[488..492].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    sector[492..496].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    sector[508..512].copy_from_slice(&[0x00, 0x00, 0x55, 0xAA]);
}
