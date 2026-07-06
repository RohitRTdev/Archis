mod system;
mod process;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use kernel_intf::KError;
use crate::sync::Spinlock;

pub type IntfHandlerFn = fn(*mut u8) -> Result<(), KError>;

struct IntfEntry {
    handler: IntfHandlerFn,
    len: usize
}

static INTF_REGISTRY: Spinlock<BTreeMap<String, IntfEntry>> = Spinlock::new(BTreeMap::new());

pub fn register_intf(name: &str, handler: IntfHandlerFn, len: usize) -> Result<(), KError> {
    let mut registry = INTF_REGISTRY.lock();
    if registry.contains_key(name) {
        return Err(KError::InvalidArgument);
    }
    registry.insert(name.to_string(), IntfEntry { handler, len });
    Ok(())
}

pub fn lookup_intf(name: &str) -> Option<(IntfHandlerFn, usize)> {
    let registry = INTF_REGISTRY.lock();
    registry.get(name).map(|e| (e.handler, e.len))
}

pub fn init() {
    system::init();
    process::init();
}
