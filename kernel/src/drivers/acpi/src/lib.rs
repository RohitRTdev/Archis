#![cfg_attr(not(test), no_std)]

use core::sync::atomic::{AtomicBool, Ordering};
use kernel_intf::{driver::{DeviceObject, DriverObject, Irp, IrpMinor, Status, create_device}, info};
use acpi_intf::*;
use kmod::dispatch_init;


static IS_SUBSYSTEM_DEV_STARTED: AtomicBool = AtomicBool::new(false);
static IS_SUBSYSTEM_DEV_ADDED: AtomicBool = AtomicBool::new(false);

fn init() {
    // Bring the OSL up first — ACPICA's AcpiOs* calls during the subsystem
    // bring-up (cache creation, mutex creation, table scanning) need the
    // work queue and bookkeeping ready.
    unsafe {
        acpica_init();
    }
}

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
    // Must be attached to root bus
    assert!(pdo.id == 0);

    create_device(driver, Some("acpi"), core::ptr::null_mut(), Some(pdo), false);

    IS_SUBSYSTEM_DEV_ADDED.store(true, Ordering::Release);

    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, req: &mut Irp) -> Status {
    match req.minor_code {
        IrpMinor::Enumerate => {},
        IrpMinor::Resources => {},
        IrpMinor::Query => {},
        IrpMinor::Start => { return do_start(device, req); },
        IrpMinor::Stop | IrpMinor::Remove => {
            panic!("Attempted to uninit acpi subsystem");
        },
        _ => {}
    }

    Status::Unsupported
}

fn do_start(device: &DeviceObject, req: &mut Irp) -> Status {
    if IS_SUBSYSTEM_DEV_STARTED.load(Ordering::Acquire) {
        info!("acpi subsystem already started!");
        req.complete_irp(Status::Failed);
        return Status::Failed;
    }

    init();

    IS_SUBSYSTEM_DEV_STARTED.store(true, Ordering::Release);
    req.complete_irp(Status::Success);

    Status::Success
}

#[kmod::driver_unload]
fn unload(driver: &DriverObject) {
    panic!("Attempted to unload {} driver!", driver.get_name());
}



