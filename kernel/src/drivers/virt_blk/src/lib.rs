#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicUsize, Ordering};
use alloc::collections::BTreeMap;

use common::PAGE_SIZE;
use kernel_intf::{
    KError, KInterruptHandle, Lock, RemoveLock, acquire_spinlock, create_spinlock, info, io_complete_irp, io_create_driver_worker, io_get_device_type, io_install_interrupt_handler, io_remove_interrupt_handler, io_set_cancel_routine, io_start_processing, release_spinlock
};
use kernel_intf::driver::{
    DeviceObject, DeviceType, DiskInfo, DriverObject, Irp, IrpMinor, ResEntry, ResType, Status,
    create_device
};
use kernel_intf::mem::{PageDescriptor, alloc_dma_memory, free_dma_memory, get_physical_address, map_mmio_region, unmap_mmio_region};
use kernel_intf::hw::{pci_cfg_read8, pci_cfg_read32};
use kernel_intf::ds::RingBuffer;
use kmod::dispatch_init;

const VIRTIO_PCI_CAP_VENDOR_ID:  u8 = 0x09;
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG:    u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const VIRTIO_STATUS_ACKNOWLEDGE:        u8 = 1;
const VIRTIO_STATUS_DRIVER:             u8 = 2;
const VIRTIO_STATUS_DRIVER_OK:          u8 = 4;
const VIRTIO_STATUS_FEATURES_OK:        u8 = 8;
const VIRTIO_STATUS_DEVICE_NEEDS_RESET: u8 = 64;
const VIRTIO_STATUS_FAILED:             u8 = 128;

const VIRTIO_F_VERSION_1_BIT: u32 = 32;

const VIRTQ_DESC_F_NEXT:     u16 = 1;
const VIRTQ_DESC_F_WRITE:    u16 = 2;
const VIRTQ_DESC_F_INDIRECT: u16 = 4;

const VIRTIO_BLK_T_IN:     u32 = 0;   // read
const VIRTIO_BLK_T_OUT:    u32 = 1;   // write
const VIRTIO_BLK_T_FLUSH:  u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;

const VIRTIO_BLK_S_OK:     u8 = 0;
const VIRTIO_BLK_S_IOERR:  u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const MAX_QUEUE_SIZE:   usize = 128;
const MAX_DESC_PER_REQ: usize = 3;   // header + data + status
const MAX_PENDING:      usize = 16;

const SECTOR_SIZE:      usize = 512;

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr:  u64,   // physical address of the buffer
    len:   u32,
    flags: u16,
    next:  u16    // next descriptor index in the chain, only meaningful if F_NEXT set
}

#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx:   u16,
    ring:  [u16; MAX_QUEUE_SIZE]
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqUsedElem { id: u32, len: u32 }

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx:   u16,
    ring:  [VirtqUsedElem; MAX_QUEUE_SIZE]
}

#[repr(C)]
struct VirtioPciCommonCfg {
    device_feature_select: u32,
    device_feature:        u32,
    driver_feature_select: u32,
    driver_feature:        u32,
    msix_config:            u16,
    num_queues:             u16,
    device_status:           u8,
    config_generation:       u8,
    queue_select:           u16,
    queue_size:             u16,
    queue_msix_vector:      u16,
    queue_enable:           u16,
    queue_notify_off:       u16,
    queue_desc:             u64,
    queue_driver:           u64,   // avail ring phys addr
    queue_device:           u64    // used ring phys addr
}

// One vendor-specific virtio-pci capability, as discovered by walking the
// PCI capability list.
#[derive(Clone, Copy)]
struct VirtioPciCap {
    cfg_type: u8,      // 1=COMMON, 2=NOTIFY, 3=ISR, 4=DEVICE, 5=PCI
    bar:      u8,
    offset:   u32,     // offset within the BAR
    length:   u32,
    notify_off_multiplier: u32   // only meaningful when cfg_type == NOTIFY_CFG
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioBlkReqHeader {
    req_type: u32,
    reserved: u32,
    sector:   u64
}

struct VirtQueue {
    size: u16,                                     // negotiated queue_size, <= MAX_QUEUE_SIZE
    desc_virt: *mut VirtqDesc,
    desc_phys: usize,    // array of MAX_QUEUE_SIZE VirtqDesc
    avail_virt: *mut VirtqAvail,
    avail_phys: usize,
    used_virt: *mut VirtqUsed,
    used_phys: usize,
    last_used_idx: u16,                             // driver's last-seen used.idx, for the DW to detect new completions
    free_desc: RingBuffer<u16, MAX_QUEUE_SIZE>,     // free descriptor index list
    notify_off: u16,
    hdr_pool: *mut VirtioBlkReqHeader,
    hdr_pool_phys: usize,
    free_hdr: RingBuffer<u16, MAX_QUEUE_SIZE>,
    status_pool: *mut u8,
    status_pool_phys: usize,
    free_status: RingBuffer<u16, MAX_QUEUE_SIZE>
}

impl VirtQueue {
    const fn zeroed() -> Self {
        Self {
            size: 0,
            desc_virt: core::ptr::null_mut(), desc_phys: 0,
            avail_virt: core::ptr::null_mut(), avail_phys: 0,
            used_virt: core::ptr::null_mut(), used_phys: 0,
            last_used_idx: 0,
            free_desc: RingBuffer::new(0),
            notify_off: 0,
            hdr_pool: core::ptr::null_mut(),
            hdr_pool_phys: 0,
            free_hdr: RingBuffer::new(0),
            status_pool: core::ptr::null_mut(),
            status_pool_phys: 0,
            free_status: RingBuffer::new(0)
        }
    }
}

#[derive(Clone, Copy)]
struct PendingEntry {
    irp:         *mut Irp,
    desc_count:  u8,                        // how many chained descriptors to free on completion
    descs:       [u16; MAX_DESC_PER_REQ],
    hdr_slot:    u16,                       // ctx.queue.hdr_pool slot in use 
    status_slot: u16                        // ctx.queue.status_pool slot in use
}

impl PendingEntry {
    const fn zeroed() -> Self {
        Self { irp: core::ptr::null_mut(), desc_count: 0, descs: [0; MAX_DESC_PER_REQ], hdr_slot: 0, status_slot: 0 }
    }
}

struct AllocatedResource {
    d_req_id: u16,
    d_buf_id: u16,
    d_stat_id: u16,
    d_req: *mut VirtqDesc,
    d_buf: *mut VirtqDesc,
    d_stat: *mut VirtqDesc,
    req_slot_id: u16,
    req_hdr: *mut VirtioBlkReqHeader,   // virtual -- for writing the header
    req_hdr_phys: u64,                  // physical -- for the descriptor's addr field
    stat_slot_id: u16,
    stat_addr: u64                      // physical -- status is only ever read back through ctx.queue.status_pool directly
}

struct VirtBlkCtx {
    lock: Lock,
    remove_lock: RemoveLock,
    pdo: *const DeviceObject,           
    bus: u8,
    device: u8, 
    function: u8,  
    common_cfg: *mut VirtioPciCommonCfg, 
    notify_base: *mut u8, 
    isr_cfg: *mut u8, 
    device_cfg: *mut u8,
    common_cfg_size: usize, 
    notify_base_size: usize, 
    isr_cfg_size: usize, 
    device_cfg_size: usize, 
    notify_off_multiplier: u32,
    queue: VirtQueue,               // single request queue 
    interrupt_handle: KInterruptHandle,
    pending: BTreeMap<u16, PendingEntry> // outstanding read/write IRPs 
}

unsafe impl Send for VirtBlkCtx {}
unsafe impl Sync for VirtBlkCtx {}

impl VirtBlkCtx {
    const fn zeroed(pdo: *const DeviceObject) -> Self {
        Self {
            lock: Lock::new(),
            remove_lock: RemoveLock::new(),
            pdo,
            bus: 0, device: 0, function: 0,
            common_cfg: core::ptr::null_mut(), 
            notify_base: core::ptr::null_mut(),
            isr_cfg: core::ptr::null_mut(), 
            device_cfg: core::ptr::null_mut(),
            common_cfg_size: 0, 
            notify_base_size: 0, 
            isr_cfg_size: 0, 
            device_cfg_size: 0,
            notify_off_multiplier: 0,
            queue: VirtQueue::zeroed(),
            interrupt_handle: KInterruptHandle::new(),
            pending: BTreeMap::new()
        }
    }
}

static DEVICE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("Initializing {} driver", driver.get_name());

    dispatch_init!(driver, dispatch_add, dispatch_pnp, dispatch_control, dispatch_read, dispatch_write);

    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: &DeviceObject) -> Status {
    if io_get_device_type(pdo) != DeviceType::Pci {
        info!("virt_blk: only pci parent devices are supported");
        return Status::Failed;
    }

    let idx = DEVICE_COUNT.fetch_add(1, Ordering::Relaxed);
    let name: &'static str = alloc::boxed::Box::leak(alloc::format!("vblk{}", idx).into_boxed_str());

    let mut ctx = alloc::boxed::Box::new(VirtBlkCtx::zeroed(pdo as *const DeviceObject));
    create_spinlock(&mut ctx.lock);

    let ctx_ptr = alloc::boxed::Box::into_raw(ctx) as *mut c_void;

    let dev = create_device(driver, Some(name), ctx_ptr, Some(pdo), false, DeviceType::None);
    if dev.is_null() {
        info!("virt_blk: create_device failed for '{}'", name);
        unsafe {
            drop(alloc::boxed::Box::from_raw(ctx_ptr as *mut VirtBlkCtx));
            drop(alloc::boxed::Box::from_raw(name as *const str as *mut str));
        }
        return Status::Failed;
    }

    info!("virt_blk: added device '{}'", name);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, req: &mut Irp) -> Status {
    match req.minor_code {
        IrpMinor::Start => {
            do_start(device, req)
        },
        IrpMinor::Stop => {
            do_stop(device, req)
        },
        IrpMinor::Remove => {
            do_remove(device, req)
        },
        _ => {
            Status::Unsupported
        }
    }
}

#[kmod::dispatch_handler]
fn dispatch_control(device: &DeviceObject, request: &mut Irp) -> Status {
    match request.minor_code {
        IrpMinor::DiskGetInfo => do_get_info(device, request),
        _ => Status::Unsupported
    }
}

fn do_get_info(device: &DeviceObject, request: &mut Irp) -> Status {
    let ctx = unsafe { &*(device.ctx as *const VirtBlkCtx) };
    if ctx.device_cfg.is_null() {
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }
    // virtio_blk_config::capacity is always the first field, in 512-byte
    // sectors, regardless of negotiated block size (virtio 1.x spec).
    let capacity = unsafe { core::ptr::read_volatile(ctx.device_cfg as *const u64) };
    request.req_info.disk_info = DiskInfo { lba_size: SECTOR_SIZE, lba_count: capacity };
    request.complete_irp(Status::Success);
    Status::Success
}

#[derive(Default)]
struct CapCollector {
    bus: u8,
    device: u8,
    function: u8,
    common_cap: Option<VirtioPciCap>,
    notify_cap: Option<VirtioPciCap>,
    isr_cap: Option<VirtioPciCap>,
    device_cap: Option<VirtioPciCap>
}

extern "C" fn collect_vendor_cap(bus: u8, device: u8, function: u8, offset: u8, ctx: *mut c_void) {
    let collector = unsafe { &mut *(ctx as *mut CapCollector) };
    collector.bus = bus;
    collector.device = device;
    collector.function = function;

    let cfg_type = pci_cfg_read8(bus, device, function, offset + 3);
    let bar = pci_cfg_read8(bus, device, function, offset + 4);
    let cap_offset = pci_cfg_read32(bus, device, function, offset + 8);
    let length = pci_cfg_read32(bus, device, function, offset + 12);
    let notify_off_multiplier = if cfg_type == VIRTIO_PCI_CAP_NOTIFY_CFG {
        pci_cfg_read32(bus, device, function, offset + 16)
    } else {
        0
    };

    let cap = VirtioPciCap { cfg_type, bar, offset: cap_offset, length, notify_off_multiplier };
    match cfg_type {
        VIRTIO_PCI_CAP_COMMON_CFG => collector.common_cap = Some(cap),
        VIRTIO_PCI_CAP_NOTIFY_CFG => collector.notify_cap = Some(cap),
        VIRTIO_PCI_CAP_ISR_CFG => collector.isr_cap = Some(cap),
        VIRTIO_PCI_CAP_DEVICE_CFG => collector.device_cap = Some(cap),
        _ => {}
    }
}

fn read_bar_base(bus: u8, device: u8, function: u8, bar_idx: u8) -> u64 {
    let off = 0x10u8 + bar_idx * 4;
    let bar = pci_cfg_read32(bus, device, function, off);
    let base_lo = (bar & !0xF) as u64;
    if (bar >> 1) & 3 == 2 {
        let hi = pci_cfg_read32(bus, device, function, off + 4);
        base_lo | ((hi as u64) << 32)
    } else {
        base_lo
    }
}

fn mmio_len(len: u32) -> usize {
    (len as usize).max(1).next_multiple_of(PAGE_SIZE)
}

fn map_cap(bus: u8, device: u8, function: u8, cap: &VirtioPciCap) -> Result<*mut u8, KError> {
    let bar_base = read_bar_base(bus, device, function, cap.bar);
    let phys = bar_base + cap.offset as u64;
    map_mmio_region(phys as usize, mmio_len(cap.length))
}

fn do_start_inner(ctx: &mut VirtBlkCtx, req: &Irp) -> Result<(), KError> {
    let mut caps = CapCollector::default();
    unsafe {
        pci::walk_pci_cap_list(
            ctx.pdo, VIRTIO_PCI_CAP_VENDOR_ID, collect_vendor_cap,
            &mut caps as *mut CapCollector as *mut c_void
        );
    }

    ctx.bus = caps.bus;
    ctx.device = caps.device;
    ctx.function = caps.function;
    let (bus, pci_device, function) = (caps.bus, caps.device, caps.function);

    let (common_cap, notify_cap, isr_cap, device_cap) =
        match (caps.common_cap, caps.notify_cap, caps.isr_cap, caps.device_cap) {
            (Some(c), Some(n), Some(i), Some(d)) => (c, n, i, d),
            _ => {
                info!("virt_blk: device is missing a required virtio-pci capability");
                return Err(KError::NotFound);
            }
        };

    ctx.notify_off_multiplier = notify_cap.notify_off_multiplier;
    ctx.common_cfg_size = mmio_len(common_cap.length);
    ctx.notify_base_size = mmio_len(notify_cap.length);
    ctx.isr_cfg_size = mmio_len(isr_cap.length);
    ctx.device_cfg_size = mmio_len(device_cap.length);

    ctx.common_cfg = map_cap(bus, pci_device, function, &common_cap)? as *mut VirtioPciCommonCfg;
    ctx.notify_base = map_cap(bus, pci_device, function, &notify_cap)?;
    ctx.isr_cfg = map_cap(bus, pci_device, function, &isr_cap)?;
    ctx.device_cfg = map_cap(bus, pci_device, function, &device_cap)?;

    // virtio device initialization handshake (virtio 1.x spec)
    let common = ctx.common_cfg;
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).device_status), 0u8);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).device_status), VIRTIO_STATUS_ACKNOWLEDGE);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).device_status), VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

        // Negotiate VIRTIO_F_VERSION_1 only -- this device id (0x1042) is the
        // modern-only variant, so nothing else is required for a minimal driver.
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).device_feature_select), 1u32);
        let hi_features = core::ptr::read_volatile(core::ptr::addr_of!((*common).device_feature));
        if hi_features & (1u32 << (VIRTIO_F_VERSION_1_BIT - 32)) == 0 {
            info!("virt_blk: device does not offer VIRTIO_F_VERSION_1");
            return Err(KError::Unsupported);
        }
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).driver_feature_select), 1u32);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).driver_feature), 1u32 << (VIRTIO_F_VERSION_1_BIT - 32));

        let mut status = VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK;
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).device_status), status);
        let readback = core::ptr::read_volatile(core::ptr::addr_of!((*common).device_status));
        if readback & VIRTIO_STATUS_FEATURES_OK == 0 {
            info!("virt_blk: device rejected feature set (FEATURES_OK did not stick)");
            return Err(KError::Unsupported);
        }

        // queue 0 setup 
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).queue_select), 0u16);
        let device_queue_size = core::ptr::read_volatile(core::ptr::addr_of!((*common).queue_size));
        let queue_size = (device_queue_size as usize).clamp(1, MAX_QUEUE_SIZE) as u16;

        let (desc_virt, desc_phys) = alloc_dma_memory(size_of::<[VirtqDesc; MAX_QUEUE_SIZE]>(), 16)?;
        let (avail_virt, avail_phys) = alloc_dma_memory(size_of::<VirtqAvail>(), 2)?;
        let (used_virt, used_phys) = alloc_dma_memory(size_of::<VirtqUsed>(), 4)?;
        let (hdr_pool_virt, hdr_pool_phys) = alloc_dma_memory(size_of::<[VirtioBlkReqHeader; MAX_QUEUE_SIZE]>(), 8)?;
        let (status_pool_virt, status_pool_phys) = alloc_dma_memory(MAX_QUEUE_SIZE, 1)?;
        core::ptr::write_bytes(desc_virt, 0, size_of::<[VirtqDesc; MAX_QUEUE_SIZE]>());
        core::ptr::write_bytes(avail_virt, 0, size_of::<VirtqAvail>());
        core::ptr::write_bytes(used_virt, 0, size_of::<VirtqUsed>());
        core::ptr::write_bytes(hdr_pool_virt, 0, size_of::<[VirtioBlkReqHeader; MAX_QUEUE_SIZE]>());
        core::ptr::write_bytes(status_pool_virt, 0, MAX_QUEUE_SIZE);

        ctx.queue.size = queue_size;
        ctx.queue.desc_virt = desc_virt as *mut VirtqDesc;
        ctx.queue.desc_phys = desc_phys;
        ctx.queue.avail_virt = avail_virt as *mut VirtqAvail;
        ctx.queue.avail_phys = avail_phys;
        ctx.queue.used_virt = used_virt as *mut VirtqUsed;
        ctx.queue.used_phys = used_phys;
        ctx.queue.last_used_idx = 0;
        ctx.queue.hdr_pool = hdr_pool_virt as *mut VirtioBlkReqHeader;
        ctx.queue.hdr_pool_phys = hdr_pool_phys;
        ctx.queue.status_pool = status_pool_virt;
        ctx.queue.status_pool_phys = status_pool_phys;
        kernel_intf::debug!("hdr_pool_phys:{:#X}, status_pool_phys:{:#X}", hdr_pool_phys, status_pool_phys);

        // All descriptors, and all preallocated header/status slots, are
        // free initially.
        ctx.queue.free_desc = RingBuffer::new(0);
        ctx.queue.free_hdr = RingBuffer::new(0);
        ctx.queue.free_status = RingBuffer::new(0);
        for i in 0..queue_size {
            ctx.queue.free_desc.push(i);
            ctx.queue.free_hdr.push(i);
            ctx.queue.free_status.push(i);
        }
        ctx.queue.notify_off = core::ptr::read_volatile(core::ptr::addr_of!((*common).queue_notify_off));

        // Tell the device about the queue physical addresses
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).queue_size), queue_size);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).queue_desc), desc_phys as u64);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).queue_driver), avail_phys as u64);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).queue_device), used_phys as u64);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).queue_enable), 1u16);

        status |= VIRTIO_STATUS_DRIVER_OK;
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*common).device_status), status);
    }

    //  Setup interrupt handler (IRQ mechanism)
    let res_list = unsafe { req.req_info.res_list };
    let res_slice: &[ResEntry] = unsafe { core::slice::from_raw_parts(res_list.base, res_list.count) };
    let mut irq = 0usize;
    let mut vector = 0usize;
    let mut active_high = true;
    let mut edge_triggered = true;
    for entry in res_slice {
        if let ResType::Interrupt = entry.res_type {
            irq = unsafe { entry.desc.interrupt.irq };
            vector = unsafe { entry.desc.interrupt.vector };
            active_high = unsafe { entry.desc.interrupt.active_high };
            edge_triggered = unsafe { entry.desc.interrupt.edge_triggered };
            break;
        }
    }

    ctx.interrupt_handle = io_install_interrupt_handler(
        vector, irq as isize, ctx as *mut VirtBlkCtx as *mut c_void,
        virtblk_isr, active_high, edge_triggered
    );

    Ok(())
}

fn do_start(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &mut *(device.ctx as *mut VirtBlkCtx) };

    match do_start_inner(ctx, req) {
        Ok(()) => {
            info!("virt_blk: started '{}'", device.get_name().unwrap_or("?"));
            req.complete_irp(Status::Success);
            Status::Success
        }
        Err(e) => {
            info!("virt_blk: do_start failed: {}", e);
            req.complete_irp(Status::Failed);
            Status::Failed
        }
    }
}

fn teardown_resources(ctx: &mut VirtBlkCtx) {
    if !ctx.common_cfg.is_null() {
        let _ = unmap_mmio_region(ctx.common_cfg as *mut u8, ctx.common_cfg_size);
        ctx.common_cfg = core::ptr::null_mut();
    }
    if !ctx.notify_base.is_null() {
        let _ = unmap_mmio_region(ctx.notify_base, ctx.notify_base_size);
        ctx.notify_base = core::ptr::null_mut();
    }
    if !ctx.isr_cfg.is_null() {
        let _ = unmap_mmio_region(ctx.isr_cfg, ctx.isr_cfg_size);
        ctx.isr_cfg = core::ptr::null_mut();
    }
    if !ctx.device_cfg.is_null() {
        let _ = unmap_mmio_region(ctx.device_cfg, ctx.device_cfg_size);
        ctx.device_cfg = core::ptr::null_mut();
    }

    if !ctx.queue.desc_virt.is_null() {
        let _ = free_dma_memory(ctx.queue.desc_virt as *mut u8, size_of::<[VirtqDesc; MAX_QUEUE_SIZE]>(), 16);
        ctx.queue.desc_virt = core::ptr::null_mut();
    }
    if !ctx.queue.avail_virt.is_null() {
        let _ = free_dma_memory(ctx.queue.avail_virt as *mut u8, size_of::<VirtqAvail>(), 2);
        ctx.queue.avail_virt = core::ptr::null_mut();
    }
    if !ctx.queue.used_virt.is_null() {
        let _ = free_dma_memory(ctx.queue.used_virt as *mut u8, size_of::<VirtqUsed>(), 4);
        ctx.queue.used_virt = core::ptr::null_mut();
    }
    if !ctx.queue.hdr_pool.is_null() {
        let _ = free_dma_memory(ctx.queue.hdr_pool as *mut u8, size_of::<[VirtioBlkReqHeader; MAX_QUEUE_SIZE]>(), 8);
        ctx.queue.hdr_pool = core::ptr::null_mut();
    }
    if !ctx.queue.status_pool.is_null() {
        let _ = free_dma_memory(ctx.queue.status_pool, MAX_QUEUE_SIZE, 1);
        ctx.queue.status_pool = core::ptr::null_mut();
    }
}

fn release_request_resources(hdr_id: u16, ctx: &mut VirtBlkCtx) -> Option<(*mut Irp, u8)> {
    match ctx.pending.remove(&hdr_id) {
        Some(val) => {
            // Free the request header and status back to pool
            ctx.queue.free_hdr.push(val.hdr_slot);
            ctx.queue.free_status.push(val.status_slot);
            
            // Free the 3 descriptors
            ctx.queue.free_desc.push(val.descs[0]);
            ctx.queue.free_desc.push(val.descs[1]);
            ctx.queue.free_desc.push(val.descs[2]);

            let status = unsafe {
                core::ptr::read_volatile(ctx.queue.status_pool.add(val.status_slot as usize))
            };

            Some((val.irp, status))
        },
        _ => { None }
    }
}


fn do_stop(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &mut *(device.ctx as *mut VirtBlkCtx) };

    if !ctx.common_cfg.is_null() {
        // Device-side reset -- halts DMA immediately, before we unmap
        // common_cfg below.
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!((*ctx.common_cfg).device_status), 0u8); }
    }

    io_remove_interrupt_handler(ctx.interrupt_handle);

    acquire_spinlock(&mut ctx.lock);
    let to_fail_hdr: alloc::vec::Vec<u16> = ctx.pending.iter().map(|e| {*e.0}).collect();
    let mut to_fail = [core::ptr::null_mut(); MAX_PENDING];
    let mut to_fail_len = 0;
    for id in to_fail_hdr {
        match release_request_resources(id, ctx) {
            Some(v) => { 
                to_fail[to_fail_len] = v.0;
                to_fail_len += 1;
            },
            None => {
                panic!("release_request() failed to release resource for virtblk descriptor!");
            }
        }
    }
    ctx.pending.clear();
    release_spinlock(&mut ctx.lock);

    for i in 0..to_fail_len {
        io_complete_irp(to_fail[i], Status::Failed);
    }

    teardown_resources(ctx);

    info!("virt_blk: stop '{}'", device.get_name().unwrap_or("?"));
    req.complete_irp(Status::Success);
    Status::Success
}

fn do_remove(device: &DeviceObject, req: &mut Irp) -> Status {
    if !device.ctx.is_null() {
        let ctx = unsafe { &mut *(device.ctx as *mut VirtBlkCtx) };

        teardown_resources(ctx);

        // If a virtblk_isr/virtblk_dw call is still queued against this ctx,
        // it holds the last reference and will free it when it releases.
        if ctx.remove_lock.begin_remove() {
            unsafe { drop(alloc::boxed::Box::from_raw(device.ctx as *mut VirtBlkCtx)); }
        }
    }

    req.complete_irp(Status::Success);
    Status::Success
}

extern "C" fn virtblk_isr(ctx_ptr: *mut c_void) -> bool {
    let ctx = unsafe { &mut *(ctx_ptr as *mut VirtBlkCtx) };
    if !ctx.remove_lock.acquire() { return true; }

    // Reading the ISR status byte both tells us the interrupt reason and
    // acknowledges the interrupt
    let _isr_status = unsafe { core::ptr::read_volatile(ctx.isr_cfg) };

    if io_create_driver_worker(virtblk_dw, ctx_ptr).is_err() {
        // Nothing will ever call virtblk_dw's release() for this reference
        // now -- release it ourselves.
        if ctx.remove_lock.release() {
            unsafe { drop(alloc::boxed::Box::from_raw(ctx_ptr as *mut VirtBlkCtx)); }
        }
    }

    true
}

extern "C" fn virtblk_dw(ctx_ptr: *mut c_void) {
    let ctx = unsafe { &mut *(ctx_ptr as *mut VirtBlkCtx) };

    let mut completed_requests = [(core::ptr::null_mut(), 0u8); MAX_PENDING];
    let mut completed_len = 0;
    acquire_spinlock(&mut ctx.lock);
    unsafe {
        let idx = core::ptr::read_volatile(core::ptr::addr_of!((*ctx.queue.used_virt).idx));
        let mut last_used_idx = ctx.queue.last_used_idx;

        while last_used_idx != idx {
            let ring_slot = (last_used_idx as usize) % (ctx.queue.size as usize);
            let used_desc = core::ptr::read_volatile(core::ptr::addr_of!((*ctx.queue.used_virt).ring[ring_slot]));

            match release_request_resources(used_desc.id as u16, ctx) {
                Some(v) => {
                    completed_requests[completed_len] = v;
                    completed_len += 1;
                },
                None => {}
            }
            last_used_idx = last_used_idx.wrapping_add(1);
        }

        ctx.queue.last_used_idx = last_used_idx;
    }

    release_spinlock(&mut ctx.lock);

    for i in 0..completed_len {
        let status = if completed_requests[i].1 == VIRTIO_BLK_S_OK {
            Status::Success
        }
        else {
            Status::Failed
        };

        io_complete_irp(completed_requests[i].0, status); 
    }

    // Release the reference taken in virtblk_isr; if do_remove already ran
    // and this was the last outstanding reference, we free ctx.
    if ctx.remove_lock.release() {
        unsafe { drop(alloc::boxed::Box::from_raw(ctx_ptr as *mut VirtBlkCtx)); }
    }
}

fn allocate_resources(ctx: &mut VirtBlkCtx) -> Option<AllocatedResource> {
    if ctx.pending.len() == MAX_PENDING
    || ctx.queue.free_desc.len() < 3
    || ctx.queue.free_hdr.len() < 1
    || ctx.queue.free_status.len() < 1
    {
        return None;
    }

    let mut d_req_id: u16 = 0;
    let mut d_buf_id: u16 = 0;
    let mut d_stat_id: u16 = 0;
    unsafe {
        ctx.queue.free_desc.dequeue_into(&mut d_req_id as *mut u16, 1);
        ctx.queue.free_desc.dequeue_into(&mut d_buf_id as *mut u16, 1);
        ctx.queue.free_desc.dequeue_into(&mut d_stat_id as *mut u16, 1);

        // Allocate the 3 descriptors
        let d_req = ctx.queue.desc_virt.add(d_req_id as usize);
        let d_buf = ctx.queue.desc_virt.add(d_buf_id as usize);
        let d_stat = ctx.queue.desc_virt.add(d_stat_id as usize);

        // Allocate space required to submit the request header and for device to write the status to
        let req_slot_id = ctx.queue.free_hdr.pop_back().expect("Request header id cannot be None");
        let stat_slot_id = ctx.queue.free_status.pop_back().expect("Request header id cannot be None");

        let req_hdr: *mut VirtioBlkReqHeader = ctx.queue.hdr_pool.add(req_slot_id as usize);
        let req_hdr_phys = (ctx.queue.hdr_pool_phys + req_slot_id as usize * size_of::<VirtioBlkReqHeader>()) as u64;
        let stat_addr = (ctx.queue.status_pool_phys + stat_slot_id as usize) as u64;

        Some(AllocatedResource { d_req_id, d_buf_id, d_stat_id, d_req, d_buf, d_stat, req_slot_id, req_hdr, req_hdr_phys, stat_slot_id, stat_addr })
    }
}

fn submit_request(ctx: &mut VirtBlkCtx, req_id: u16) {
    unsafe {
        let avail = ctx.queue.avail_virt;

        // Append request to input ring buffer
        let idx = core::ptr::read_volatile(core::ptr::addr_of!((*avail).idx));
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*avail).ring[idx as usize % ctx.queue.size as usize]), req_id);

        core::sync::atomic::fence(Ordering::SeqCst);
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*avail).idx), idx.wrapping_add(1));

        core::sync::atomic::fence(Ordering::SeqCst);
        let notify_addr = ctx.notify_base.add(
        ctx.queue.notify_off as usize * ctx.notify_off_multiplier as usize
        ) as *mut u16;

        // Ring the doorbell
        core::ptr::write_volatile(notify_addr, 0); 
    }
}

#[kmod::dispatch_handler]
fn dispatch_read(device: &DeviceObject, request: &mut Irp) -> Status {
    if request.buffer.base_address == 0 || request.buffer.size == 0 || request.buffer.size % SECTOR_SIZE != 0 {
        info!("Invalid parameters");
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }


    let start_sector = request.offset as u64;
    
    let base_addr = match get_physical_address(request.buffer.base_address, PageDescriptor::VIRTUAL) {
        Ok(v) => v as u64,
        Err(e) => {
            info!("Error {:?} on trying to fetch caller buffer physical address", e);
            request.complete_irp(Status::Failed);
            return Status::Failed;
        }   
    };
    let ctx_ptr = device.ctx;

    let ctx = unsafe { &mut *(ctx_ptr as *mut VirtBlkCtx) };
    acquire_spinlock(&mut ctx.lock);
    let resource = match allocate_resources(ctx) {
        Some(r) => r,
        None => {
            info!("Unable to allocate sufficient resources for read request");
            release_spinlock(&mut ctx.lock);
            request.complete_irp(Status::Failed);
            return Status::Failed;
        }
    };

    // Allocate irp entry
    let irp_entry = PendingEntry {
        irp: request as *mut _,
        desc_count: 3,
        descs: [resource.d_req_id, resource.d_buf_id, resource.d_stat_id],
        hdr_slot: resource.req_slot_id,
        status_slot: resource.stat_slot_id
    };

    if ctx.pending.insert(resource.d_req_id, irp_entry).is_some() {
        panic!("virt_blk: duplicate pending entry for descriptor {}", resource.d_req_id);
    }

    // Fill the read request params
    unsafe {
        resource.req_hdr.write(VirtioBlkReqHeader { req_type: VIRTIO_BLK_T_IN, reserved: 0, sector: start_sector });
        resource.d_req.write(VirtqDesc {
            addr: resource.req_hdr_phys,
            len: size_of::<VirtioBlkReqHeader>() as u32,
            flags: VIRTQ_DESC_F_NEXT,
            next: resource.d_buf_id as u16
        });
        resource.d_buf.write(VirtqDesc {
            addr: base_addr,
            len: request.buffer.size as u32,
            flags: VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            next: resource.d_stat_id as u16
        });
        resource.d_stat.write(VirtqDesc {
            addr: resource.stat_addr,
            len: 1u32,
            flags: VIRTQ_DESC_F_WRITE,
            next: 0
        });
    }

    submit_request(ctx, resource.d_req_id);
    release_spinlock(&mut ctx.lock);

    // Request will be completed once dma is done
    Status::Pending
}

#[kmod::dispatch_handler]
fn dispatch_write(device: &DeviceObject, request: &mut Irp) -> Status {
    if request.buffer.base_address == 0 || request.buffer.size == 0 || request.buffer.size % SECTOR_SIZE != 0 {
        info!("Invalid parameters");
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let start_sector = request.offset as u64;
    
    let base_addr = match get_physical_address(request.buffer.base_address, PageDescriptor::VIRTUAL) {
        Ok(v) => v as u64,
        Err(e) => {
            info!("Error {:?} on trying to fetch caller buffer physical address", e);
            request.complete_irp(Status::Failed);
            return Status::Failed;
        }   
    };
    
    let ctx_ptr = device.ctx;

    let ctx = unsafe { &mut *(ctx_ptr as *mut VirtBlkCtx) };
    acquire_spinlock(&mut ctx.lock);
    let resource = match allocate_resources(ctx) {
        Some(r) => r,
        None => {
            info!("Unable to allocate sufficient resources for read request");
            release_spinlock(&mut ctx.lock);
            request.complete_irp(Status::Failed);
            return Status::Failed;
        }
    };

    // Allocate irp entry
    let irp_entry = PendingEntry {
        irp: request as *mut _,
        desc_count: 3,
        descs: [resource.d_req_id, resource.d_buf_id, resource.d_stat_id],
        hdr_slot: resource.req_slot_id,
        status_slot: resource.stat_slot_id
    };

    if ctx.pending.insert(resource.d_req_id, irp_entry).is_some() {
        panic!("virt_blk: duplicate pending entry for descriptor {}", resource.d_req_id);
    }

    // Fill the write request params
    unsafe {
        resource.req_hdr.write(VirtioBlkReqHeader { req_type: VIRTIO_BLK_T_OUT, reserved: 0, sector: start_sector });
        resource.d_req.write(VirtqDesc {
            addr: resource.req_hdr_phys,
            len: size_of::<VirtioBlkReqHeader>() as u32,
            flags: VIRTQ_DESC_F_NEXT,
            next: resource.d_buf_id as u16
        });
        resource.d_buf.write(VirtqDesc {
            addr: base_addr,
            len: request.buffer.size as u32,
            flags: VIRTQ_DESC_F_NEXT,
            next: resource.d_stat_id as u16
        });
        resource.d_stat.write(VirtqDesc {
            addr: resource.stat_addr,
            len: 1u32,
            flags: VIRTQ_DESC_F_WRITE,
            next: 0
        });
    }

    submit_request(ctx, resource.d_req_id);
    release_spinlock(&mut ctx.lock);

    Status::Pending
}
