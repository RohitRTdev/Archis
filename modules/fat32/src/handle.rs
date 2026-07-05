use crate::sync_state::SharedFileState;

pub enum FileKind {
    Dir { first_cluster: u32 },
    File { shared: *mut SharedFileState }
}

pub struct OpenFile {
    pub kind: FileKind,
    pub parent_cluster: u32,
    pub slot_start: usize,
    pub slot_count: usize
}
