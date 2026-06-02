#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::sync::atomic::{AtomicPtr, Ordering};

use kernel_intf::{info, exported_function};
use kernel_intf::driver::{Status, DriverObject, Irp, DeviceObject};
use kernel_intf::mem::PoolAllocatorGlobal;
use alloc::vec::Vec;

// Per-device state owned by this driver 
struct Test1Device {
    _reads: u64
}

static PENDING_IRP: AtomicPtr<Irp> = AtomicPtr::new(core::ptr::null_mut());

#[kmod::init]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing...", driver.get_name());

    kmod::dispatch_init!(driver, dispatch_add, dispatch_read, dispatch_write, dispatch_start);

    let mut vec: Vec<i32, PoolAllocatorGlobal> = Vec::new_in(PoolAllocatorGlobal);

    for i in 0..10 {
        vec.push(i);
    }
    info!("{:?}", vec);

    unsafe {exported_function();test2::get_test2()};

    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, _pdo: Option<&DeviceObject>) -> Status {
    info!("test1 add_device: creating functional device");

    // Allocate this device's private state and hand it to the kernel as ctx.
    let ctx = alloc::boxed::Box::new_in(Test1Device { _reads: 0 }, PoolAllocatorGlobal);
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut core::ffi::c_void;

    let device = unsafe {
        kernel_intf::io_create_device(driver.id, common::StrRef::from_str("test1"), ctx_ptr)
    };

    if device.is_null() {
        info!("test1 add_device: io_create_device failed");
        return Status::Failed;
    }

    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_start(_device: &DeviceObject, _request: &mut Irp) -> Status {
    info!("test1 start_device");
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_read(_device: &DeviceObject, request: &mut Irp) -> Status {
    info!("Calling driver read");

    // Real async path: hand the IRP to a worker thread and report Pending
    // without touching any IRP state. The worker completes it later.
    PENDING_IRP.store(request as *mut Irp, Ordering::Release);

    unsafe { kernel_intf::create_kernel_thread(read_worker) };

    Status::Pending
}

fn read_worker() -> ! {
    info!("test1 read worker: completing pending IRP");

    let irp = PENDING_IRP.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if !irp.is_null() {
        unsafe { (*irp).complete_irp(Status::Success) };
    }

    unsafe { kernel_intf::exit_kernel_thread() }
}

#[kmod::dispatch_handler]
fn dispatch_write(_device: &DeviceObject, _request: &mut Irp) -> Status {
    info!("Calling driver write");
    Status::Success
}
