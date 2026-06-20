use core::alloc::{Allocator, Layout};
use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use common::{MemoryRegion, StrRef};
use kernel_intf::driver::{DeviceObject, EMPTY_REGION, IrpMajor, IrpMinor, Status};

// Max devices a bus can report from a single Enumerate IRP. The caller
// pre-allocates a buffer this big; the driver writes pointers and sets
// bytes_completed.
const MAX_ENUMERATE_DEVICES: usize = 16;
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LevelState {
    NotStarted,
    Started,
    Stopped,
    Failed
}

struct DriverInformation {
    name: String,
    path: String,
    boot_start: bool
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
static DRIVER_INFO_REGISTRY: Spinlock<Vec<DriverInformation>> = Spinlock::new(Vec::new());

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
    if out.iter().any(|d| d.match_id != ROOT_ID && d.match_id == match_id) {
        panic!("boot.conf: duplicate device stack id '{}' (a PDO must belong to at most one stack)", match_id);
    }
    out.push(Arc::new(DeviceStackDescriptor { match_id, driver_names }));
}

fn parse(text: &str) -> Vec<Arc<DeviceStackDescriptor>> {
    let mut descriptors: Vec<Arc<DeviceStackDescriptor>> = Vec::new();
    let mut cur_id: Option<String> = None;
    let mut cur_drivers: Vec<String> = Vec::new();
    let mut in_block = false;
    let mut parse_description = true;
    let mut driver_name = None;
    let mut driver_path = None;
    let mut driver_boot_start = false;
    let mut driver_info_registry = DRIVER_INFO_REGISTRY.lock();
    let mut driver_names: BTreeSet<String> = BTreeSet::new();

    let reduce_device_stack = |
        cur_id: &mut Option<String>, 
        descriptors: &mut Vec<Arc<DeviceStackDescriptor>>,
        cur_drivers: &mut Vec<String>
    | {
        if let Some(id) = cur_id.take() {
            push_descriptor(descriptors, id, core::mem::take(cur_drivers));
        }
    };

    let mut reduce_description = |
        driver_name: &mut Option<String>,
        driver_path: &mut Option<String>,
        driver_boot_start: bool
    | {
        if driver_name.is_none() || driver_path.is_none() {
            panic!("Driver configuration is missing path/name!");
        }

        let name = driver_name.take().unwrap();
        if driver_names.contains(&name) {
            panic!("Duplicate driver name found while parsing boot.conf -> {}", name);
        }

        driver_info_registry.push(DriverInformation{
            name: name.clone(),
            path: driver_path.take().unwrap(),
            boot_start: driver_boot_start
        });

        driver_names.insert(name);
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line == "[DeviceStack]" {
            if in_block {
                if !parse_description {
                    reduce_device_stack(&mut cur_id, &mut descriptors, &mut cur_drivers);
                }
                else {
                    reduce_description(&mut driver_name, &mut driver_path, driver_boot_start);
                }
            }

            cur_drivers.clear();
            in_block = true;
            parse_description = false;
            continue;
        }
        else if line == "[description]" {
            if in_block {
                if !parse_description {
                    reduce_device_stack(&mut cur_id, &mut descriptors, &mut cur_drivers);
                }
                else {
                    reduce_description(&mut driver_name, &mut driver_path, driver_boot_start);
                }
            }

            in_block = true;
            parse_description = true;

            // If boot_start field is absent we consider default value as false
            driver_boot_start = false;
            continue;
        }

        if !in_block {
            continue;
        }

        if parse_description {
            let segments: Vec<&str> = raw.split("=").map(|e| e.trim()).collect();
            if segments.len() != 2 {
                panic!("Invalid field {} found while parsing boot.conf", raw);
            }

            if segments[0] == "name" {
                driver_name = Some(segments[1].to_string());
            }
            else if segments[0] == "path" {
                driver_path = Some(segments[1].to_string());
            }
            else if segments[0] == "boot_start" {
                if segments[1] != "true" && segments[1] != "false" {
                    panic!("Invalid boot_start value while parsing boot.conf -> {}", raw);
                }

                driver_boot_start = segments[1] == "true";
            }
        }
        else {
            if cur_id.is_none() {
                cur_id = Some(line.to_string());
            } else {
                cur_drivers.push(line.to_string());
            }
        }
    }

    if in_block {
        if !parse_description {
            reduce_device_stack(&mut cur_id, &mut descriptors, &mut cur_drivers);
        }
        else {
            reduce_description(&mut driver_name, &mut driver_path, driver_boot_start);
        }
    }

    descriptors
}

pub fn get_driver_path(name: &str) -> Option<String> {
    DRIVER_INFO_REGISTRY
    .lock()
    .iter()
    .find(|n| {n.name == name})
    .map(|n| {n.path.to_string()})
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

    // Confirm that all boot start drivers can atleast be opened
    DRIVER_INFO_REGISTRY.lock().iter().for_each(|f| {
        if f.boot_start {
            if open(&f.path).is_err() {
                panic!("Unable to load boot start driver {} at path {}!", f.name, f.path);
            }
        }
    });

    for d in parsed.iter() {
        crate::io_log!("  stack id='{}' drivers={:?}", d.match_id, d.driver_names);
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

    info!("Loading root stack");
    for descriptor in root_descriptors {
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
        assert!(!fdo.is_class_device(), "driver bug: add_device created a class device in the PnP stack");
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

    crate::io_log!("stack '{}' fully loaded", stack.match_id());
}

// Parse the bus-written enumerate buffer back into a slice of device pointers.
// Layout: a tightly-packed array of *const DeviceObject starting at
// buf.base_address, with bytes_completed set to count * size_of by the driver.
fn parse_enumerate_buffer(buf: MemoryRegion, bytes_completed: usize)
-> &'static [*const DeviceObject] {
    let entry_size = core::mem::size_of::<*const DeviceObject>();
    if bytes_completed == 0 || buf.base_address == 0 || entry_size == 0 {
        return &[];
    }
    let count = bytes_completed / entry_size;
    unsafe {
        core::slice::from_raw_parts(buf.base_address as *const *const DeviceObject, count)
    }
}

// Enumerate a started FDO and reconcile its children against the bus-reported
// current set. Newly appeared PDOs are attached + detected (each spawns its own
// stack instance), disappeared ones torn down, unchanged ones left alone.
pub fn enumerate_and_detect(fdo: DeviceHandleK) {
    let _g = fdo.config_guard();

    let old = fdo.children_snapshot();

    // Allocate the buffer required for driver to fill in the new device list
    let entry_size = core::mem::size_of::<*const DeviceObject>();
    let buf_size = MAX_ENUMERATE_DEVICES * entry_size;
    let layout = Layout::from_size_align(buf_size, entry_size).unwrap();
    let buf_ptr = match PoolAllocatorGlobal.allocate(layout) {
        Ok(p) => p.cast::<u8>(),
        Err(_) => {
            info!("enumerate on '{}': failed to allocate result buffer", fdo.name());
            return;
        }
    };
    let buffer = MemoryRegion { base_address: buf_ptr.as_ptr() as usize, size: buf_size };

    let irp = match io_request_sync(&fdo, IrpMajor::Pnp, IrpMinor::Enumerate, buffer, 0, None, false) {
        Ok(irp) => irp,
        Err(e) => {
            info!("enumerate on '{}' failed: {}", fdo.name(), e);
            unsafe { PoolAllocatorGlobal.deallocate(buf_ptr, layout); }
            return;
        }
    };

    if irp.status == Status::Unsupported {
        // Not a bus — nothing to enumerate.
        unsafe { PoolAllocatorGlobal.deallocate(buf_ptr, layout); }
        return;
    }
    if irp.status != Status::Success {
        info!("enumerate on '{}' returned status {}", fdo.name(), irp.status as isize);
        unsafe { PoolAllocatorGlobal.deallocate(buf_ptr, layout); }
        return;
    }

    let new_ids: Vec<usize> = parse_enumerate_buffer(irp.buffer, irp.bytes_completed)
        .iter()
        .map(|&dev| unsafe { (*dev).id })
        .collect();
    unsafe { PoolAllocatorGlobal.deallocate(buf_ptr, layout); }

    let added: Vec<usize> = new_ids.iter().copied().filter(|id| !old.contains(id)).collect();
    let removed: Vec<usize> = old.iter().copied().filter(|id| !new_ids.contains(id)).collect();
    crate::io_log!("enumerate on '{}': +{} -{}", fdo.name(), added.len(), removed.len());

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
        assert!(!pdo.is_class_device(), "driver bug: enumerate returned a class device as a PDO");
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
                crate::io_log!("PDO {} matched stack '{}'", id, match_id);
                // Each PDO is the base of its own fresh instance.
                start_stack_instance(descriptor, pdo.clone(), true);
            }
            None => { crate::io_log!("PDO {} id '{}' matched no stack", id, match_id); }
        }
    }
}

fn query_id(dev: &DeviceHandleK) -> Option<String> {
    let irp = {
        let _g = dev.config_guard();
        io_request_sync(dev, IrpMajor::Pnp, IrpMinor::Query, EMPTY_REGION, 0, None, false).ok()?
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

pub fn do_refresh_device_tree() {
    let instances: Vec<Arc<DeviceStack>> = match STACK_INSTANCES.get() {
        Some(list) => list.lock().clone(),
        None => panic!("STACK_INSTANCES not initialized??")
    };

    for stack in instances {
        if let Some(level) = stack.first_incomplete_level() {
            crate::io_log!("refresh: retrying stack '{}' from level {}", stack.match_id(), level);
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
