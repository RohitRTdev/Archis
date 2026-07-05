pub const FS_MODE_FILE: u16    = 1 << 0;
pub const FS_MODE_DIR: u16     = 1 << 1;
pub const FS_MODE_SYMLINK: u16 = 1 << 2;

pub const FS_ENTRY_FILE: u8    = 0;
pub const FS_ENTRY_DIR: u8     = 1;
pub const FS_ENTRY_SYMLINK: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FsFileStat {
    pub size: u64,
    pub mode: u16
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FsDirEntry {
    pub name: [u8; 256],
    pub name_len: usize,
    pub kind: u8,
    pub mode: u16,
    pub size: u64,
    pub target: [u8; 256],
    pub target_len: usize
}

impl FsDirEntry {
    pub const fn empty() -> Self {
        Self {
            name: [0; 256],
            name_len: 0,
            kind: FS_ENTRY_FILE,
            mode: 0,
            size: 0,
            target: [0; 256],
            target_len: 0
        }
    }
}
