#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::alloc::{Allocator, Layout};
use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};

use kernel_intf::{exported_function, info, io_complete_irp};
use kernel_intf::driver::{
    DeviceObject, DriverObject, Irp, IrpMinor, ReqInfo, Status, create_device, create_device_by_id
};
use kernel_intf::mem::PoolAllocatorGlobal;
use common::MemoryRegion;

const CHILD_ID: &str = "test1-child";
const MAX_PDOS: usize = 4;

struct Test1Device {
    pdos: [*mut DeviceObject; MAX_PDOS],
    pdo_count: usize,
    generation: u32
}

static PENDING_IRP: AtomicPtr<Irp> = AtomicPtr::new(core::ptr::null_mut());

#[kmod::init]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing (id={})...", driver.get_name(), driver.id);

    kmod::dispatch_init!(
        driver,
        dispatch_add,
        dispatch_pnp,
        dispatch_read,
        dispatch_write
    );

    unsafe { exported_function(); test2::get_test2(); }
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: Option<&DeviceObject>) -> Status {
    info!("test1 add_device: creating bus FDO");

    let ctx = alloc::boxed::Box::new_in(
        Test1Device { pdos: [core::ptr::null_mut(); MAX_PDOS], pdo_count: 0, generation: 0 },
        PoolAllocatorGlobal
    );
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;

    let dev = create_device(driver, Some("test1"), ctx_ptr, pdo);
    if dev.is_null() {
        info!("test1 add_device: create_device failed");
        return Status::Failed;
    }
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, request: &mut Irp) -> Status {
    match request.minor_code {
        IrpMinor::Enumerate => enumerate(device, request),
        IrpMinor::Query => {
            request.buffer = MemoryRegion {
                base_address: CHILD_ID.as_ptr() as usize,
                size: CHILD_ID.len()
            };
            request.complete_irp(Status::Success);
            Status::Success
        },
        IrpMinor::Start => {
            dispatch_start(device, request)
        },
        IrpMinor::Stop => {
            dispatch_stop(device, request)
        },
        IrpMinor::Remove => {
            dispatch_remove(device, request)
        },
        IrpMinor::None => Status::Unsupported
    }
}

fn enumerate(device: &DeviceObject, request: &mut Irp) -> Status {
    let state = unsafe { &mut *(device.ctx as *mut Test1Device) };
    let driver_id = device.get_driver_id();
    state.generation += 1;

    if state.generation == 1 {
        // First enumeration: two fresh PDOs.
        for _ in 0..2 {
            let pdo = create_device_by_id(driver_id, None, core::ptr::null_mut(), None);
            state.pdos[state.pdo_count] = pdo;
            state.pdo_count += 1;
        }
    } else if state.generation == 2 {
        let keep = state.pdos[1];
        let fresh = create_device_by_id(driver_id, None, core::ptr::null_mut(), None);
        state.pdos[0] = keep;
        state.pdos[1] = fresh;
        state.pdo_count = 2;
    }
    // Later generations report the same set

    let count = state.pdo_count;
    let layout = Layout::array::<*const DeviceObject>(count).unwrap();
    let array = match PoolAllocatorGlobal.allocate(layout) {
        Ok(array) => unsafe { 
            let ptr = array.cast::<*const DeviceObject>().as_ptr();
            core::slice::from_raw_parts_mut(ptr, count)
        },
        Err(_) => return Status::Failed
    };

    for i in 0..count {
        array[i] = state.pdos[i];
    }

    info!("test1 enumerate: reporting {} PDO(s)", count);
    request.req_params = Some(ReqInfo{enumerate: array});
    request.complete_irp(Status::Success);
    Status::Success
}

fn dispatch_start(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("test1 start_device {}", device.id);

    request.complete_irp(Status::Success);
    Status::Success
}

fn dispatch_stop(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("test1 stop_device {}", device.id);
    request.complete_irp(Status::Success);
    Status::Success
}

fn dispatch_remove(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("test1 remove_device {}", device.id);
    // Free the per-FDO ctx if any (PDOs carry none).
    if !device.ctx.is_null() {
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(device.ctx as *mut Test1Device, PoolAllocatorGlobal));
        }
    }
    request.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_read(_device: &DeviceObject, request: &mut Irp) -> Status {
    info!("test1 read (async pending)");
    PENDING_IRP.store(request as *mut Irp, Ordering::Release);
    unsafe { kernel_intf::create_kernel_thread(read_worker) };
    Status::Pending
}

fn read_worker() -> ! {
    info!("test1 read worker: completing pending IRP");
    let irp = PENDING_IRP.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if !irp.is_null() {
        unsafe { io_complete_irp(irp, Status::Success) };
    }
    unsafe { kernel_intf::exit_kernel_thread() }
}

#[kmod::dispatch_handler]
fn dispatch_write(_device: &DeviceObject, request: &mut Irp) -> Status {
    info!("test1 write");
    request.complete_irp(Status::Success);
    Status::Success
}
