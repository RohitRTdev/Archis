#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

use kernel_intf::{
    driver::{
        DeviceObject, DriverObject, Irp, IrpMinor, Status,
        create_device, create_device_by_id,
        ResEntry, ResType, ResTypeDesc, IntDesc, PortDesc,
        MAX_RESOURCE_ENTRIES
    },
    info
};
use kernel_intf::mem::PoolAllocatorGlobal;
use common::MemoryRegion;
use acpi_intf::{
    AcpiHandle, AcpiSimpleResource, AE_OK,
    acpi_enumerate_devices, acpi_get_hid, acpi_get_resources, acpica_init
};
use kmod::dispatch_init;

static IS_SUBSYSTEM_DEV_STARTED: AtomicBool = AtomicBool::new(false);
static IS_SUBSYSTEM_DEV_ADDED: AtomicBool = AtomicBool::new(false);

struct AcpiPdoCtx {
    handle: AcpiHandle,
    hid: [u8; 24],
    hid_len: usize
}

unsafe impl Send for AcpiPdoCtx {}
unsafe impl Sync for AcpiPdoCtx {}

struct AcpiEnumCtx {
    driver_id: usize,
    parent_device: *const DeviceObject,
    buf: *mut *const DeviceObject,
    max_count: usize,
    count: usize
}

unsafe impl Send for AcpiEnumCtx {}

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("Starting driver init for {}", driver.get_name());
    dispatch_init!(driver, dispatch_add, dispatch_pnp);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: &DeviceObject) -> Status {
    if IS_SUBSYSTEM_DEV_ADDED.load(Ordering::Acquire) {
        info!("acpi subsystem already init!");
        return Status::Failed;
    }

    info!("Added device for driver {}", driver.get_name());
    assert!(pdo.id == 0);

    create_device(driver, Some("acpi"), core::ptr::null_mut(), Some(pdo), false);

    IS_SUBSYSTEM_DEV_ADDED.store(true, Ordering::Release);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, req: &mut Irp) -> Status {
    match req.minor_code {
        IrpMinor::Start => do_start(device, req),
        IrpMinor::Enumerate => do_enumerate(device, req),
        IrpMinor::Query => do_query(device, req),
        IrpMinor::Resources => do_resources(device, req),
        IrpMinor::Stop | IrpMinor::Remove => {
            panic!("Attempted to uninit acpi subsystem");
        }
        _ => {
            Status::Unsupported
        }
    }
}

fn do_start(_device: &DeviceObject, req: &mut Irp) -> Status {
    if IS_SUBSYSTEM_DEV_STARTED.load(Ordering::Acquire) {
        info!("acpi subsystem already started!");
        req.complete_irp(Status::Failed);
        return Status::Failed;
    }

    unsafe { acpica_init(); }

    IS_SUBSYSTEM_DEV_STARTED.store(true, Ordering::Release);
    req.complete_irp(Status::Success);
    Status::Success
}

fn do_enumerate(device: &DeviceObject, req: &mut Irp) -> Status {
    let entry_size = size_of::<*const DeviceObject>();
    let max_count = req.buffer.size / entry_size;

    let mut ctx = AcpiEnumCtx {
        driver_id: device.get_driver_id(),
        parent_device: device as *const DeviceObject,
        buf: req.buffer.base_address as *mut *const DeviceObject,
        max_count,
        count: 0
    };

    acpi_enumerate_devices(acpi_pdo_callback, &mut ctx as *mut _ as *mut c_void);

    info!("acpi: enumerated {} device(s)", ctx.count);
    req.bytes_completed = ctx.count * entry_size;
    req.complete_irp(Status::Success);
    Status::Success
}

unsafe extern "C" fn acpi_pdo_callback(
    handle: *mut c_void,
    _nesting_level: u32,
    context: *mut c_void,
    _return_value: *mut *mut c_void
) -> u32 {
    let ctx = unsafe { &mut *(context as *mut AcpiEnumCtx) };
    if ctx.count >= ctx.max_count {
        return AE_OK;
    }

    let mut hid = [0u8; 24];
    let hid_len = acpi_get_hid(handle, &mut hid);
    if hid_len == 0 {
        return AE_OK;
    }

    let hid_str = core::str::from_utf8(&hid[..hid_len]).unwrap_or("?");
    info!("acpi: found device HID={}", hid_str);

    let pdo_ctx = alloc::boxed::Box::new_in(
        AcpiPdoCtx { handle, hid, hid_len },
        PoolAllocatorGlobal
    );
    let pdo_ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(pdo_ctx).0 as *mut c_void;

    let parent = unsafe { &*ctx.parent_device };
    let pdo = create_device_by_id(ctx.driver_id, None, pdo_ctx_ptr, Some(parent), false);

    if pdo.is_null() {
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(
                pdo_ctx_ptr as *mut AcpiPdoCtx,
                PoolAllocatorGlobal
            ));
        }
        return AE_OK;
    }

    unsafe { *ctx.buf.add(ctx.count) = pdo as *const DeviceObject; }
    ctx.count += 1;

    AE_OK
}

fn do_query(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &*(device.ctx as *const AcpiPdoCtx) };
    req.buffer.base_address = ctx.hid.as_ptr() as usize;
    req.buffer.size = ctx.hid_len;
    req.complete_irp(Status::Success);
    Status::Success
}

fn do_resources(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &*(device.ctx as *const AcpiPdoCtx) };
    let res_list = unsafe { req.req_info.res_list };

    let mut simple = [AcpiSimpleResource::default(); MAX_RESOURCE_ENTRIES];
    let count = acpi_get_resources(ctx.handle, simple.as_mut_ptr(), simple.len());

    let fill_count = count.min(res_list.count);
    if fill_count > 0 {
        let entries = unsafe { core::slice::from_raw_parts_mut(res_list.base, fill_count) };
        for (i, s) in simple[..fill_count].iter().enumerate() {
            entries[i] = match s.res_type {
                0 => {
                    info!("Interrupt irq {}", s.address);
                    ResEntry {
                        res_type: ResType::Interrupt,
                        desc: ResTypeDesc { interrupt: IntDesc { irq: s.address as usize, vector: 0 } }
                    }
                }, 
                1 => {
                    info!("Port {} with range {}", s.address, s.length);
                    ResEntry {
                        res_type: ResType::Port,
                        desc: ResTypeDesc { port: PortDesc { base: s.address as usize, range: s.length as usize } }
                    }
                }, 
                _ => {
                    info!("Memory {} with range {}", s.address, s.length);
                    ResEntry {
                        res_type: ResType::Memory,
                        desc: ResTypeDesc { mem: MemoryRegion { base_address: s.address as usize, size: s.length as usize } }
                    }
                }
            };
        }
    }

    req.req_info.res_list.count = fill_count;
    req.complete_irp(Status::Success);
    Status::Success
}

#[kmod::driver_unload]
fn unload(driver: &DriverObject) {
    panic!("Attempted to unload {} driver!", driver.get_name());
}
