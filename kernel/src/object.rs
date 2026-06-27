use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use kernel_intf::KError;
use crate::sched::Handle;
use crate::sync::Spinlock;

pub type ObjectOpenFn = fn(&str, u64) -> Result<Handle, KError>;

static TYPE_REGISTRY: Spinlock<BTreeMap<String, ObjectOpenFn>> = Spinlock::new(BTreeMap::new());

pub fn register_object_type(type_name: &str, open_fn: ObjectOpenFn) -> Result<(), KError> {
    let mut registry = TYPE_REGISTRY.lock();
    if registry.contains_key(type_name) {
        return Err(KError::InvalidArgument);
    }
    registry.insert(type_name.to_string(), open_fn);
    Ok(())
}

pub fn open_object(type_name: &str, name: &str, flags: u64) -> Result<Handle, KError> {
    let handler = {
        let registry = TYPE_REGISTRY.lock();
        registry.get(type_name).copied().ok_or(KError::InvalidArgument)?
    };
    handler(name, flags)
}
