#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

use kernel_intf::{
    info,
    driver::{DeviceObject, DeviceType, DriverObject, Irp, IrpMinor, Status, ResEntry, ResType, create_device}
};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::hw::{inb, outb, ec_read, ec_write, ec_wait_ibf, ec_wait_obf};
use acpi_intf::{
    AE_OK, ACPI_STATUS, ACPI_PHYSICAL_ADDRESS, AcpiHandle,
    ACPI_ADR_SPACE_EC, ACPI_ALL_NOTIFY, ACPI_GPE_LEVEL_TRIGGERED, ACPI_REENABLE_GPE,
    acpi_evaluate_integer, acpi_evaluate_void, acpi_queue_work,
    acpi_install_address_space_handler, acpi_remove_address_space_handler,
    acpi_install_notify_handler, acpi_remove_notify_handler,
    acpi_install_gpe_handler, acpi_remove_gpe_handler,
    acpi_enable_gpe, acpi_disable_gpe
};

static DEVICE_COUNT: AtomicUsize = AtomicUsize::new(0);

struct EcCtx {
    acpi_handle: AcpiHandle,
    gpe_number:  u32,
    data_port:   u16,
    cmd_port:    u16
}

unsafe impl Send for EcCtx {}
unsafe impl Sync for EcCtx {}

impl EcCtx {
    const fn zeroed() -> Self {
        Self {
            acpi_handle: core::ptr::null_mut(),
            gpe_number:  u32::MAX,
            data_port:   0,
            cmd_port:    0
        }
    }
}

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("ec: initializing (id={})", driver.id);
    kmod::dispatch_init!(driver, dispatch_add, dispatch_pnp);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: &DeviceObject) -> Status {
    let idx = DEVICE_COUNT.fetch_add(1, Ordering::Relaxed);
    let name: &'static str = alloc::boxed::Box::leak(
        alloc::format!("ec{}", idx).into_boxed_str()
    );

    let mut ctx = alloc::boxed::Box::new_in(EcCtx::zeroed(), PoolAllocatorGlobal);

    ctx.acpi_handle = unsafe { acpi::acpi_pdo_get_handle(pdo.ctx) };

    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;

    let dev = create_device(driver, Some(name), ctx_ptr, Some(pdo), false, DeviceType::None);
    if dev.is_null() {
        unsafe { drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut EcCtx, PoolAllocatorGlobal)); }
        return Status::Failed;
    }

    info!("ec: added device '{}'", name);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, request: &mut Irp) -> Status {
    match request.minor_code {
        IrpMinor::Start  => do_start(device, request),
        IrpMinor::Stop   => do_stop(device, request),
        IrpMinor::Remove => do_remove(device, request),
        _                => Status::Unsupported
    }
}

fn do_start(device: &DeviceObject, request: &mut Irp) -> Status {
    let ctx = unsafe { &mut *(device.ctx as *mut EcCtx) };

    // Parse I/O port resources; lower address = data, higher = cmd.
    let res_list = unsafe { request.req_info.res_list };
    let res_slice: &[ResEntry] = unsafe { core::slice::from_raw_parts(res_list.base, res_list.count) };
    let mut port_a: u16 = 0;
    let mut port_b: u16 = 0;
    for entry in res_slice {
        if let ResType::Port = entry.res_type {
            let base = unsafe { entry.desc.port.base as u16 };
            if port_a == 0 { port_a = base; } else if port_b == 0 { port_b = base; }
        }
    }
    if port_a != 0 && port_b != 0 {
        ctx.data_port = port_a.min(port_b);
        ctx.cmd_port  = port_a.max(port_b);
    }

    let handle = ctx.acpi_handle;
    if handle.is_null() {
        info!("ec: no ACPI handle, cannot install handlers");
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    // Address space handler for EC operation regions.
    let s = acpi_install_address_space_handler(
        handle, ACPI_ADR_SPACE_EC, Some(ec_region_handler), None, device.ctx
    );
    if s != AE_OK {
        info!("ec: address space handler install failed: {:#X}", s);
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    // Notify handler (catches system notifications on this device object).
    acpi_install_notify_handler(handle, ACPI_ALL_NOTIFY, Some(ec_notify_handler), device.ctx);

    // GPE for EC events — evaluate _GPE to get the GPE number.
    let mut gpe_num = 0u64;
    let s = acpi_evaluate_integer(
        handle,
        b"_GPE\0".as_ptr() as *const core::ffi::c_char,
        &mut gpe_num
    );
    if s == AE_OK {
        ctx.gpe_number = gpe_num as u32;
        acpi_install_gpe_handler(
            core::ptr::null_mut(), ctx.gpe_number,
            ACPI_GPE_LEVEL_TRIGGERED, Some(ec_gpe_handler), device.ctx
        );
        acpi_enable_gpe(core::ptr::null_mut(), ctx.gpe_number);
        info!("ec: start — data={:#X} cmd={:#X} gpe={}", ctx.data_port, ctx.cmd_port, ctx.gpe_number);
    } else {
        info!("ec: start — data={:#X} cmd={:#X} (no _GPE)", ctx.data_port, ctx.cmd_port);
    }

    request.complete_irp(Status::Success);
    Status::Success
}

fn do_stop(device: &DeviceObject, request: &mut Irp) -> Status {
    let ctx = unsafe { &*(device.ctx as *mut EcCtx) };

    if ctx.gpe_number != u32::MAX {
        acpi_disable_gpe(core::ptr::null_mut(), ctx.gpe_number);
        acpi_remove_gpe_handler(core::ptr::null_mut(), ctx.gpe_number, Some(ec_gpe_handler));
    }

    if !ctx.acpi_handle.is_null() {
        acpi_remove_notify_handler(ctx.acpi_handle, ACPI_ALL_NOTIFY, Some(ec_notify_handler));
        acpi_remove_address_space_handler(ctx.acpi_handle, ACPI_ADR_SPACE_EC, Some(ec_region_handler));
    }

    info!("ec: stopped");
    request.complete_irp(Status::Success);
    Status::Success
}

fn do_remove(device: &DeviceObject, request: &mut Irp) -> Status {
    if let Some(name) = device.get_name() {
        unsafe { drop(alloc::boxed::Box::from_raw(name as *const str as *mut str)); }
    }

    if !device.ctx.is_null() {
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(
                device.ctx as *mut EcCtx,
                PoolAllocatorGlobal
            ));
        }
    }
    request.complete_irp(Status::Success);
    Status::Success
}

unsafe extern "C" fn ec_region_handler(
    function: u32,
    address: ACPI_PHYSICAL_ADDRESS,
    bit_width: u32,
    value: *mut u64,
    handler_ctx: *mut c_void,
    _region_ctx: *mut c_void
) -> ACPI_STATUS {
    let ctx   = unsafe { &*(handler_ctx as *const EcCtx) };
    let data  = ctx.data_port;
    let cmd   = ctx.cmd_port;
    let bytes = (bit_width / 8) as u64;

    if function == 0 {
        let mut result = 0u64;
        for i in 0..bytes {
            result |= (ec_read(data, cmd, (address + i) as u8) as u64) << (i * 8);
        }
        unsafe { *value = result; }
    } else {
        let v = unsafe { *value };
        for i in 0..bytes {
            ec_write(data, cmd, (address + i) as u8, (v >> (i * 8)) as u8);
        }
    }
    AE_OK
}

unsafe extern "C" fn ec_notify_handler(
    _device: AcpiHandle,
    value: u32,
    _context: *mut c_void
) {
    info!("ec: notify {:#X}", value);
}

struct EcQueryWorkCtx {
    handle: AcpiHandle,
    method: [u8; 5]
}

unsafe impl Send for EcQueryWorkCtx {}

extern "C" fn ec_query_worker(ctx: *mut c_void) {
    let work = unsafe {
        alloc::boxed::Box::from_raw_in(ctx as *mut EcQueryWorkCtx, PoolAllocatorGlobal)
    };
    acpi_evaluate_void(work.handle, work.method.as_ptr() as *const core::ffi::c_char);
}

unsafe extern "C" fn ec_gpe_handler(
    _device: AcpiHandle,
    _gpe_number: u32,
    context: *mut c_void
) -> u32 {
    let ctx = unsafe { &*(context as *const EcCtx) };
    let cmd  = ctx.cmd_port;
    let data = ctx.data_port;

    // Check SCI_EVT bit in EC status register.
    let status = unsafe { inb(cmd) };
    if status & 0x20 == 0 {
        return ACPI_REENABLE_GPE;
    }

    // Issue EC query command to retrieve the query code.
    if !ec_wait_ibf(cmd) { return ACPI_REENABLE_GPE; }
    unsafe { outb(cmd, 0x84); }

    if !ec_wait_obf(cmd) { return ACPI_REENABLE_GPE; }
    let query_code = unsafe { inb(data) };

    if query_code != 0 {
        let method = query_method_name(query_code);
        let work = alloc::boxed::Box::new_in(
            EcQueryWorkCtx { handle: ctx.acpi_handle, method },
            PoolAllocatorGlobal
        );
        let work_ptr = alloc::boxed::Box::into_raw_with_allocator(work).0 as *mut c_void;
        if !acpi_queue_work(ec_query_worker, work_ptr) {
            unsafe {
                drop(alloc::boxed::Box::from_raw_in(work_ptr as *mut EcQueryWorkCtx, PoolAllocatorGlobal));
            }
        }
    }

    ACPI_REENABLE_GPE
}

fn query_method_name(code: u8) -> [u8; 5] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    [b'_', b'Q', HEX[(code >> 4) as usize], HEX[(code & 0xF) as usize], 0]
}

#[kmod::driver_unload]
fn destroy(driver: &mut DriverObject) {
    info!("ec: unloading (id={})", driver.id);
}
