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

struct LevelInfo {
    driver_loaded: bool,
    devices: Vec<usize>,
    state: LevelState
}

pub struct DeviceStack {
    match_id: String,
    driver_names: Vec<String>,
    inner: Spinlock<StackInner>
}

struct StackInner {
    base_pdos: Vec<usize>,
    levels: Vec<LevelInfo>
}

impl DeviceStack {
    fn new(match_id: String, driver_names: Vec<String>) -> Self {
        let levels = driver_names
            .iter()
            .map(|_| LevelInfo { driver_loaded: false, devices: Vec::new(), state: LevelState::NotStarted })
            .collect();
        Self {
            match_id,
            driver_names,
            inner: Spinlock::new(StackInner { base_pdos: Vec::new(), levels })
        }
    }

    pub fn set_level_state(&self, level: usize, state: LevelState) {
        let mut inner = self.inner.lock();
        if level < inner.levels.len() {
            inner.levels[level].state = state;
        }
    }

    fn mark_loaded(&self, level: usize) {
        self.inner.lock().levels[level].driver_loaded = true;
    }

    fn record_device(&self, level: usize, id: usize) {
        self.inner.lock().levels[level].devices.push(id);
    }

    fn add_base_pdo(&self, id: usize) {
        self.inner.lock().base_pdos.push(id);
    }
}

static STACKS: Once<Spinlock<Vec<Arc<DeviceStack>>>> = Once::new();

fn read_file_to_string(path: &str) -> Option<String> {
    let file = open(path).ok()?;
    let size = file.lock().len();
    let buf = FileBuffer::new(size, false).ok()?;
    if file.lock().read(&buf) != size {
        return None;
    }
    core::str::from_utf8(buf.as_slice()).ok().map(|s| s.to_string())
}

fn parse(text: &str) -> Vec<Arc<DeviceStack>> {
    let mut stacks = Vec::new();
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
                stacks.push(Arc::new(DeviceStack::new(id, core::mem::take(&mut cur_drivers))));
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
        stacks.push(Arc::new(DeviceStack::new(id, cur_drivers)));
    }

    stacks
}

pub fn load_boot_config() {
    STACKS.call_once(|| Spinlock::new(Vec::new()));

    let text = match read_file_to_string(BOOT_CONF_PATH) {
        Some(text) => text,
        None => {
            info!("boot.conf not found or unreadable at {}", BOOT_CONF_PATH);
            return;
        }
    };

    let parsed = parse(&text);
    info!("boot.conf: parsed {} device stack(s)", parsed.len());
    for stack in parsed.iter() {
        info!("  stack id='{}' drivers={:?}", stack.match_id, stack.driver_names);
    }
    *STACKS.get().unwrap().lock() = parsed;
}

fn find_stack(id: &str) -> Option<Arc<DeviceStack>> {
    let stacks = STACKS.get()?.lock();
    stacks.iter().find(|s| s.match_id == id).cloned()
}

pub fn load_root_stacks() {
    let root = root_device();
    let root_stacks: Vec<Arc<DeviceStack>> = match STACKS.get() {
        Some(stacks) => stacks.lock().iter().filter(|s| s.match_id == ROOT_ID).cloned().collect(),
        None => panic!("STACKS is uninitialized??")
    };

    for stack in root_stacks {
        info!("Loading root stack");
        load_stack_instance(&stack, root.clone());
    }
}

// Load one instance of a stack onto base_parent from level 0.
fn load_stack_instance(stack: &Arc<DeviceStack>, base_parent: DeviceHandleK) {
    continue_stack(stack, 0, base_parent);
}

// Resume/load a stack instance from from_level upward on base_parent. Each
// level: load driver, add_device(parent), start the FDO, enumerate it. add is
// serialized on the parent's config semaphore.
fn continue_stack(stack: &Arc<DeviceStack>, from_level: usize, base_parent: DeviceHandleK) {
    let mut parent = base_parent;

    for level in from_level..stack.driver_names.len() {
        let name = &stack.driver_names[level];

        let driver = match load_driver_by_name(name) {
            Ok(driver) => driver,
            Err(e) => {
                info!("stack '{}': could not load driver '{}': {}", stack.match_id, name, e);
                stack.set_level_state(level, LevelState::Failed);
                return;
            }
        };
        stack.mark_loaded(level);

        let before = parent.children_snapshot();
        let added = {
            let _g = parent.config_guard();
            let status = driver_invoke_add(&driver, parent.device_ptr());
            
            if status != Status::Success {
                info!("stack '{}': add_device failed at level {} ('{}')", stack.match_id, level, name);
                stack.set_level_state(level, LevelState::Failed);
                return;
            }
            
            parent.children_added(&before)
        };

        assert!(added.len() == 1);
        let fdo_id = match added.first().copied() {
            Some(id) => id,
            None => {
                info!("stack '{}': driver '{}' created no device at level {}", stack.match_id, name, level);
                stack.set_level_state(level, LevelState::Failed);
                return;
            }
        };

        let fdo = match get_device(fdo_id) {
            Some(fdo) => fdo,
            None => return
        };
        fdo.set_stack(stack.clone(), level);
        stack.record_device(level, fdo_id);

        if fdo.start() != Ok(Status::Success) {
            info!("stack '{}': start failed at level {} ('{}')", stack.match_id, level, name);
            stack.set_level_state(level, LevelState::Failed);
            return;
        }

        // The FDO may itself be a bus — enumerate and detect its children.
        enumerate_and_detect(fdo.clone());

        parent = fdo;
    }

    info!("stack '{}' fully loaded", stack.match_id);
}

// Enumerate a started FDO and reconcile its children against the bus-reported
// current set. Newly appeared PDOs are attached + detected, disappeared ones torn down,
// unchanged ones left alone.
pub fn enumerate_and_detect(fdo: DeviceHandleK) {
    let old = fdo.children_snapshot();

    let irp = {
        let _g = fdo.config_guard();
        match io_request_sync(&fdo, IrpMajor::Configure, IrpMinor::Enumerate, EMPTY_REGION, 0) {
            Ok(irp) => irp,
            Err(e) => {
                info!("enumerate on '{}' failed: {}", fdo.name(), e);
                return;
            }
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

    // irp.buffer = slice of *mut DeviceObject: base_address = ptr, size = count.
    let count = irp.buffer.size;
    let array_ptr = irp.buffer.base_address as *const *mut DeviceObject;
    let mut new_ids: Vec<usize> = Vec::new();
    if !array_ptr.is_null() {
        for i in 0..count {
            let dev_ptr = unsafe { *array_ptr.add(i) };
            if !dev_ptr.is_null() {
                new_ids.push(unsafe { (*dev_ptr).id });
            }
        }
        // Free the driver-allocated array using the matching pool layout.
        if let (Some(nn), Ok(layout)) =
            (NonNull::new(array_ptr as *mut u8), Layout::array::<*mut DeviceObject>(count))
        {
            unsafe { PoolAllocatorGlobal.deallocate(nn, layout); }
        }
    }

    let added: Vec<usize> = new_ids.iter().copied().filter(|id| !old.contains(id)).collect();
    let removed: Vec<usize> = old.iter().copied().filter(|id| !new_ids.contains(id)).collect();
    info!("enumerate on '{}': +{} -{}", fdo.name(), added.len(), removed.len());

    // Tear down disappeared children first.
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

        match find_stack(&match_id) {
            Some(stack) => {
                info!("PDO {} matched stack '{}'", id, match_id);
                pdo.set_stack(stack.clone(), BASE_LEVEL);
                stack.add_base_pdo(id);
                load_stack_instance(&stack, pdo.clone());
            }
            None => info!("PDO {} id '{}' matched no stack", id, match_id)
        }
    }
}

fn query_id(dev: &DeviceHandleK) -> Option<String> {
    let irp = {
        let _g = dev.config_guard();
        io_request_sync(dev, IrpMajor::Configure, IrpMinor::Query, EMPTY_REGION, 0).ok()?
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

pub fn refresh_device_tree() {
    let stacks: Vec<Arc<DeviceStack>> = match STACKS.get() {
        Some(stacks) => stacks.lock().clone(),
        None => panic!("STACKS not initialized??")
    };

    for stack in stacks {
        let level = {
            let inner = stack.inner.lock();
            inner.levels.iter().position(|l| l.state == LevelState::Failed)
        };
        let level = match level {
            Some(level) => level,
            None => continue
        };

        info!("refresh: retrying stack '{}' from level {}", stack.match_id, level);

        let parents: Vec<DeviceHandleK> = if level == 0 {
            if stack.match_id == ROOT_ID {
                let mut v = Vec::new();
                v.push(root_device());
                v
            } else {
                let base = stack.inner.lock().base_pdos.clone();
                base.iter().filter_map(|&id| get_device(id)).collect()
            }
        } else {
            let devs = stack.inner.lock().levels[level - 1].devices.clone();
            devs.iter().filter_map(|&id| get_device(id)).collect()
        };

        stack.set_level_state(level, LevelState::NotStarted);
        for parent in parents {
            continue_stack(&stack, level, parent);
        }
    }
}

#[allow(dead_code)]
pub fn resume_stack(_driver_name: &str) {
    // TODO: scan STACKS for a level whose driver == driver_name and state ==
    // Failed/NotStarted, then continue from the recorded parent device(s).
}
