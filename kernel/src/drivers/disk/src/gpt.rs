// GPT (GUID Partition Table) encode/decode helpers, plus the protective MBR.
// Pure byte-buffer logic — no device I/O here, that's the caller's job.
//
// Deliberate simplifications vs the full UEFI spec: primary header/entries
// only (no backup GPT at the end of the disk, no repair-from-backup logic).
// GUIDs are treated as plain 16-byte arrays compared byte-for-byte (not the
// UEFI mixed-endian encoding) — fine since we're both writer and reader.

pub const SECTOR_SIZE: usize = 512;
pub const GPT_HEADER_LBA: u64 = 1;
pub const GPT_ENTRIES_LBA: u64 = 2;
pub const GPT_MAX_ENTRIES: usize = 128;
pub const GPT_ENTRY_SIZE: usize = 128;
pub const GPT_ENTRIES_SECTORS: u64 = (GPT_MAX_ENTRIES * GPT_ENTRY_SIZE / SECTOR_SIZE) as u64; // 32
pub const FIRST_USABLE_LBA: u64 = GPT_ENTRIES_LBA + GPT_ENTRIES_SECTORS; // 34

const GPT_SIGNATURE: [u8; 8] = *b"EFI PART";
const GPT_REVISION: u32 = 0x0001_0000;
const GPT_HEADER_SIZE: u32 = 92;

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc ^ 0xFFFF_FFFF
}

#[derive(Clone, Copy)]
pub struct GptHeader {
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub disk_guid: [u8; 16],
    pub partition_entry_lba: u64,
    pub num_partition_entries: u32,
    pub size_of_partition_entry: u32,
    pub partition_entry_array_crc32: u32
}

impl GptHeader {
    pub fn encode(&self, out: &mut [u8; SECTOR_SIZE]) {
        out.fill(0);
        out[0..8].copy_from_slice(&GPT_SIGNATURE);
        out[8..12].copy_from_slice(&GPT_REVISION.to_le_bytes());
        out[12..16].copy_from_slice(&GPT_HEADER_SIZE.to_le_bytes());
        // header_crc32 (16..20) filled in below, after the rest is written
        out[24..32].copy_from_slice(&self.my_lba.to_le_bytes());
        out[32..40].copy_from_slice(&self.alternate_lba.to_le_bytes());
        out[40..48].copy_from_slice(&self.first_usable_lba.to_le_bytes());
        out[48..56].copy_from_slice(&self.last_usable_lba.to_le_bytes());
        out[56..72].copy_from_slice(&self.disk_guid);
        out[72..80].copy_from_slice(&self.partition_entry_lba.to_le_bytes());
        out[80..84].copy_from_slice(&self.num_partition_entries.to_le_bytes());
        out[84..88].copy_from_slice(&self.size_of_partition_entry.to_le_bytes());
        out[88..92].copy_from_slice(&self.partition_entry_array_crc32.to_le_bytes());
        let crc = crc32(&out[0..GPT_HEADER_SIZE as usize]);
        out[16..20].copy_from_slice(&crc.to_le_bytes());
    }

    pub fn decode(bytes: &[u8; SECTOR_SIZE]) -> Option<Self> {
        if bytes[0..8] != GPT_SIGNATURE {
            return None;
        }
        let header_size = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        if header_size as usize > SECTOR_SIZE || (header_size as usize) < 20 {
            return None;
        }
        let stored_crc = u32::from_le_bytes(bytes[16..20].try_into().unwrap());

        let mut check = [0u8; SECTOR_SIZE];
        check[..header_size as usize].copy_from_slice(&bytes[..header_size as usize]);
        check[16..20].copy_from_slice(&0u32.to_le_bytes());
        let calc_crc = crc32(&check[..header_size as usize]);
        if calc_crc != stored_crc {
            return None;
        }

        Some(Self {
            my_lba: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            alternate_lba: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            first_usable_lba: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
            last_usable_lba: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
            disk_guid: bytes[56..72].try_into().unwrap(),
            partition_entry_lba: u64::from_le_bytes(bytes[72..80].try_into().unwrap()),
            num_partition_entries: u32::from_le_bytes(bytes[80..84].try_into().unwrap()),
            size_of_partition_entry: u32::from_le_bytes(bytes[84..88].try_into().unwrap()),
            partition_entry_array_crc32: u32::from_le_bytes(bytes[88..92].try_into().unwrap())
        })
    }
}

#[derive(Clone, Copy)]
pub struct GptPartitionEntry {
    pub part_type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub start_lba: u64,
    // Inclusive, per the GPT spec.
    pub end_lba: u64,
    pub attributes: u64,
    pub name_utf16: [u16; 36]
}

impl GptPartitionEntry {
    pub const SIZE: usize = GPT_ENTRY_SIZE;

    pub fn is_unused(&self) -> bool {
        self.part_type_guid == [0u8; 16]
    }

    // num_lba, as opposed to the on-disk inclusive end_lba.
    pub fn num_lba(&self) -> u64 {
        if self.end_lba < self.start_lba { 0 } else { self.end_lba - self.start_lba + 1 }
    }

    pub fn encode(&self, out: &mut [u8]) {
        debug_assert!(out.len() == Self::SIZE);
        out.fill(0);
        out[0..16].copy_from_slice(&self.part_type_guid);
        out[16..32].copy_from_slice(&self.unique_guid);
        out[32..40].copy_from_slice(&self.start_lba.to_le_bytes());
        out[40..48].copy_from_slice(&self.end_lba.to_le_bytes());
        out[48..56].copy_from_slice(&self.attributes.to_le_bytes());
        for (i, ch) in self.name_utf16.iter().enumerate() {
            out[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&ch.to_le_bytes());
        }
    }

    pub fn decode(bytes: &[u8]) -> Self {
        debug_assert!(bytes.len() == Self::SIZE);
        let mut name_utf16 = [0u16; 36];
        for (i, slot) in name_utf16.iter_mut().enumerate() {
            *slot = u16::from_le_bytes([bytes[56 + i * 2], bytes[56 + i * 2 + 1]]);
        }
        Self {
            part_type_guid: bytes[0..16].try_into().unwrap(),
            unique_guid: bytes[16..32].try_into().unwrap(),
            start_lba: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            end_lba: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
            attributes: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
            name_utf16
        }
    }
}

// Writes the 512-byte protective MBR (LBA0) marking the whole disk as a
// single 0xEE (GPT protective) partition.
pub fn encode_protective_mbr(out: &mut [u8; SECTOR_SIZE], total_lba: u64) {
    out.fill(0);
    const ENTRY_OFF: usize = 446;

    // Not bootable
    out[ENTRY_OFF] = 0x00;

    // CHS
    out[ENTRY_OFF + 1..ENTRY_OFF + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    
    // GPT disk
    out[ENTRY_OFF + 4] = 0xEE;
    
    //CHS
    out[ENTRY_OFF + 5..ENTRY_OFF + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    
    // Starting LBA = 1
    out[ENTRY_OFF + 8..ENTRY_OFF + 12].copy_from_slice(&1u32.to_le_bytes());
    
    let num_sectors = if total_lba - 1 > u32::MAX as u64 { u32::MAX } else { (total_lba - 1) as u32 };
    
    // Number of sectors
    out[ENTRY_OFF + 12..ENTRY_OFF + 16].copy_from_slice(&num_sectors.to_le_bytes());
    
    // MBR signature
    out[510] = 0x55;
    out[511] = 0xAA;
}
