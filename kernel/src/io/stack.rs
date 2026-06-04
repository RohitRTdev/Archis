use core::alloc::{Allocator, Layout};
use core::ptr::NonNull;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use common::{MemoryRegion, StrRef};
use kernel_intf::driver::{DeviceObject, IrpMajor, IrpMinor, Status};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::info;

use crate::fs::{FileBuffer, open};
use crate::sync::{Once, Spinlock};

use super::driver::{
    DeviceHandleK, driver_invoke_add, get_device, io_request_sync, load_driver_by_name, remove_device,
    root_device
};

const BOOT_CONF_PATH: &str = "/sys/drivers/boot.conf";
const ROOT_ID: &str = "Root";
const BASE_LEVEL: usize = usize::MAX;
const EMPTY_REGION: MemoryRegion = MemoryRegion { base_address: 0, size: 0 };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LevelState {
    NotStarted,
    Started,
    Stopped,
    Failed
}

// Immutable template parsed from boot.conf: one per [DeviceStack] block. Tells us
// which drivers to load (bottom -> top) for a given match id. Shared 
// across every live instance that matches the same id.
struct DeviceStackDescriptor {
    match_id: String,
    driver_names: Vec<String>
}

struct LevelInfo {
    // The single device occupying this level (None until created / after teardown).
    device: Option<usize>,
    state: LevelState
}

// A single live instance of a stack, created on demand for exactly one base PDO.
pub struct DeviceStack {
    descriptor: Arc<DeviceStackDescriptor>,
    inner: Spinlock<StackInstance>
}

struct StackInstance {
    // The single base parent device id (a matched PDO, or the ROOT device for Root stacks).
    base_pdo: usize,
    levels: Vec<LevelInfo>
}

impl DeviceStack {
    fn new(descriptor: Arc<DeviceStackDescriptor>, base_pdo: usize) -> Self {
        let levels = descriptor
            .driver_names
            .iter()
            .map(|_| LevelInfo { device: None, state: LevelState::NotStarted })
            .collect();
        Self {
            descriptor,
            inner: Spinlock::new(StackInstance { base_pdo, levels })
        }
    }

    fn match_id(&self) -> &str {
        &self.descriptor.match_id
    }

    fn driver_names(&self) -> &[String] {
        &self.descriptor.driver_names
    }

    fn base_pdo(&self) -> usize {
        self.inner.lock().base_pdo
    }

    // Called from io::driver via the device's stored (stack, level) tuple. A PDO
    // carries BASE_LEVEL (== usize::MAX), which is out of range and thus a no-op.
    pub fn set_level_state(&self, level: usize, state: LevelState) {
        let mut inner = self.inner.lock();
        if level < inner.levels.len() {
            inner.levels[level].state = state;
        }
    }

    fn set_level_device(&self, level: usize, id: usize) {
        let mut inner = self.inner.lock();
        if level < inner.levels.len() {
            inner.levels[level].device = Some(id);
        }
    }

    fn level_device(&self, level: usize) -> Option<usize> {
        self.inner.lock().levels.get(level).and_then(|l| l.device)
    }

    // Reset a level after its device is torn down: drop the device id and mark it
    // NotStarted so a later refresh re-runs add_device for this level.
    fn clear_level(&self, level: usize) {
        let mut inner = self.inner.lock();
        if level < inner.levels.len() {
            inner.levels[level].device = None;
            inner.levels[level].state = LevelState::NotStarted;
        }
    }

    // Lowest level needing (re)work: Failed (load/start error) or NotStarted
    // (cleared by a removal). Started and Stopped levels are intentionally skipped
    // so a deliberately stopped stack is never auto-restarted.
    fn first_incomplete_level(&self) -> Option<usize> {
        self.inner
            .lock()
            .levels
            .iter()
            .position(|l| matches!(l.state, LevelState::Failed | LevelState::NotStarted))
    }
}

static DESCRIPTORS: Once<Spinlock<Vec<Arc<DeviceStackDescriptor>>>> = Once::new();
static STACK_INSTANCES: Once<Spinlock<Vec<Arc<DeviceStack>>>> = Once::new();

fn read_file_to_string(path: &str) -> Option<String> {
    let file = open(path).ok()?;
    let size = file.lock().len();
    let buf = FileBuffer::new(size, false).ok()?;
    if file.lock().read(&buf) != size {
        return None;
    }
    core::str::from_utf8(buf.as_slice()).ok().map(|s| s.to_string())
}

fn push_descriptor(out: &mut Vec<Arc<DeviceStackDescriptor>>, match_id: String, driver_names: Vec<String>) {
    if out.iter().any(|d| d.match_id == match_id) {
        panic!("boot.conf: duplicate device stack id '{}' (a PDO must belong to at most one stack)", match_id);
    }
    out.push(Arc::new(DeviceStackDescriptor { match_id, driver_names }));
}

fn parse(text: &str) -> Vec<Arc<DeviceStackDescriptor>> {
    let mut descriptors: Vec<Arc<DeviceStackDescriptor>> = Vec::new();
    let mut cur_id: Option<String> = None;
    let mut cur_drivers: Vec<String> = Vec::new();
    let mut in_block = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line == "[DeviceStack]" {
            if let Some(id) = cur_id.take() {
                push_descriptor(&mut descriptors, id, core::mem::take(&mut cur_drivers));
            }
            cur_drivers.clear();
            in_block = true;
            continue;
        }

        if !in_block {
            continue;
        }

        if cur_id.is_none() {
            cur_id = Some(line.to_string());
        } else {
            cur_drivers.push(line.to_string());
        }
    }

    if let Some(id) = cur_id.take() {
        push_descriptor(&mut descriptors, id, cur_drivers);
    }

    descriptors
}

pub fn load_boot_config() {
    DESCRIPTORS.call_once(|| Spinlock::new(Vec::new()));
    STACK_INSTANCES.call_once(|| Spinlock::new(Vec::new()));

    let text = match read_file_to_string(BOOT_CONF_PATH) {
        Some(text) => text,
        None => {
            info!("boot.conf not found or unreadable at {}", BOOT_CONF_PATH);
            return;
        }
    };

    let parsed = parse(&text);
    info!("boot.conf: parsed {} device stack descriptor(s)", parsed.len());
    for d in parsed.iter() {
        info!("  stack id='{}' drivers={:?}", d.match_id, d.driver_names);
    }
    *DESCRIPTORS.get().unwrap().lock() = parsed;
}

fn find_descriptor(id: &str) -> Option<Arc<DeviceStackDescriptor>> {
    let descriptors = DESCRIPTORS.get()?.lock();
    descriptors.iter().find(|d| d.match_id == id).cloned()
}

fn register_instance(stack: &Arc<DeviceStack>) {
    STACK_INSTANCES.get().expect("STACK_INSTANCES not initialized").lock().push(stack.clone());
}

fn remove_instance(stack: &Arc<DeviceStack>) {
    if let Some(list) = STACK_INSTANCES.get() {
        list.lock().retain(|s| !Arc::ptr_eq(s, stack));
    }
}

pub fn load_root_stacks() {
    let root = root_device();
    let root_descriptors: Vec<Arc<DeviceStackDescriptor>> = match DESCRIPTORS.get() {
        Some(d) => d.lock().iter().filter(|d| d.match_id == ROOT_ID).cloned().collect(),
        None => panic!("DESCRIPTORS is uninitialized??")
    };

    for descriptor in root_descriptors {
        info!("Loading root stack");
        // The ROOT device is never attached to a stack (it is never removed).
        start_stack_instance(descriptor, root.clone(), false);
    }
}

// Create a fresh per-base-PDO stack instance, register it, and bring it up from level 0 
fn start_stack_instance(descriptor: Arc<DeviceStackDescriptor>, base: DeviceHandleK, attach_base: bool) {
    let stack = Arc::new(DeviceStack::new(descriptor, base.id()));
    if attach_base {
        base.set_stack(stack.clone(), BASE_LEVEL);
    }
    register_instance(&stack);
    continue_stack(&stack, 0);
}

fn parent_for_level(stack: &Arc<DeviceStack>, level: usize) -> Option<DeviceHandleK> {
    if level == 0 {
        get_device(stack.base_pdo())
    } else {
        stack.level_device(level - 1).and_then(get_device)
    }
}

// Bring a single stack instance up from from_level. The parent of from_level is the
// base PDO (level 0) or the device recorded one level below. Each level: load the
// driver, add_device on the parent (serialized on the parent's config semaphore),
// expect exactly one FDO, record + start it, then enumerate it (it may be a bus).
fn continue_stack(stack: &Arc<DeviceStack>, from_level: usize) {
    let names_len = stack.driver_names().len();

    let mut parent = match parent_for_level(stack, from_level) {
        Some(parent) => parent,
        None => {
            info!("stack '{}': no parent device for level {}", stack.match_id(), from_level);
            stack.set_level_state(from_level, LevelState::Failed);
            return;
        }
    };

    for level in from_level..names_len {
        // Drop any device left over from a prior failed attempt at this level so we
        // don't create a duplicate FDO when retrying.
        if let Some(stale) = stack.level_device(level) {
            assert!(false, "Found stale fdo device!");
            if let Some(dev) = get_device(stale) {
                remove_device(&dev);
            }
            stack.clear_level(level);
        }

        let name = stack.driver_names()[level].clone();

        let driver = match load_driver_by_name(&name) {
            Ok(driver) => driver,
            Err(e) => {
                info!("stack '{}': could not load driver '{}': {}", stack.match_id(), name, e);
                stack.set_level_state(level, LevelState::Failed);
                return;
            }
        };

        let before = parent.children_snapshot();
        let added = {
            let _g = parent.config_guard();
            let status = driver_invoke_add(&driver, parent.device_ptr());

            if status != Status::Success {
                info!("stack '{}': add_device failed at level {} ('{}')", stack.match_id(), level, name);
                stack.set_level_state(level, LevelState::Failed);
                return;
            }

            parent.children_added(&before)
        };

        // A well-behaved add_device creates exactly one FDO under the parent.
        // Anything else is a driver bug
        if added.len() != 1 {
            info!(
                "stack '{}': add_device for '{}' created {} devices at level {} (expected 1)",
                stack.match_id(), name, added.len(), level
            );
            stack.set_level_state(level, LevelState::Failed);
            return;
        }
        let fdo_id = added[0];

        let fdo = match get_device(fdo_id) {
            Some(fdo) => fdo,
            None => return
        };
        fdo.set_stack(stack.clone(), level);
        stack.set_level_device(level, fdo_id);

        if fdo.start() != Ok(Status::Success) {
            info!("stack '{}': start failed at level {} ('{}')", stack.match_id(), level, name);
            stack.set_level_state(level, LevelState::Failed);
            return;
        }

        // The FDO may itself be a bus — enumerate and detect its children.
        enumerate_and_detect(fdo.clone());

        parent = fdo;
    }

    info!("stack '{}' fully loaded", stack.match_id());
}

// Enumerate a started FDO and reconcile its children against the bus-reported
// current set. Newly appeared PDOs are attached + detected (each spawns its own
// stack instance), disappeared ones torn down, unchanged ones left alone.
pub fn enumerate_and_detect(fdo: DeviceHandleK) {
    let _g = fdo.config_guard();

    let old = fdo.children_snapshot();
    let irp = match io_request_sync(&fdo, IrpMajor::Configure, IrpMinor::Enumerate, None, EMPTY_REGION, 0) {
        Ok(irp) => irp,
        Err(e) => {
            info!("enumerate on '{}' failed: {}", fdo.name(), e);
            return;
        }
    };

    if irp.status == Status::Unsupported {
        // Not a bus — nothing to enumerate.
        return;
    }
    if irp.status != Status::Success {
        info!("enumerate on '{}' returned status {}", fdo.name(), irp.status as isize);
        return;
    }

    let mut new_ids: Vec<usize> = Vec::new();
    if let Some(req) = irp.req_params {
        let array = unsafe { req.enumerate };
        array.iter().for_each(|&dev| {
            new_ids.push(unsafe{ (*dev).id });
        });

        // Free the driver-allocated array using the matching pool layout.
        let nn = NonNull::new(array.as_ptr() as *mut u8).unwrap();  
        let layout = Layout::array::<*mut DeviceObject>(array.len()).unwrap();
        unsafe { PoolAllocatorGlobal.deallocate(nn, layout); }
    }

    let added: Vec<usize> = new_ids.iter().copied().filter(|id| !old.contains(id)).collect();
    let removed: Vec<usize> = old.iter().copied().filter(|id| !new_ids.contains(id)).collect();
    info!("enumerate on '{}': +{} -{}", fdo.name(), added.len(), removed.len());

    // Tear down disappeared children first
    for id in removed {
        if let Some(dev) = get_device(id) {
            remove_device(&dev);
        }
    }

    // Attach + detect newly appeared PDOs.
    for id in added {
        let pdo = match get_device(id) {
            Some(pdo) => pdo,
            None => continue
        };
        fdo.attach_child(id);
        pdo.mark_started_pdo();

        let match_id = match query_id(&pdo) {
            Some(m) => m,
            None => {
                info!("PDO {} returned no id; skipping", id);
                continue;
            }
        };

        match find_descriptor(&match_id) {
            Some(descriptor) => {
                info!("PDO {} matched stack '{}'", id, match_id);
                // Each PDO is the base of its own fresh instance.
                start_stack_instance(descriptor, pdo.clone(), true);
            }
            None => info!("PDO {} id '{}' matched no stack", id, match_id)
        }
    }
}

fn query_id(dev: &DeviceHandleK) -> Option<String> {
    let irp = {
        let _g = dev.config_guard();
        io_request_sync(dev, IrpMajor::Configure, IrpMinor::Query, None, EMPTY_REGION, 0).ok()?
    };
    if irp.status != Status::Success {
        return None;
    }

    let sref = StrRef { ptr: irp.buffer.base_address as *const u8, len: irp.buffer.size };
    if sref.ptr.is_null() || sref.len == 0 {
        return None;
    }
    Some(unsafe { sref.as_str() }.to_string())
}

// Retry every live stack instance that has an incomplete level (failed to load or
// cleared by a removal), continuing from its lowest such level.
pub fn refresh_device_tree() {
    let instances: Vec<Arc<DeviceStack>> = match STACK_INSTANCES.get() {
        Some(list) => list.lock().clone(),
        None => panic!("STACK_INSTANCES not initialized??")
    };

    for stack in instances {
        if let Some(level) = stack.first_incomplete_level() {
            info!("refresh: retrying stack '{}' from level {}", stack.match_id(), level);
            continue_stack(&stack, level);
        }
    }
}

// Called from io::driver::remove_device when a device that belongs to a stack is
// torn down. A base PDO leaving means the whole instance is gone; a numbered FDO
// level leaving just resets that level so a later refresh re-runs add_device.
pub fn on_device_removed(stack: &Arc<DeviceStack>, level: usize) {
    if level == BASE_LEVEL {
        remove_instance(stack);
    } else {
        stack.clear_level(level);
    }
}
