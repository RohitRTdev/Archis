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
    AE_OK, AcpiHandle, AcpiObjectType, AcpiPnpDeviceId, AcpiPnpDeviceIdList,
    AcpiSimpleResource, AcpiBufferRaw, ACPI_ALLOCATE_BUFFER,
    acpi_enumerate_devices, acpi_get_object_info, acpi_os_free, acpi_get_current_resources, acpica_init
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

#[repr(C, packed)]
struct AcpiResourceHeader {
    res_type: u32,
    length: u32
}

#[repr(C, packed)]
struct AcpiResourceSource {
    index: u8,
    string_length: u16,
    string_ptr: *const core::ffi::c_char
}

#[repr(C, packed)]
struct AcpiResourceIrq {
    descriptor_length: u8,
    triggering: u8, // 1 -> level triggered
    polarity: u8, // 1 -> active low
    shareable: u8,
    wake_capable: u8,
    interrupt_count: u8,
    interrupts: [u8; 1]
}

#[repr(C, packed)]
struct AcpiResourceExtendedIrq {
    producer_consumer: u8,
    triggering: u8,
    polarity: u8,
    shareable: u8,
    wake_capable: u8,
    interrupt_count: u8,
    resource_source: AcpiResourceSource,
    interrupts: [u32; 1]
}

#[repr(C, packed)]
struct AcpiResourceIo {
    io_decode: u8,
    alignment: u8,
    address_length: u8,
    minimum: u16,
    maximum: u16
}

#[repr(C, packed)]
struct AcpiResourceFixedIo {
    address: u16,
    address_length: u8
}

#[repr(C, packed)]
struct AcpiResourceMemory32 {
    write_protect: u8,
    minimum: u32,
    maximum: u32,
    alignment: u32,
    address_length: u32
}

#[repr(C)]
struct AcpiDeviceInfo {
    _info_size: u32, 
    _name: u32, 
    _type: AcpiObjectType, 
    _param_count: u8, 
    valid: u16, 
    _flags: u8, 
    _highest_d_states: [u8; 4], 
    _lowest_d_states: [u8; 5], 
    _address: u64, 
    hardware_id: AcpiPnpDeviceId, 
    _unique_id: AcpiPnpDeviceId,
    _subsystem_id: AcpiPnpDeviceId, 
    _compatible_id_list: AcpiPnpDeviceIdList 
}

// Resource type IDs from acrestyp.h
const ACPI_RESOURCE_TYPE_IRQ: u32 = 0;
const ACPI_RESOURCE_TYPE_IO: u32 = 4;
const ACPI_RESOURCE_TYPE_FIXED_IO: u32 = 5;
const ACPI_RESOURCE_TYPE_END_TAG: u32 = 7;
const ACPI_RESOURCE_TYPE_MEMORY32: u32 = 9;
const ACPI_RESOURCE_TYPE_EXTENDED_IRQ: u32 = 15;

const ACPI_VALID_HID_FLAG: u16 = 0x4;

fn acpi_get_hid(handle: *mut c_void, buf: *mut u8, buf_len: usize) -> usize {
    if handle.is_null() || buf.is_null() || buf_len == 0 {
        return 0;
    }
    unsafe {
        let mut info_ptr: *mut u8 = core::ptr::null_mut();
        let status = acpi_get_object_info(handle, &mut info_ptr);
        if status != AE_OK || info_ptr.is_null() {
            return 0;
        }
        let info = &mut *(info_ptr as *mut AcpiDeviceInfo);
        let result = if info.valid & ACPI_VALID_HID_FLAG != 0 {
            let string_ptr = info.hardware_id.string;
            if !string_ptr.is_null() {
                let hid = core::ffi::CStr::from_ptr(string_ptr).to_bytes();
                let copy_len = hid.len().min(buf_len.saturating_sub(1));
                core::ptr::copy_nonoverlapping(hid.as_ptr(), buf, copy_len);
                *buf.add(copy_len) = 0;
                copy_len
            } else {
                0
            }
        } else {
            0
        };

        acpi_os_free(info_ptr as *mut c_void);
        result
    }
}

fn acpi_get_resources(
    handle: *mut c_void,
    out: *mut AcpiSimpleResource,
    max: usize,
) -> usize {
    if handle.is_null() || out.is_null() || max == 0 {
        return 0;
    }

    unsafe {
        let mut buf = AcpiBufferRaw {
            length: ACPI_ALLOCATE_BUFFER,
            pointer: core::ptr::null_mut()
        };

        let status = acpi_get_current_resources(handle, &mut buf);
        if status != AE_OK || buf.pointer.is_null() {
            return 0;
        }

        let mut count = 0usize;
        let mut ptr = buf.pointer as *const u8;

        loop {
            let header = core::ptr::read_unaligned(ptr as *const AcpiResourceHeader);

            if header.res_type == ACPI_RESOURCE_TYPE_END_TAG || header.length == 0 {
                break;
            }

            let data = ptr.add(core::mem::size_of::<AcpiResourceHeader>());

            if count < max {
                match header.res_type {
                    ACPI_RESOURCE_TYPE_IRQ => {
                        let irq = core::ptr::read_unaligned(data as *const AcpiResourceIrq);

                        let interrupts = data.add(core::mem::offset_of!(AcpiResourceIrq, interrupts))
                            as *const u8;

                        for i in 0..irq.interrupt_count as usize {
                            if count >= max {
                                break;
                            }

                            *out.add(count) = AcpiSimpleResource {
                                res_type: 0,
                                address: *interrupts.add(i) as u64,
                                length: 0,
                            };
                            count += 1;
                        }
                    },
                    ACPI_RESOURCE_TYPE_EXTENDED_IRQ => {
                        let irq = core::ptr::read_unaligned(data as *const AcpiResourceExtendedIrq);

                        let interrupts = data.add(core::mem::offset_of!(
                            AcpiResourceExtendedIrq,
                            interrupts
                        )) as *const u32;

                        for i in 0..irq.interrupt_count as usize {
                            if count >= max {
                                break;
                            }

                            *out.add(count) = AcpiSimpleResource {
                                res_type: 0,
                                address: core::ptr::read_unaligned(interrupts.add(i)) as u64,
                                length: 0,
                            };
                            count += 1;
                        }
                    },
                    ACPI_RESOURCE_TYPE_IO => {
                        let io = core::ptr::read_unaligned(data as *const AcpiResourceIo);

                        *out.add(count) = AcpiSimpleResource {
                            res_type: 1,
                            address: io.minimum as u64,
                            length: io.address_length as u64,
                        };
                        count += 1;
                    },
                    ACPI_RESOURCE_TYPE_FIXED_IO => {
                        let io = core::ptr::read_unaligned(data as *const AcpiResourceFixedIo);

                        *out.add(count) = AcpiSimpleResource {
                            res_type: 1,
                            address: io.address as u64,
                            length: io.address_length as u64,
                        };
                        count += 1;
                    },
                    ACPI_RESOURCE_TYPE_MEMORY32 => {
                        let mem = core::ptr::read_unaligned(data as *const AcpiResourceMemory32);

                        *out.add(count) = AcpiSimpleResource {
                            res_type: 2,
                            address: mem.minimum as u64,
                            length: mem.address_length as u64,
                        };
                        count += 1;
                    },
                    _ => {}
                }
            }

            ptr = ptr.add(header.length as usize);
        }

        acpi_os_free(buf.pointer);
        count
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
    let hid_len = acpi_get_hid(handle, hid.as_mut_ptr(), hid.len());
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
