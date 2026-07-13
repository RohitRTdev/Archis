// Directory entry codec (short 8.3 + LFN chains) and path resolution.
//
// Every new entry always gets a generated `BASE~N.EXT` short alias plus a
// full LFN chain reconstructing the exact original name — simpler and still
// fully spec-valid, at the cost of a little extra space versus only using
// LFN when a name doesn't already fit 8.3.
//
// Directories are always fully buffered in memory for simplicity (read the
// whole cluster chain into one Vec, decode/mutate, write the whole chain
// back) 

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use kernel_intf::{E_IS_SYMLINK, E_NOT_DIR, E_NOT_FOUND, KSyncHandle, sync_wait_semaphore, sync_signal_semaphore};
use kernel_intf::driver::DeviceObject;

use crate::bpb::Bpb;
use crate::fat;
use crate::sync_state;

pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
// Reserved/unused bit in the real FAT spec — repurposed as our own symlink
// marker. Invisible to conformant FAT drivers that don't know about it.
pub const ATTR_SYMLINK: u8 = 0x40;
pub const ATTR_LFN: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;

#[derive(Clone)]
pub struct DirEntryView {
    pub long_name: String,
    pub short_name: [u8; 11],
    pub attr: u8,
    pub first_cluster: u32,
    pub size: u32,
    // Byte offset of the short entry's 32-byte slot within its directory's
    // buffer, and how many 32-byte slots (LFN chain + 1) this entry spans.
    pub slot_start: usize,
    pub slot_count: usize
}

impl DirEntryView {
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIRECTORY != 0
    }

    pub fn is_symlink(&self) -> bool {
        self.attr & ATTR_SYMLINK != 0
    }

    pub fn root(root_cluster: u32) -> Self {
        Self {
            long_name: String::from("/"),
            short_name: [b' '; 11],
            attr: ATTR_DIRECTORY,
            first_cluster: root_cluster,
            size: 0,
            slot_start: 0,
            slot_count: 0
        }
    }
}

fn derive_short_display_name(short: &[u8; 11]) -> String {
    let base = core::str::from_utf8(&short[0..8]).unwrap_or("").trim_end();
    let ext = core::str::from_utf8(&short[8..11]).unwrap_or("").trim_end();
    if ext.is_empty() {
        base.to_string()
    } else {
        format!("{}.{}", base, ext)
    }
}

fn decode_lfn_name(parts: &[(u8, [u16; 13])]) -> String {
    let mut units: Vec<u16> = Vec::new();
    for (_, chunk) in parts.iter().rev() {
        for &u in chunk.iter() {
            if u == 0x0000 {
                break;
            }
            if u == 0xFFFF {
                continue;
            }
            units.push(u);
        }
    }
    String::from_utf16_lossy(&units)
}

pub fn decode_dir_entries(buf: &[u8]) -> Vec<DirEntryView> {
    let mut out = Vec::new();
    let mut lfn_parts: Vec<(u8, [u16; 13])> = Vec::new();
    let mut i = 0usize;
    while i + 32 <= buf.len() {
        // Each directory entry is 32 bytes
        let slot = &buf[i..i + 32];
        let first = slot[0];
        
        // No more directory entries available
        // Stop scanning
        if first == 0x00 {
            break;
        }

        // 0xE5 represents a deleted entry
        if first == 0xE5 {
            lfn_parts.clear();
            i += 32;
            continue;
        }
        let attr = slot[11];
        
        // If long file name, then
        // accumulate more until we hit a 
        // non lfn (short name) entry
        if attr == ATTR_LFN {
            let seq = first;
            let mut chunk = [0u16; 13];
            for k in 0..5 {
                chunk[k] = u16::from_le_bytes([slot[1 + k * 2], slot[2 + k * 2]]);
            }
            // Bytes 11, 12, 13, 26 and 27 are not part of name
            for k in 0..6 {
                chunk[5 + k] = u16::from_le_bytes([slot[14 + k * 2], slot[15 + k * 2]]);
            }
            for k in 0..2 {
                chunk[11 + k] = u16::from_le_bytes([slot[28 + k * 2], slot[29 + k * 2]]);
            }
            lfn_parts.push((seq, chunk));
            i += 32;
            continue;
        }

        // Volume label entry.
        // Not a real file or directory, so it never belongs in a listing.
        if attr & ATTR_VOLUME_ID != 0 {
            lfn_parts.clear();
            i += 32;
            continue;
        }

        let short_name: [u8; 11] = slot[0..11].try_into().unwrap();
        let first_cluster_hi = u16::from_le_bytes([slot[20], slot[21]]) as u32;
        let first_cluster_lo = u16::from_le_bytes([slot[26], slot[27]]) as u32;
        let first_cluster = (first_cluster_hi << 16) | first_cluster_lo;
        let size = u32::from_le_bytes(slot[28..32].try_into().unwrap());

        let long_name = if !lfn_parts.is_empty() {
            decode_lfn_name(&lfn_parts)
        } else {
            derive_short_display_name(&short_name)
        };

        let slot_count = lfn_parts.len() + 1;
        let slot_start = i - lfn_parts.len() * 32;

        out.push(DirEntryView { long_name, short_name, attr, first_cluster, size, slot_start, slot_count });
        lfn_parts.clear();
        i += 32;
    }
    out
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(idx) => (&name[..idx], &name[idx + 1..])
    }
}

pub fn make_short_name(name: &str, tail: u32) -> [u8; 11] {
    let (base_part, ext_part) = split_ext(name);
    let tail_str = format!("~{}", tail);
    let base_budget = 8usize.saturating_sub(tail_str.len()).max(1);

    let mut base = [b' '; 8];
    let mut i = 0usize;
    for c in base_part.chars() {
        if i >= base_budget {
            break;
        }
        let u = c.to_ascii_uppercase();
        if u.is_ascii_alphanumeric() {
            base[i] = u as u8;
            i += 1;
        }
    }
    if i == 0 {
        base[0] = b'_';
        i = 1;
    }
    for (j, b) in tail_str.as_bytes().iter().enumerate() {
        if i + j < 8 {
            base[i + j] = *b;
        }
    }

    let mut ext = [b' '; 3];
    let mut k = 0usize;
    for c in ext_part.chars() {
        if k >= 3 {
            break;
        }
        let u = c.to_ascii_uppercase();
        if u.is_ascii_alphanumeric() {
            ext[k] = u as u8;
            k += 1;
        }
    }

    let mut out = [b' '; 11];
    out[0..8].copy_from_slice(&base);
    out[8..11].copy_from_slice(&ext);
    out
}

pub fn unique_short_name(name: &str, existing: &[DirEntryView]) -> [u8; 11] {
    for tail in 1u32..=999_999 {
        let candidate = make_short_name(name, tail);
        if !existing.iter().any(|e| e.short_name == candidate) {
            return candidate;
        }
    }
    make_short_name(name, 999_999)
}

fn lfn_checksum(short_name: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &b in short_name.iter() {
        sum = (if sum & 1 != 0 { 0x80u8 } else { 0u8 }).wrapping_add(sum >> 1).wrapping_add(b);
    }
    sum
}

fn encode_lfn_entries(name: &str, short_name: &[u8; 11]) -> Vec<[u8; 32]> {
    let utf16: Vec<u16> = name.encode_utf16().collect();
    let checksum = lfn_checksum(short_name);
    let num_entries = ((utf16.len() + 12) / 13).max(1);
    let mut entries = Vec::with_capacity(num_entries);

    for seq in 1..=num_entries {
        let start = (seq - 1) * 13;
        let mut chunk = [0xFFFFu16; 13];
        for j in 0..13 {
            let idx = start + j;
            if idx < utf16.len() {
                chunk[j] = utf16[idx];
            } else if idx == utf16.len() {
                chunk[j] = 0x0000;
            }
        }

        let mut entry = [0u8; 32];
        let mut seq_byte = seq as u8;
        if seq == num_entries {
            seq_byte |= 0x40;
        }
        entry[0] = seq_byte;
        for k in 0..5 {
            entry[1 + k * 2..3 + k * 2].copy_from_slice(&chunk[k].to_le_bytes());
        }
        entry[11] = ATTR_LFN;
        entry[12] = 0;
        entry[13] = checksum;
        for k in 0..6 {
            entry[14 + k * 2..16 + k * 2].copy_from_slice(&chunk[5 + k].to_le_bytes());
        }
        entry[26] = 0;
        entry[27] = 0;
        for k in 0..2 {
            entry[28 + k * 2..30 + k * 2].copy_from_slice(&chunk[11 + k].to_le_bytes());
        }
        entries.push(entry);
    }

    entries.reverse();
    entries
}

pub fn encode_entry_slots(name: &str, short_name: [u8; 11], attr: u8, first_cluster: u32, size: u32) -> Vec<[u8; 32]> {
    // LFN entries need more than one slot. Calculate the required number
    // of slots and what the slots should be
    let mut slots = encode_lfn_entries(name, &short_name);
    let mut short_slot = [0u8; 32];
    short_slot[0..11].copy_from_slice(&short_name);
    short_slot[11] = attr;
    short_slot[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    short_slot[26..28].copy_from_slice(&((first_cluster & 0xFFFF) as u16).to_le_bytes());
    short_slot[28..32].copy_from_slice(&size.to_le_bytes());
    slots.push(short_slot);
    slots
}

pub fn find_end_marker(buf: &[u8]) -> usize {
    let mut i = 0usize;
    while i + 32 <= buf.len() {
        if buf[i] == 0x00 {
            return i;
        }
        i += 32;
    }
    buf.len()
}

// Appends `new_slots` at the directory's end-of-entries marker, growing the
// cluster chain if there isn't room, then writes the whole buffer back.
pub fn append_entries(
    dev: *const DeviceObject,
    bpb: &Bpb,
    first_cluster: u32,
    buf: &mut Vec<u8>,
    new_slots: &[[u8; 32]]
) -> Result<(), i64> {
    let end = find_end_marker(buf);
    let needed = new_slots.len() * 32;
    
    // If we don't have enough space for the new set of entries
    // grow the chain
    while end + needed > buf.len() {
        fat::grow_chain(dev, bpb, first_cluster)?;
        let cluster_size = bpb.cluster_size() as usize;
        buf.extend(vec![0u8; cluster_size]);
    }
    for (k, slot) in new_slots.iter().enumerate() {
        buf[end + k * 32..end + (k + 1) * 32].copy_from_slice(slot);
    }
    fat::write_chain(dev, bpb, first_cluster, buf)
}

pub fn mark_deleted(buf: &mut [u8], slot_start: usize, slot_count: usize) {
    for k in 0..slot_count {
        buf[slot_start + k * 32] = 0xE5;
    }
}

pub struct ResolveOk {
    pub entry: DirEntryView,
    pub parent_cluster: u32,
    pub parent_lock: Option<KSyncHandle>
}

// Walks `path` one component at a time. Any symlink hit that isn't supposed
// to be transparently passed through (an intermediate component, or the
// final one when `follow_final`) aborts resolution with `E_IS_SYMLINK`,
// copies the symlink's target text into `out_symlink`, and copies whatever
// path components came after the symlink component into `out_remaining`
// (empty if the symlink was the final component).
//
// `hold_lock`: if true, the final matched component's dir_lock (guarding its
// containing directory, i.e. parent_cluster) is left held and returned as
// ResolveOk::parent_lock instead of being released before returning
pub fn resolve(
    dev: *const DeviceObject,
    bpb: &Bpb,
    path: &str,
    follow_final: bool,
    hold_lock: bool,
    out_symlink: &mut [u8],
    out_symlink_len: &mut usize,
    out_remaining: &mut [u8],
    out_remaining_len: &mut usize
) -> Result<ResolveOk, i64> {
    let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if comps.is_empty() {
        return Ok(ResolveOk { entry: DirEntryView::root(bpb.root_cluster), parent_cluster: bpb.root_cluster, parent_lock: None });
    }

    let mut cur_cluster = bpb.root_cluster;
    let mut parent_cluster = bpb.root_cluster;

    // What one locked step of the walk found, decided while still holding
    // cur_cluster's dir_lock -- resolved into resolve()'s actual return
    // values only after the lock is released (unless this is the final,
    // hold_lock-requested step).
    enum Step {
        Entry(DirEntryView),
        SymlinkTarget(String)
    }

    for (idx, comp) in comps.iter().enumerate() {
        let is_last = idx == comps.len() - 1;

        // Need to hold the directory lock whilst checking its contents
        let lock = sync_state::dir_lock(dev, cur_cluster);
        sync_wait_semaphore(lock);
        let step: Result<Step, i64> = (|| {
            let buf = fat::read_chain(dev, bpb, cur_cluster)?;
            let entries = decode_dir_entries(&buf);

            // FAT32 does is case insensitive
            let entry = entries
                .iter()
                .find(|e| e.long_name.eq_ignore_ascii_case(comp))
                .cloned()
                .ok_or(E_NOT_FOUND)?;

            if entry.is_symlink() && (!is_last || follow_final) {
                let target = crate::file_io::read_symlink_target(dev, bpb, &entry)?;
                Ok(Step::SymlinkTarget(target))
            } else {
                Ok(Step::Entry(entry))
            }
        })();

        // On the final hop with hold_lock requested and a real (non-symlink)
        // entry found, keep the lock held for the caller instead of
        // releasing here.
        let keep_lock = hold_lock && is_last && matches!(step, Ok(Step::Entry(_)));
        if !keep_lock {
            sync_signal_semaphore(lock);
        }

        match step? {
            Step::SymlinkTarget(target) => {
                let n = target.len().min(out_symlink.len());
                out_symlink[..n].copy_from_slice(&target.as_bytes()[..n]);
                *out_symlink_len = n;

                let remaining = comps[idx + 1..].join("/");
                let m = remaining.len().min(out_remaining.len());
                out_remaining[..m].copy_from_slice(&remaining.as_bytes()[..m]);
                *out_remaining_len = m;

                return Err(E_IS_SYMLINK);
            },
            Step::Entry(entry) => {
                if is_last {
                    return Ok(ResolveOk { entry, parent_cluster: cur_cluster, parent_lock: if keep_lock { Some(lock) } else { None } });
                }

                if !entry.is_dir() {
                    return Err(E_NOT_DIR);
                }
                parent_cluster = cur_cluster;
                cur_cluster = entry.first_cluster;
            }
        }
    }

    let _ = parent_cluster;
    unreachable!()
}

pub fn split_parent(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
        None => ("", trimmed)
    }
}
