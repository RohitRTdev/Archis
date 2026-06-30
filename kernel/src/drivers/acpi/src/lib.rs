#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::{c_void, c_char};
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use common::StrRef;
use kernel_intf::{
    driver::{
        DeviceObject, DriverObject, Irp, IrpMinor, Status,
        create_device, create_device_by_id,
        ResEntry, ResType, ResTypeDesc, IntDesc, PortDesc,
        IrqInfo, MAX_RESOURCE_ENTRIES
    },
    info
};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::hw::pci_cfg_read8;
use common::MemoryRegion;
use acpi_intf::{
    AE_OK, AcpiHandle, AcpiObjectType, AcpiPnpDeviceId, AcpiPnpDeviceIdList,
    AcpiSimpleResource, AcpiBufferRaw, ACPI_ALLOCATE_BUFFER,
    acpi_enumerate_devices, acpi_get_object_info, acpi_os_free,
    acpi_get_current_resources, acpi_get_parent, acpi_get_irq_routing_table,
    acpi_get_handle, acpi_evaluate_integer, acpica_init
};
use kmod::dispatch_init;

static IS_SUBSYSTEM_DEV_STARTED: AtomicBool = AtomicBool::new(false);
static IS_SUBSYSTEM_DEV_ADDED: AtomicBool = AtomicBool::new(false);
static IS_ENUMERATED: AtomicBool = AtomicBool::new(false);

// PCI root bridge tracking
const MAX_ROOT_BRIDGES: usize = 8;
const MAX_PCI_CHILDREN: usize = 256;

static ROOT_BRIDGE_HANDLES: [AtomicUsize; MAX_ROOT_BRIDGES] =
    [const { AtomicUsize::new(0) }; MAX_ROOT_BRIDGES];
static ROOT_BRIDGE_COUNT: AtomicUsize = AtomicUsize::new(0);

// Indexed as [rb_idx * MAX_PCI_CHILDREN + child_idx]
static PCI_CHILD_ADDRS: [AtomicU32; MAX_ROOT_BRIDGES * MAX_PCI_CHILDREN] =
    [const { AtomicU32::new(u32::MAX) }; MAX_ROOT_BRIDGES * MAX_PCI_CHILDREN];
// Indexed as [(rb_idx * MAX_PCI_CHILDREN + child_idx) * 4 + pin]
static PCI_CHILD_GSIS: [AtomicU32; MAX_ROOT_BRIDGES * MAX_PCI_CHILDREN * 4] =
    [const { AtomicU32::new(u32::MAX) }; MAX_ROOT_BRIDGES * MAX_PCI_CHILDREN * 4];
static PCI_CHILD_COUNTS: [AtomicUsize; MAX_ROOT_BRIDGES] =
    [const { AtomicUsize::new(0) }; MAX_ROOT_BRIDGES];

static ROOT_BRIDGE_BUS: [AtomicU8; MAX_ROOT_BRIDGES] =
    [const { AtomicU8::new(0) }; MAX_ROOT_BRIDGES];
static ROOT_BRIDGE_SEG: [AtomicU32; MAX_ROOT_BRIDGES] =
    [const { AtomicU32::new(0) }; MAX_ROOT_BRIDGES];

struct AcpiPdoCtx {
    handle: AcpiHandle,
    hid: [u8; 24],
    hid_len: usize,
    cids: [[u8; 24]; MAX_CIDS],
    cid_lens: [usize; MAX_CIDS],
    cid_count: usize
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
    polarity: u8,   // 1 -> active low
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
    address: u64,
    hardware_id: AcpiPnpDeviceId,
    _unique_id: AcpiPnpDeviceId,
    _subsystem_id: AcpiPnpDeviceId,
    compatible_id_list: AcpiPnpDeviceIdList
}

// Resource type IDs from acrestyp.h
const ACPI_RESOURCE_TYPE_IRQ: u32 = 0;
const ACPI_RESOURCE_TYPE_IO: u32 = 4;
const ACPI_RESOURCE_TYPE_FIXED_IO: u32 = 5;
const ACPI_RESOURCE_TYPE_END_TAG: u32 = 7;
const ACPI_RESOURCE_TYPE_MEMORY32: u32 = 9;
const ACPI_RESOURCE_TYPE_EXTENDED_IRQ: u32 = 15;

const ACPI_VALID_HID_FLAG: u16 = 0x4;
const ACPI_VALID_ADR_FLAG: u16 = 0x2;
const ACPI_VALID_CID_FLAG: u16 = 0x20;
const MAX_CIDS: usize = 8;

fn pack_gsi(gsi: u32, active_high: bool, edge_triggered: bool) -> u32 {
    (gsi & 0x0FFF_FFFF) | ((active_high as u32) << 28) | ((edge_triggered as u32) << 29)
}


// Returns (hid_buf, hid_len, address, valid_flags) from one acpi_get_object_info call.
fn acpi_get_device_info(handle: AcpiHandle) -> ([u8; 24], usize, u64, u16) {
    let mut hid = [0u8; 24];
    let mut hid_len = 0usize;
    let mut address = 0u64;
    let mut valid = 0u16;

    if handle.is_null() {
        return (hid, hid_len, address, valid);
    }

    unsafe {
        let mut info_ptr: *mut u8 = core::ptr::null_mut();
        let status = acpi_get_object_info(handle, &mut info_ptr);
        if status != AE_OK || info_ptr.is_null() {
            return (hid, hid_len, address, valid);
        }

        let info = &*(info_ptr as *const AcpiDeviceInfo);
        valid = info.valid;
        address = info.address;

        if valid & ACPI_VALID_HID_FLAG != 0 {
            let string_ptr = info.hardware_id.string;
            if !string_ptr.is_null() {
                let bytes = core::ffi::CStr::from_ptr(string_ptr).to_bytes();
                let copy_len = bytes.len().min(hid.len().saturating_sub(1));
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), hid.as_mut_ptr(), copy_len);
                hid[copy_len] = 0;
                hid_len = copy_len;
            }
        }

        acpi_os_free(info_ptr as *mut c_void);
    }

    (hid, hid_len, address, valid)
}

fn acpi_get_resources(
    handle: *mut c_void,
    out: *mut AcpiSimpleResource,
    max: usize,
) -> usize {
    if handle.is_null() || out.is_null() || max == 0 {
        return 0;
    }

    let mut buf = AcpiBufferRaw {
        length: ACPI_ALLOCATE_BUFFER,
        pointer: core::ptr::null_mut()
    };

    let status = acpi_get_current_resources(handle, &mut buf);
    if status != AE_OK || buf.pointer.is_null() {
        return 0;
    }

    let count = parse_resource_buf(buf.pointer as *const u8, out, max);
    acpi_os_free(buf.pointer);
    count
}

// Parses all resource types from a raw ACPICA resource buffer into AcpiSimpleResource entries.
fn parse_resource_buf(raw: *const u8, out: *mut AcpiSimpleResource, max: usize) -> usize {
    let mut count = 0usize;
    let mut ptr = raw;

    loop {
        let header = unsafe { core::ptr::read_unaligned(ptr as *const AcpiResourceHeader) };
        if header.res_type == ACPI_RESOURCE_TYPE_END_TAG || header.length == 0 {
            break;
        }

        let data = unsafe { ptr.add(size_of::<AcpiResourceHeader>()) };

        if count < max {
            match header.res_type {
                ACPI_RESOURCE_TYPE_IRQ => {
                    let irq = unsafe { core::ptr::read_unaligned(data as *const AcpiResourceIrq) };
                    let interrupts = unsafe { data.add(core::mem::offset_of!(AcpiResourceIrq, interrupts)) as *const u8 };
                    for i in 0..irq.interrupt_count as usize {
                        if count >= max { break; }
                        unsafe {
                            *out.add(count) = AcpiSimpleResource {
                                res_type: 0,
                                address: *interrupts.add(i) as u64,
                                length: 0,
                                active_high: irq.polarity == 0,
                                edge_triggered: irq.triggering == 0
                            };
                        }
                        count += 1;
                    }
                }
                ACPI_RESOURCE_TYPE_EXTENDED_IRQ => {
                    let irq = unsafe { core::ptr::read_unaligned(data as *const AcpiResourceExtendedIrq) };
                    let interrupts = unsafe { data.add(core::mem::offset_of!(AcpiResourceExtendedIrq, interrupts)) as *const u32 };
                    for i in 0..irq.interrupt_count as usize {
                        if count >= max { break; }
                        unsafe {
                            *out.add(count) = AcpiSimpleResource {
                                res_type: 0,
                                address: core::ptr::read_unaligned(interrupts.add(i)) as u64,
                                length: 0,
                                active_high: irq.polarity == 0,
                                edge_triggered: irq.triggering == 0
                            };
                        }
                        count += 1;
                    }
                }
                ACPI_RESOURCE_TYPE_IO => {
                    let io = unsafe { core::ptr::read_unaligned(data as *const AcpiResourceIo) };
                    unsafe {
                        *out.add(count) = AcpiSimpleResource {
                            res_type: 1,
                            address: io.minimum as u64,
                            length: io.address_length as u64,
                            ..AcpiSimpleResource::default()
                        };
                    }
                    count += 1;
                }
                ACPI_RESOURCE_TYPE_FIXED_IO => {
                    let io = unsafe { core::ptr::read_unaligned(data as *const AcpiResourceFixedIo) };
                    unsafe {
                        *out.add(count) = AcpiSimpleResource {
                            res_type: 1,
                            address: io.address as u64,
                            length: io.address_length as u64,
                            ..AcpiSimpleResource::default()
                        };
                    }
                    count += 1;
                }
                ACPI_RESOURCE_TYPE_MEMORY32 => {
                    let mem = unsafe { core::ptr::read_unaligned(data as *const AcpiResourceMemory32) };
                    unsafe {
                        *out.add(count) = AcpiSimpleResource {
                            res_type: 2,
                            address: mem.minimum as u64,
                            length: mem.address_length as u64,
                            ..AcpiSimpleResource::default()
                        };
                    }
                    count += 1;
                }
                _ => {}
            }
        } else {
            info!("Crossing max resource descriptor count of {}", max);
        }

        ptr = unsafe { ptr.add(header.length as usize) };
    }

    count
}

// Walks the _PRT table on `bus_handle` and stores packed GSI entries for the device
// at address `dev_addr` (bits [31:16] = device number) into PCI_CHILD_GSIS.
fn resolve_prt_for_child(bus_handle: AcpiHandle, dev_addr: u64, gsi_base: usize) {
    let mut buf = AcpiBufferRaw {
        length: ACPI_ALLOCATE_BUFFER,
        pointer: core::ptr::null_mut()
    };

    let status = acpi_get_irq_routing_table(bus_handle, &mut buf);
    if status != AE_OK || buf.pointer.is_null() {
        return;
    }

    let dev = dev_addr >> 16;
    let mut ptr = buf.pointer as *const u8;

    loop {
        // ACPI_PCI_ROUTING_TABLE: u32 Length, u32 Pin, u64 Address, u32 SourceIndex, char Source[]
        let length     = unsafe { core::ptr::read_unaligned(ptr as *const u32) };
        if length == 0 { break; }

        let pin        = unsafe { core::ptr::read_unaligned(ptr.add(4) as *const u32) };
        let address    = unsafe { core::ptr::read_unaligned(ptr.add(8) as *const u64) };
        let src_index  = unsafe { core::ptr::read_unaligned(ptr.add(16) as *const u32) };
        let source_ptr = unsafe { ptr.add(20) };

        if address >> 16 == dev && pin < 4 {
            let packed = if unsafe { *source_ptr } == 0 {
                // Direct GSI: PCI spec mandates active-low, level-triggered
                pack_gsi(src_index, false, false)
            } else {
                let (gsi, ah, et) = resolve_link_device_gsi(bus_handle, source_ptr, src_index);
                if gsi == u32::MAX { u32::MAX } else { pack_gsi(gsi, ah, et) }
            };
            PCI_CHILD_GSIS[gsi_base + pin as usize].store(packed, Ordering::Release);
        }

        ptr = unsafe { ptr.add(length as usize) };
    }

    acpi_os_free(buf.pointer);
}

// Resolves GSI + polarity/trigger from an IRQ link device referenced by `source_path`.
// `source_index` selects which IRQ entry in the link device's _CRS to use.
fn resolve_link_device_gsi(bus_handle: AcpiHandle, source_path: *const u8, source_index: u32) -> (u32, bool, bool) {
    let mut link_handle: AcpiHandle = core::ptr::null_mut();
    let status = acpi_get_handle(bus_handle, source_path as *const c_char, &mut link_handle);
    if status != AE_OK || link_handle.is_null() {
        return (u32::MAX, false, false);
    }

    let mut buf = AcpiBufferRaw {
        length: ACPI_ALLOCATE_BUFFER,
        pointer: core::ptr::null_mut()
    };

    let status = acpi_get_current_resources(link_handle, &mut buf);
    if status != AE_OK || buf.pointer.is_null() {
        return (u32::MAX, false, false);
    }

    let mut irq_idx = 0u32;
    let mut ptr = buf.pointer as *const u8;
    let mut result = (u32::MAX, false, false);

    'outer: loop {
        let header = unsafe { core::ptr::read_unaligned(ptr as *const AcpiResourceHeader) };
        if header.res_type == ACPI_RESOURCE_TYPE_END_TAG || header.length == 0 {
            break;
        }
        let data = unsafe { ptr.add(size_of::<AcpiResourceHeader>()) };

        match header.res_type {
            ACPI_RESOURCE_TYPE_IRQ => {
                let irq = unsafe { core::ptr::read_unaligned(data as *const AcpiResourceIrq) };
                let interrupts = unsafe { data.add(core::mem::offset_of!(AcpiResourceIrq, interrupts)) as *const u8 };
                for i in 0..irq.interrupt_count as usize {
                    if irq_idx == source_index {
                        let gsi = unsafe { *interrupts.add(i) } as u32;
                        result = (gsi, irq.polarity == 0, irq.triggering == 0);
                        break 'outer;
                    }
                    irq_idx += 1;
                }
            }
            ACPI_RESOURCE_TYPE_EXTENDED_IRQ => {
                let irq = unsafe { core::ptr::read_unaligned(data as *const AcpiResourceExtendedIrq) };
                let interrupts = unsafe { data.add(core::mem::offset_of!(AcpiResourceExtendedIrq, interrupts)) as *const u32 };
                for i in 0..irq.interrupt_count as usize {
                    if irq_idx == source_index {
                        let gsi = unsafe { core::ptr::read_unaligned(interrupts.add(i)) };
                        result = (gsi, irq.polarity == 0, irq.triggering == 0);
                        break 'outer;
                    }
                    irq_idx += 1;
                }
            }
            _ => {}
        }

        ptr = unsafe { ptr.add(header.length as usize) };
    }

    acpi_os_free(buf.pointer);
    result
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
        _ => Status::Unsupported
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
    if IS_ENUMERATED.swap(true, Ordering::AcqRel) {
        panic!("acpi enumeration called more than once");
    }

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

fn acpi_get_cids(
    handle: AcpiHandle,
    cids: &mut [[u8; 24]; MAX_CIDS],
    cid_lens: &mut [usize; MAX_CIDS]
) -> usize {
    let mut info_ptr: *mut u8 = core::ptr::null_mut();
    if acpi_get_object_info(handle, &mut info_ptr) != AE_OK || info_ptr.is_null() {
        return 0;
    }
    let info = unsafe { &*(info_ptr as *const AcpiDeviceInfo) };
    let mut count = 0usize;
    if info.valid & ACPI_VALID_CID_FLAG != 0 {
        let list_ptr = &info.compatible_id_list as *const AcpiPnpDeviceIdList;
        let n = unsafe { (*list_ptr).count } as usize;
        let ids_ptr = unsafe {
            (list_ptr as *const u8)
                .add(core::mem::size_of::<AcpiPnpDeviceIdList>())
                as *const AcpiPnpDeviceId
        };
        for i in 0..n.min(MAX_CIDS) {
            let cid = unsafe { &*ids_ptr.add(i) };
            if !cid.string.is_null() {
                let bytes = unsafe { core::ffi::CStr::from_ptr(cid.string) }.to_bytes();
                let copy_len = bytes.len().min(23);
                cids[i][..copy_len].copy_from_slice(&bytes[..copy_len]);
                cids[i][copy_len] = 0;
                cid_lens[i] = copy_len;
                count += 1;
            }
        }
    }
    acpi_os_free(info_ptr as *mut c_void);
    count
}

unsafe extern "C" fn acpi_pdo_callback(
    handle: *mut c_void,
    _nesting_level: u32,
    context: *mut c_void,
    _return_value: *mut *mut c_void
) -> u32 {
    let ctx = unsafe { &mut *(context as *mut AcpiEnumCtx) };
    if ctx.count >= ctx.max_count {
        info!("Warning: Discarding further device objects since we crossed max_count={}", ctx.max_count);
        return AE_OK;
    }

    let (hid, hid_len, adr, valid) = acpi_get_device_info(handle);

    // Root bridge detection — HID present and is PNP0A03 or PNP0A08
    let hid_str = core::str::from_utf8(&hid[..hid_len]).unwrap_or("");
    let is_root_bridge = hid_len > 0 && (hid_str == "PNP0A03" || hid_str == "PNP0A08");

    if is_root_bridge {
        // Create PDO for the root bridge (PCI driver loads on top)
        let pdo_ctx = alloc::boxed::Box::new_in(
            AcpiPdoCtx {
                handle, hid, hid_len,
                cids: [[0; 24]; MAX_CIDS],
                cid_lens: [0; MAX_CIDS],
                cid_count: 0
            },
            PoolAllocatorGlobal
        );
        let pdo_ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(pdo_ctx).0 as *mut c_void;

        let parent = unsafe { &*ctx.parent_device };
        let pdo = create_device_by_id(ctx.driver_id, None, pdo_ctx_ptr, Some(parent), false);

        if pdo.is_null() {
            unsafe {
                drop(alloc::boxed::Box::from_raw_in(pdo_ctx_ptr as *mut AcpiPdoCtx, PoolAllocatorGlobal));
            }
            return AE_OK;
        }

        unsafe { *ctx.buf.add(ctx.count) = pdo as *const DeviceObject; }
        ctx.count += 1;

        // Store handle for PCI IRQ routing
        let rb_idx = ROOT_BRIDGE_COUNT.fetch_add(1, Ordering::AcqRel);
        assert!(rb_idx < MAX_ROOT_BRIDGES, "too many PCI root bridges");
        ROOT_BRIDGE_HANDLES[rb_idx].store(handle as usize, Ordering::Release);

        let mut base_bus = 0u64;
        if acpi_evaluate_integer(handle, b"_BBN\0".as_ptr() as *const c_char, &mut base_bus) == AE_OK {
            ROOT_BRIDGE_BUS[rb_idx].store(base_bus as u8, Ordering::Release);
        }
        let mut seg = 0u64;
        if acpi_evaluate_integer(handle, b"_SEG\0".as_ptr() as *const c_char, &mut seg) == AE_OK {
            ROOT_BRIDGE_SEG[rb_idx].store(seg as u32, Ordering::Release);
        }
        info!("acpi: PCI root bridge {} stored at index {}, bus={}", hid_str, rb_idx, base_bus as u8);

        return AE_OK;
    }

    // Check for PCI child device — has _ADR but no _HID
    if hid_len == 0 && valid & ACPI_VALID_ADR_FLAG != 0 {
        let mut immediate_parent: AcpiHandle = core::ptr::null_mut();
        if acpi_get_parent(handle, &mut immediate_parent) != AE_OK || immediate_parent.is_null() {
            return AE_OK;
        }

        // Collect ancestor chain while walking up to find a root bridge.
        let mut chain = [core::ptr::null_mut::<c_void>(); 8];
        let mut chain_len = 0usize;
        let mut current = immediate_parent;
        let mut rb_idx = usize::MAX;

        loop {
            let rb_count = ROOT_BRIDGE_COUNT.load(Ordering::Acquire);
            // Terminate once we find the root bridge this device belongs to
            for i in 0..rb_count {
                if ROOT_BRIDGE_HANDLES[i].load(Ordering::Relaxed) == current as usize {
                    rb_idx = i;
                    break;
                }
            }
            if rb_idx != usize::MAX { break; }

            // Otherwise, add this bus device to the chain
            if chain_len < chain.len() {
                chain[chain_len] = current;
                chain_len += 1;
            }
            let mut next: AcpiHandle = core::ptr::null_mut();
            if acpi_get_parent(current, &mut next) != AE_OK || next.is_null() {
                return AE_OK;
            }
            current = next;
        }

        let seg = ROOT_BRIDGE_SEG[rb_idx].load(Ordering::Relaxed);  
        if seg != 0 {
            // Currently we haven't implemented the mechanism for reading 
            // config space for device under segments != 0
            info!("Skipping child device on seg {}", seg);
            return AE_OK;
        }

        // Derive bus by walking chain downward from root bridge, reading secondary bus at each level.
        // chain[chain_len-1] is on ROOT_BRIDGE_BUS; chain[0] is immediate_parent.
        let mut bus = ROOT_BRIDGE_BUS[rb_idx].load(Ordering::Relaxed);
        for i in (0..chain_len).rev() {
            let (_, _, bridge_adr, _) = acpi_get_device_info(chain[i]);
            let bridge_dev  = (bridge_adr >> 16) as u8;
            let bridge_func = (bridge_adr & 0xFF) as u8;
            bus = pci_cfg_read8(bus, bridge_dev, bridge_func, 0x19);
        }

        let child_idx = PCI_CHILD_COUNTS[rb_idx].fetch_add(1, Ordering::AcqRel);
        assert!(child_idx < MAX_PCI_CHILDREN, "too many PCI children under root bridge {}", rb_idx);

        let dev_num  = (adr >> 16) as u8;
        let func_num = (adr & 0xFF) as u8;
        let packed_addr = (bus as u32) << 24 | (dev_num as u32) << 16 | func_num as u32;
        PCI_CHILD_ADDRS[rb_idx * MAX_PCI_CHILDREN + child_idx].store(packed_addr, Ordering::Release);

        let gsi_base = (rb_idx * MAX_PCI_CHILDREN + child_idx) * 4;
        resolve_prt_for_child(immediate_parent, adr, gsi_base);

        return AE_OK;
    }

    // Regular ACPI device with a HID — create PDO
    if hid_len == 0 {
        return AE_OK;
    }

    info!("Found pnp hid {}", hid_str);

    let mut cids = [[0u8; 24]; MAX_CIDS];
    let mut cid_lens = [0usize; MAX_CIDS];
    let cid_count = acpi_get_cids(handle, &mut cids, &mut cid_lens);

    let pdo_ctx = alloc::boxed::Box::new_in(
        AcpiPdoCtx { handle, hid, hid_len, cids, cid_lens, cid_count },
        PoolAllocatorGlobal
    );
    let pdo_ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(pdo_ctx).0 as *mut c_void;

    let parent = unsafe { &*ctx.parent_device };
    let pdo = create_device_by_id(ctx.driver_id, None, pdo_ctx_ptr, Some(parent), false);

    if pdo.is_null() {
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(pdo_ctx_ptr as *mut AcpiPdoCtx, PoolAllocatorGlobal));
        }
        return AE_OK;
    }

    unsafe { *ctx.buf.add(ctx.count) = pdo as *const DeviceObject; }
    ctx.count += 1;

    AE_OK
}

fn do_query(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &*(device.ctx as *const AcpiPdoCtx) };
    let sref_buf = req.buffer.base_address as *mut StrRef;
    let max = req.buffer.size / core::mem::size_of::<StrRef>();
    let mut count = 0usize;

    if ctx.hid_len > 0 && count < max {
        unsafe { *sref_buf.add(count) = StrRef { ptr: ctx.hid.as_ptr(), len: ctx.hid_len }; }
        count += 1;
    }
    for i in 0..ctx.cid_count {
        if count >= max { break; }
        if ctx.cid_lens[i] > 0 {
            unsafe { *sref_buf.add(count) = StrRef { ptr: ctx.cids[i].as_ptr(), len: ctx.cid_lens[i] }; }
            count += 1;
        }
    }
    req.bytes_completed = count * core::mem::size_of::<StrRef>();
    req.complete_irp(Status::Success);
    Status::Success
}

fn do_resources(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &*(device.ctx as *const AcpiPdoCtx) };
    let res_list = unsafe { req.req_info.res_list };

    let entry_limit = res_list.count.min(MAX_RESOURCE_ENTRIES);
    let mut simple = [AcpiSimpleResource::default(); MAX_RESOURCE_ENTRIES];
    let count = acpi_get_resources(ctx.handle, simple.as_mut_ptr(), entry_limit);

    let fill_count = count.min(entry_limit);
    if fill_count > 0 {
        let entries = unsafe { core::slice::from_raw_parts_mut(res_list.base, fill_count) };
        for (i, s) in simple[..fill_count].iter().enumerate() {
            entries[i] = match s.res_type {
                0 => {
                    info!("Interrupt irq {}", s.address);
                    ResEntry {
                        res_type: ResType::Interrupt,
                        desc: ResTypeDesc { interrupt: IntDesc {
                            irq: s.address as usize,
                            vector: 0,
                            active_high: s.active_high,
                            edge_triggered: s.edge_triggered
                        }}
                    }
                }
                1 => {
                    info!("Port {} with range {}", s.address, s.length);
                    ResEntry {
                        res_type: ResType::Port,
                        desc: ResTypeDesc { port: PortDesc { base: s.address as usize, range: s.length as usize } }
                    }
                }
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

// Looks up IRQ routing for a PCI device. `addr = (device << 16) | function`, pin 0-3 = INTA-INTD.
// Returns true and fills `out` if a routing entry exists; false otherwise.
#[kmod::export]
fn acpi_get_irq(pdo: *const DeviceObject, bus: u8, addr: u32, pin: u8, out: *mut IrqInfo) -> bool {
    let ctx = unsafe { &*((*pdo).ctx as *const AcpiPdoCtx) };
    let rb_handle = ctx.handle as usize;
    let rb_count = ROOT_BRIDGE_COUNT.load(Ordering::Acquire);
    let dev = (addr >> 16) as u8;

    for rb_idx in 0..rb_count {
        if ROOT_BRIDGE_HANDLES[rb_idx].load(Ordering::Relaxed) != rb_handle { continue; }
        let child_count = PCI_CHILD_COUNTS[rb_idx].load(Ordering::Acquire);
        for child_idx in 0..child_count {
            let stored = PCI_CHILD_ADDRS[rb_idx * MAX_PCI_CHILDREN + child_idx].load(Ordering::Relaxed);
            if (stored >> 24) as u8 == bus && (stored >> 16) as u8 == dev {
                let packed = PCI_CHILD_GSIS[(rb_idx * MAX_PCI_CHILDREN + child_idx) * 4 + pin as usize]
                    .load(Ordering::Relaxed);
                if packed == u32::MAX { return false; }
                unsafe {
                    *out = IrqInfo {
                        gsi: packed & 0x0FFF_FFFF,
                        active_high: (packed >> 28) & 1 != 0,
                        edge_triggered: (packed >> 29) & 1 != 0
                    };
                }
                return true;
            }
        }
    }
    false
}

// Returns the base bus number and PCI segment for the root bridge identified by `pdo`.
#[kmod::export]
fn acpi_get_rb_info(pdo: *const DeviceObject, out_bus: *mut u8, out_seg: *mut u16) -> bool {
    let ctx = unsafe { &*((*pdo).ctx as *const AcpiPdoCtx) };
    let rb_handle = ctx.handle as usize;
    let rb_count = ROOT_BRIDGE_COUNT.load(Ordering::Acquire);
    for rb_idx in 0..rb_count {
        if ROOT_BRIDGE_HANDLES[rb_idx].load(Ordering::Relaxed) == rb_handle {
            unsafe {
                *out_bus = ROOT_BRIDGE_BUS[rb_idx].load(Ordering::Relaxed);
                *out_seg = ROOT_BRIDGE_SEG[rb_idx].load(Ordering::Relaxed) as u16;
            }
            return true;
        }
    }
    false
}

#[kmod::export]
fn acpi_pdo_get_handle(pdo_ctx: *const c_void) -> AcpiHandle {
    if pdo_ctx.is_null() { return core::ptr::null_mut(); }
    unsafe { &*(pdo_ctx as *const AcpiPdoCtx) }.handle
}

#[kmod::driver_unload]
fn unload(driver: &DriverObject) {
    panic!("Attempted to unload {} driver!", driver.get_name());
}
