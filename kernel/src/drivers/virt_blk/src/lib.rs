#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicUsize, Ordering};

use common::PAGE_SIZE;
use kernel_intf::{
    KError, KInterruptHandle, Lock, RemoveLock,
    acquire_spinlock, create_spinlock, release_spinlock,
    info,
    io_complete_irp, io_create_driver_worker, io_get_device_type,
    io_install_interrupt_handler, io_remove_interrupt_handler
};
use kernel_intf::driver::{
    DeviceObject, DeviceType, DriverObject, Irp, IrpMinor, ResEntry, ResType, Status,
    create_device
};
use kernel_intf::mem::{PoolAllocatorGlobal, alloc_dma_memory, free_dma_memory, map_mmio_region, unmap_mmio_region};
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
    notify_off: u16
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
            notify_off: 0
        }
    }
}

// Descriptor indices used by one in-flight request (header, data, status =
// 3, but keep this generic in case a request ever needs a scattered data
// buffer).
#[derive(Clone, Copy)]
struct PendingEntry {
    irp:         *mut Irp,
    head_desc:   u16,                       // first descriptor index of this request's chain
    desc_count:  u8,                        // how many chained descriptors to free on completion
    descs:       [u16; MAX_DESC_PER_REQ],
    status_virt: *mut u8                   
}

impl PendingEntry {
    const fn zeroed() -> Self {
        Self { irp: core::ptr::null_mut(), head_desc: 0, desc_count: 0, descs: [0; MAX_DESC_PER_REQ], status_virt: core::ptr::null_mut() }
    }
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
    pending: [PendingEntry; MAX_PENDING],   // outstanding read/write IRPs
    pending_len: usize
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
            pending: [PendingEntry::zeroed(); MAX_PENDING],
            pending_len: 0
        }
    }
}

static DEVICE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("Initializing {} driver", driver.get_name());

    dispatch_init!(driver, dispatch_add, dispatch_pnp, dispatch_read, dispatch_write);

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

    let mut ctx = alloc::boxed::Box::new_in(VirtBlkCtx::zeroed(pdo as *const DeviceObject), PoolAllocatorGlobal);
    create_spinlock(&mut ctx.lock);

    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;

    let dev = create_device(driver, Some(name), ctx_ptr, Some(pdo), false, DeviceType::None);
    if dev.is_null() {
        info!("virt_blk: create_device failed for '{}'", name);
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut VirtBlkCtx, PoolAllocatorGlobal));
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
        core::ptr::write_bytes(desc_virt, 0, size_of::<[VirtqDesc; MAX_QUEUE_SIZE]>());
        core::ptr::write_bytes(avail_virt, 0, size_of::<VirtqAvail>());
        core::ptr::write_bytes(used_virt, 0, size_of::<VirtqUsed>());

        ctx.queue.size = queue_size;
        ctx.queue.desc_virt = desc_virt as *mut VirtqDesc;
        ctx.queue.desc_phys = desc_phys;
        ctx.queue.avail_virt = avail_virt as *mut VirtqAvail;
        ctx.queue.avail_phys = avail_phys;
        ctx.queue.used_virt = used_virt as *mut VirtqUsed;
        ctx.queue.used_phys = used_phys;
        ctx.queue.last_used_idx = 0;

        // All descriptors are free initially
        ctx.queue.free_desc = RingBuffer::new(0);
        for i in 0..queue_size {
            ctx.queue.free_desc.push(i);
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
}

fn do_stop(device: &DeviceObject, req: &mut Irp) -> Status {
    let ctx = unsafe { &mut *(device.ctx as *mut VirtBlkCtx) };

    if !ctx.common_cfg.is_null() {
        // Device-side reset -- halts DMA immediately, before we unmap
        // common_cfg below.
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!((*ctx.common_cfg).device_status), 0u8); }
    }

    io_remove_interrupt_handler(ctx.interrupt_handle);

    let mut to_fail = [core::ptr::null_mut::<Irp>(); MAX_PENDING];
    let to_fail_len;
    acquire_spinlock(&mut ctx.lock);
    to_fail_len = ctx.pending_len;
    for i in 0..to_fail_len {
        to_fail[i] = ctx.pending[i].irp;
    }
    ctx.pending_len = 0;
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
            unsafe { drop(alloc::boxed::Box::from_raw_in(device.ctx as *mut VirtBlkCtx, PoolAllocatorGlobal)); }
        }
    }

    req.complete_irp(Status::Success);
    Status::Success
}

extern "C" fn virtblk_isr(ctx_ptr: *mut c_void) -> bool {
    let ctx = unsafe { &mut *(ctx_ptr as *mut VirtBlkCtx) };
    if !ctx.remove_lock.acquire() { return true; }

    // TODO: read the ISR status byte (ctx.isr_cfg) -- reading it acks the
    // interrupt and tells you queue-interrupt vs config-change-interrupt.
    // TODO: walk the used ring (ctx.queue.used_virt, starting at
    // ctx.queue.last_used_idx) and collect completed request ids.

    if io_create_driver_worker(virtblk_dw, ctx_ptr).is_err() {
        // Nothing will ever call virtblk_dw's release() for this reference
        // now -- release it ourselves.
        if ctx.remove_lock.release() {
            unsafe { drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut VirtBlkCtx, PoolAllocatorGlobal)); }
        }
    }

    true
}

extern "C" fn virtblk_dw(ctx_ptr: *mut c_void) {
    let ctx = unsafe { &mut *(ctx_ptr as *mut VirtBlkCtx) };

    acquire_spinlock(&mut ctx.lock);
    // TODO: match completed used-ring entries (by descriptor/request id)
    // against ctx.pending[], read each request's status byte, free the
    // chain's descriptors back to ctx.queue.free_desc, and collect the
    // satisfied IRPs into a local array.
    release_spinlock(&mut ctx.lock);

    // TODO: for each satisfied IRP, outside the lock:
    //   if io_start_processing(irp) { io_complete_irp(irp, Status::Success); }

    // Release the reference taken in virtblk_isr; if do_remove already ran
    // and this was the last outstanding reference, we free ctx.
    if ctx.remove_lock.release() {
        unsafe { drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut VirtBlkCtx, PoolAllocatorGlobal)); }
    }
}

#[kmod::dispatch_handler]
fn dispatch_read(device: &DeviceObject, request: &mut Irp) -> Status {
    let _ = device;
    // TODO: build a descriptor chain (header|data|status) for a
    // VIRTIO_BLK_T_IN request, queue it via ctx.queue, notify the device,
    // stash a PendingEntry, return Status::Pending.
    Status::Unsupported
}

#[kmod::dispatch_handler]
fn dispatch_write(device: &DeviceObject, request: &mut Irp) -> Status {
    let _ = device;
    // TODO: same as dispatch_read but VIRTIO_BLK_T_OUT and a device-readable
    // data descriptor instead of device-writable.
    Status::Unsupported
}
