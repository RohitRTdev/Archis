use alloc::sync::Arc;
use kernel_intf::list::{List, DynList};
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::sync::Spinlock;

pub type KSession      = Arc<Spinlock<Session>,      PoolAllocatorGlobal>;
pub type KProcessGroup = Arc<Spinlock<ProcessGroup>, PoolAllocatorGlobal>;

pub struct Session {
    pub sid:       usize,
    pub leader:    Option<usize>,
    pub processes: DynList<usize>
}

pub struct ProcessGroup {
    pub pgid:      usize,
    pub processes: DynList<usize>
}

impl Session {
    pub fn new(sid: usize) -> KSession {
        Arc::new_in(Spinlock::new(Self {
            sid,
            leader: None,
            processes: List::new()
        }), PoolAllocatorGlobal)
    }
}

impl ProcessGroup {
    pub fn new(pgid: usize) -> KProcessGroup {
        Arc::new_in(Spinlock::new(Self {
            pgid,
            processes: List::new()
        }), PoolAllocatorGlobal)
    }
}
