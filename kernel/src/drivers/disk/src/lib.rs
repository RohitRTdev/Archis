#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use kernel_intf::{
    Lock, acquire_spinlock, create_spinlock, debug, info, io_remove_device, io_send_request, io_start_device, io_stop_device, release_spinlock
};
use kernel_intf::driver::{
    CreatePartitionInfo, DeviceObject, DeviceType, DriverObject, Irp, IrpMajor, IrpMinor,
    IrpResult, PartitionInfo, DiskInfo, ReqInfo, Status, create_device_by_id
};
use kernel_intf::mem::PoolAllocatorGlobal;

mod gpt;
use gpt::{
    FIRST_USABLE_LBA, GPT_ENTRIES_LBA, GPT_ENTRIES_SECTORS, GPT_HEADER_LBA, GPT_MAX_ENTRIES,
    GptHeader, GptPartitionEntry, SECTOR_SIZE, crc32, encode_protective_mbr
};

mod cache;
use cache::{cache_lookup, cache_put, cache_fill_no_evict, flush_all_dirty, cache_invalidate_all};

pub const MAX_CACHE_BLOCKS: usize = 512; // 256 KiB of cached sectors

static DISK_COUNT: AtomicUsize = AtomicUsize::new(0);

pub struct CacheBlock {
    pub data: [u8; SECTOR_SIZE],
    pub dirty: bool,
    pub seq: u64
}

fn last_usable_lba(lba_count: u64) -> u64 {
    lba_count - 1 - GPT_ENTRIES_SECTORS - 1
}

struct PartitionHandle {
    dev_ptr: *mut DeviceObject,
    ctx_ptr: *mut DiskDeviceCtx,
    name_ptr: *mut str
}

unsafe impl Send for PartitionHandle {}
unsafe impl Sync for PartitionHandle {}

pub struct RawDiskCtx {
    pub lock: Lock,
    busy: bool,
    pub child_dev: *const DeviceObject,
    lba_count: u64,
    parts: Vec<PartitionHandle>,
    driver_id: usize,
    disk_index: usize,
    pub cache: BTreeMap<u64, CacheBlock>,
    pub cache_clock: u64
}

// Test-and-set the busy flag under the spinlock 
// Returns false if an admin op is already in flight.
fn try_enter_busy(ctx: &mut RawDiskCtx) -> bool {
    acquire_spinlock(&mut ctx.lock);
    let free = !ctx.busy;
    if free {
        ctx.busy = true;
    }
    release_spinlock(&mut ctx.lock);
    free
}

fn leave_busy(ctx: &mut RawDiskCtx) {
    acquire_spinlock(&mut ctx.lock);
    ctx.busy = false;
    release_spinlock(&mut ctx.lock);
}

unsafe impl Send for RawDiskCtx {}
unsafe impl Sync for RawDiskCtx {}

struct PartCtx {
    raw_ctx: *mut RawDiskCtx,
    entry: GptPartitionEntry
}

unsafe impl Send for PartCtx {}
unsafe impl Sync for PartCtx {}

enum DiskDeviceCtx {
    Raw(RawDiskCtx),
    Part(PartCtx)
}

// Synchronous (blocking) forwarding to child_dev -- used only by the GPT
// metadata paths below, which runs rarely 
// and can afford to block.
fn sync_io(ctx: &RawDiskCtx, is_write: bool, lba: u64, buf_addr: usize, buf_len: usize) -> bool {
    let major = if is_write { IrpMajor::Write } else { IrpMajor::Read };
    let result = io_send_request(
        ctx.child_dev, major as usize, IrpMinor::None as usize,
        buf_addr, buf_len, lba as usize,
        core::ptr::null(), None, core::ptr::null_mut()
    );
    result.status == Status::Success
}

fn read_bytes(ctx: &RawDiskCtx, byte_off: usize, out: &mut [u8]) -> bool {
    debug_assert!(byte_off % SECTOR_SIZE == 0);
    sync_io(ctx, false, (byte_off / SECTOR_SIZE) as u64, out.as_mut_ptr() as usize, out.len())
}

pub fn write_bytes(ctx: &RawDiskCtx, byte_off: usize, data: &[u8]) -> bool {
    debug_assert!(byte_off % SECTOR_SIZE == 0);
    sync_io(ctx, true, (byte_off / SECTOR_SIZE) as u64, data.as_ptr() as usize, data.len())
}

// Write a fresh protective MBR + empty primary GPT (header + entry array).
fn write_fresh_gpt(ctx: &mut RawDiskCtx) -> bool {
    let total_lba = ctx.lba_count;

    let mut mbr = [0u8; SECTOR_SIZE];
    encode_protective_mbr(&mut mbr, total_lba);
    if !write_bytes(ctx, 0, &mbr) {
        return false;
    }

    let entries_buf = vec![0u8; GPT_ENTRIES_SECTORS as usize * SECTOR_SIZE];
    let entries_crc = crc32(&entries_buf);

    let header = GptHeader {
        my_lba: GPT_HEADER_LBA,
        alternate_lba: total_lba - 1,
        first_usable_lba: FIRST_USABLE_LBA,
        last_usable_lba: last_usable_lba(total_lba),
        disk_guid: [0u8; 16],
        partition_entry_lba: GPT_ENTRIES_LBA,
        num_partition_entries: GPT_MAX_ENTRIES as u32,
        size_of_partition_entry: GptPartitionEntry::SIZE as u32,
        partition_entry_array_crc32: entries_crc
    };
    let mut header_buf = [0u8; SECTOR_SIZE];
    header.encode(&mut header_buf);
    if !write_bytes(ctx, GPT_HEADER_LBA as usize * SECTOR_SIZE, &header_buf) {
        return false;
    }
    if !write_bytes(ctx, GPT_ENTRIES_LBA as usize * SECTOR_SIZE, &entries_buf) {
        return false;
    }
    true
}

// Append a new GPT entry for `info`, persist it, and spawn+start the
// corresponding partition class device.
fn add_partition(ctx: &mut RawDiskCtx, info: &CreatePartitionInfo) -> Option<GptPartitionEntry> {
    let mut header_buf = [0u8; SECTOR_SIZE];
    if !read_bytes(ctx, GPT_HEADER_LBA as usize * SECTOR_SIZE, &mut header_buf) {
        return None;
    }
    let header = GptHeader::decode(&header_buf)?;

    if info.num_lba == 0 {
        return None;
    }
    let end_lba = info.start_lba.checked_add(info.num_lba)?.checked_sub(1)?;
    if info.start_lba < header.first_usable_lba || end_lba > header.last_usable_lba {
        return None;
    }

    let mut entries_buf = vec![0u8; GPT_ENTRIES_SECTORS as usize * SECTOR_SIZE];
    if !read_bytes(ctx, header.partition_entry_lba as usize * SECTOR_SIZE, &mut entries_buf) {
        return None;
    }

    let mut free_index = None;
    for i in 0..GPT_MAX_ENTRIES {
        let raw = &entries_buf[i * GptPartitionEntry::SIZE..(i + 1) * GptPartitionEntry::SIZE];
        if GptPartitionEntry::decode(raw).is_unused() {
            free_index = Some(i);
            break;
        }
    }
    let idx = free_index?;

    let entry = GptPartitionEntry {
        part_type_guid: info.part_type_guid,
        unique_guid: info.unique_guid,
        start_lba: info.start_lba,
        end_lba,
        attributes: 0,
        name_utf16: info.name_utf16
    };
    entry.encode(&mut entries_buf[idx * GptPartitionEntry::SIZE..(idx + 1) * GptPartitionEntry::SIZE]);

    let new_crc = crc32(&entries_buf);
    if !write_bytes(ctx, header.partition_entry_lba as usize * SECTOR_SIZE, &entries_buf) {
        return None;
    }

    let mut updated_header = header;
    updated_header.partition_entry_array_crc32 = new_crc;
    let mut header_out = [0u8; SECTOR_SIZE];
    updated_header.encode(&mut header_out);
    if !write_bytes(ctx, GPT_HEADER_LBA as usize * SECTOR_SIZE, &header_out) {
        return None;
    }

    let raw_ctx_ptr = ctx as *mut RawDiskCtx;
    let driver_id = ctx.driver_id;
    let disk_index = ctx.disk_index;
    create_partition_device(driver_id, raw_ctx_ptr, disk_index, idx, entry, &mut ctx.parts);

    Some(entry)
}

// Scan an existing GPT (if any) and spawn+start a partition device for every
// in-use entry found.
fn scan_and_create_partitions(ctx: &mut RawDiskCtx) {
    flush_all_dirty(ctx);
    cache_invalidate_all(ctx);

    let mut header_buf = [0u8; SECTOR_SIZE];
    if !read_bytes(ctx, GPT_HEADER_LBA as usize * SECTOR_SIZE, &mut header_buf) {
        info!("disk: failed to read GPT header from child device");
        return;
    }
    let header = match GptHeader::decode(&header_buf) {
        Some(h) => h,
        None => return
    };

    let mut entries_buf = vec![0u8; GPT_ENTRIES_SECTORS as usize * SECTOR_SIZE];
    if !read_bytes(ctx, header.partition_entry_lba as usize * SECTOR_SIZE, &mut entries_buf) {
        info!("disk: failed to read GPT partition entries from child device");
        return;
    }

    let driver_id = ctx.driver_id;
    let disk_index = ctx.disk_index;
    let raw_ctx_ptr = ctx as *mut RawDiskCtx;

    let count = (header.num_partition_entries as usize).min(GPT_MAX_ENTRIES);
    for i in 0..count {
        let raw = &entries_buf[i * GptPartitionEntry::SIZE..(i + 1) * GptPartitionEntry::SIZE];
        let entry = GptPartitionEntry::decode(raw);
        if entry.is_unused() {
            continue;
        }
        create_partition_device(driver_id, raw_ctx_ptr, disk_index, i, entry, &mut ctx.parts);
    }
}

fn create_partition_device(
    driver_id: usize,
    raw_ctx_ptr: *mut RawDiskCtx,
    disk_index: usize,
    part_index: usize,
    entry: GptPartitionEntry,
    parts: &mut Vec<PartitionHandle>
) {
    let name: &'static str = Box::leak(format!("disk{}p{}", disk_index, part_index).into_boxed_str());

    let part_ctx = Box::new_in(DiskDeviceCtx::Part(PartCtx { raw_ctx: raw_ctx_ptr, entry }), PoolAllocatorGlobal);
    let ctx_ptr =  Box::into_raw_with_allocator(part_ctx).0;

    let dev = create_device_by_id(driver_id, Some(name), ctx_ptr as *mut c_void, None, true, DeviceType::Partition);
    if dev.is_null() {
        info!("disk: failed to create partition device '{}'", name);
        unsafe {
            drop(Box::from_raw_in(ctx_ptr, PoolAllocatorGlobal));
            drop(Box::from_raw(name as *const str as *mut str));
        }
        return;
    }

    parts.push(PartitionHandle { dev_ptr: dev, ctx_ptr, name_ptr: name as *const str as *mut str });

    io_start_device(dev);
    info!("disk: created + started partition device '{}'", name);
}

fn stop_partitions(ctx: &mut RawDiskCtx) {
    for p in &ctx.parts {
        let name = unsafe { (*p.dev_ptr).get_name().expect("Partition device expected to have name!") };
        info!("Stopping partition device: {}", name);
        io_stop_device(p.dev_ptr);
    }
}

fn teardown_partitions(ctx: &mut RawDiskCtx) {
    for p in ctx.parts.drain(..) {
        io_stop_device(p.dev_ptr);
        io_remove_device(p.dev_ptr);
        unsafe {
            drop(Box::from_raw_in(p.ctx_ptr, PoolAllocatorGlobal));
            drop(Box::from_raw(p.name_ptr));
        }
    }
}

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing (id={})...", driver.get_name(), driver.id);
    kmod::dispatch_init!(
        driver, dispatch_add, dispatch_pnp, dispatch_control, dispatch_read, dispatch_write,
        dispatch_open, dispatch_close
    );
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: &DeviceObject) -> Status {
    let disk_index = DISK_COUNT.fetch_add(1, Ordering::Relaxed);
    let name: &'static str = Box::leak(format!("disk{}", disk_index).into_boxed_str());

    let mut lock = Lock::new();
    create_spinlock(&mut lock);

    let raw_ctx = DiskDeviceCtx::Raw(RawDiskCtx {
        lock,
        busy: false,
        child_dev: pdo as *const _,
        lba_count: 0,   // later queried in do_start
        parts: Vec::new(),
        driver_id: driver.id,
        disk_index,
        cache: BTreeMap::new(),
        cache_clock: 0
    });
    let ctx_box = Box::new_in(raw_ctx, PoolAllocatorGlobal);
    let ctx_ptr = Box::into_raw_with_allocator(ctx_box).0;

    let dev = create_device_by_id(driver.id, Some(name), ctx_ptr as *mut c_void, Some(pdo), false, DeviceType::Disk);
    if dev.is_null() {
        info!("disk: create_device failed for '{}'", name);
        unsafe {
            drop(Box::from_raw_in(ctx_ptr, PoolAllocatorGlobal));
            drop(Box::from_raw(name as *const str as *mut str));
        }
        return Status::Failed;
    }

    info!("disk: created raw device '{}'", name);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, request: &mut Irp) -> Status {
    match request.minor_code {
        IrpMinor::Start => do_start(device, request),
        IrpMinor::Stop => do_stop(device, request),
        IrpMinor::Remove => do_remove(device, request),
        _ => Status::Unsupported
    }
}

fn do_start(device: &DeviceObject, request: &mut Irp) -> Status {
    let wrapper = unsafe { &mut *(device.ctx as *mut DiskDeviceCtx) };
    if let DiskDeviceCtx::Raw(ctx) = wrapper {
        // Query the child for its capacity 
        let req_info = ReqInfo { _unused: [0; 2] };
        let result = io_send_request(
            ctx.child_dev, IrpMajor::Control as usize, IrpMinor::DiskGetInfo as usize,
            0, 0, 0, &req_info as *const ReqInfo, None, core::ptr::null_mut()
        );
        if result.status != Status::Success {
            info!("disk: DiskGetInfo failed against child device for '{}'", device.get_name().unwrap_or("?"));
            request.complete_irp(Status::Failed);
            return Status::Failed;
        }
        let disk_info = unsafe { result.req_info.disk_info };
        // Right now we only support 512 byte sector size
        assert!(disk_info.lba_size == SECTOR_SIZE, "disk: child device sector size mismatch");
        info!("Disk info -> num_sectors: {}, sector_size: {}", disk_info.lba_count, disk_info.lba_size);
        if !try_enter_busy(ctx) {
            request.complete_irp(Status::Failed);
            return Status::Failed;
        }

        acquire_spinlock(&mut ctx.lock);
        ctx.lba_count = disk_info.lba_count;
        release_spinlock(&mut ctx.lock);

        scan_and_create_partitions(ctx);

        leave_busy(ctx);
    }
    info!("disk: start '{}'", device.get_name().unwrap_or("?"));
    request.complete_irp(Status::Success);
    Status::Success
}

fn do_stop(device: &DeviceObject, request: &mut Irp) -> Status {
    let wrapper = unsafe { &mut *(device.ctx as *mut DiskDeviceCtx) };
    if let DiskDeviceCtx::Raw(ctx) = wrapper {
        assert!(try_enter_busy(ctx));

        stop_partitions(ctx);

        if !flush_all_dirty(ctx) {
            info!("disk: flush_all_dirty failed during stop for '{}' -- some writes may be lost",
                  device.get_name().unwrap_or("?"));
        }

        leave_busy(ctx);
    }
    info!("disk: stop '{}'", device.get_name().unwrap_or("?"));
    request.complete_irp(Status::Success);
    Status::Success
}

fn do_remove(device: &DeviceObject, request: &mut Irp) -> Status {
    let wrapper = unsafe { &mut *(device.ctx as *mut DiskDeviceCtx) };
    if let DiskDeviceCtx::Raw(ctx) = wrapper {
        assert!(try_enter_busy(ctx));
        flush_all_dirty(ctx);
        teardown_partitions(ctx);
        leave_busy(ctx);
    }
    if let Some(name) = device.get_name() {
        unsafe { drop(Box::from_raw(name as *const str as *mut str)); }
    }
    if !device.ctx.is_null() {
        unsafe { drop(Box::from_raw_in(device.ctx as *mut DiskDeviceCtx, PoolAllocatorGlobal)); }
    }
    request.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_open(_device: &DeviceObject, req: &mut Irp) -> Status {
    req.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_close(_device: &DeviceObject, req: &mut Irp) -> Status {
    req.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_control(device: &DeviceObject, request: &mut Irp) -> Status {
    match request.minor_code {
        IrpMinor::DiskGetInfo => do_get_info(device, request),
        IrpMinor::DiskCreateGpt => do_create_gpt(device, request),
        IrpMinor::DiskAddPartition => do_add_partition(device, request),
        IrpMinor::DiskGetPartitionInfo => do_get_partition_info(device, request),
        _ => Status::Unsupported
    }
}

fn do_get_info(device: &DeviceObject, request: &mut Irp) -> Status {
    let wrapper = unsafe { &mut *(device.ctx as *mut DiskDeviceCtx) };
    let info = match wrapper {
        DiskDeviceCtx::Raw(ctx) => {
            acquire_spinlock(&mut ctx.lock);
            let lba_count = ctx.lba_count;
            release_spinlock(&mut ctx.lock);
            DiskInfo { lba_size: SECTOR_SIZE, lba_count }
        }
        DiskDeviceCtx::Part(p) => DiskInfo { lba_size: SECTOR_SIZE, lba_count: p.entry.num_lba() }
    };
    request.req_info.disk_info = info;
    request.complete_irp(Status::Success);
    Status::Success
}

fn do_create_gpt(device: &DeviceObject, request: &mut Irp) -> Status {
    let wrapper = unsafe { &mut *(device.ctx as *mut DiskDeviceCtx) };
    let ctx = match wrapper {
        DiskDeviceCtx::Raw(ctx) => ctx,
        DiskDeviceCtx::Part(_) => return Status::Unsupported
    };

    if !try_enter_busy(ctx) {
        info!("Could not satisfy create_gpt request since another admin request is ongoing in disk");
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }
    flush_all_dirty(ctx);
    cache_invalidate_all(ctx);
    teardown_partitions(ctx);
    let ok = write_fresh_gpt(ctx);
    leave_busy(ctx);
    if !ok {
        info!("disk: failed to write fresh GPT to child device for '{}'", device.get_name().unwrap_or("?"));
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }
    info!("disk: created fresh GPT on '{}'", device.get_name().unwrap_or("?"));
    request.complete_irp(Status::Success);
    Status::Success
}

fn do_add_partition(device: &DeviceObject, request: &mut Irp) -> Status {
    let wrapper = unsafe { &mut *(device.ctx as *mut DiskDeviceCtx) };
    let ctx = match wrapper {
        DiskDeviceCtx::Raw(ctx) => ctx,
        DiskDeviceCtx::Part(_) => return Status::Unsupported
    };
    let info = unsafe { request.req_info.create_partition };
    if !try_enter_busy(ctx) {
        info!("Could not satisfy add_partition request since another admin request is ongoing in disk");
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }
    flush_all_dirty(ctx);
    cache_invalidate_all(ctx);
    let result = add_partition(ctx, &info);
    leave_busy(ctx);
    match result {
        Some(_) => {
            request.complete_irp(Status::Success);
            Status::Success
        }
        None => {
            info!("disk: add_partition rejected (bad range, no free entry, or child I/O failure)");
            request.complete_irp(Status::Failed);
            Status::Failed
        }
    }
}

fn do_get_partition_info(device: &DeviceObject, request: &mut Irp) -> Status {
    let wrapper = unsafe { &*(device.ctx as *const DiskDeviceCtx) };
    let p = match wrapper {
        DiskDeviceCtx::Part(p) => p,
        DiskDeviceCtx::Raw(_) => return Status::Unsupported
    };
    request.req_info.partition_info = PartitionInfo {
        part_type_guid: p.entry.part_type_guid,
        unique_guid: p.entry.unique_guid,
        start_lba: p.entry.start_lba,
        num_lba: p.entry.num_lba()
    };
    request.complete_irp(Status::Success);
    Status::Success
}

struct DiskPendingCompletion {
    outer_irp: *mut Irp,
    raw_ctx_ptr: *mut RawDiskCtx,
    base_lba: u64,
    num_lba: u64
}

extern "C" fn on_child_io_complete(result: *const IrpResult, ctx: *mut c_void) {
    let pending = unsafe { Box::from_raw(ctx as *mut DiskPendingCompletion) };
    let outer = unsafe { &mut *pending.outer_irp };
    let raw_ctx = unsafe { &mut *pending.raw_ctx_ptr };
    let r = unsafe { &*result };
    outer.bytes_completed = r.bytes_completed;

    if r.status == Status::Success {
        cache_fill_no_evict(raw_ctx, pending.base_lba, outer.buffer.base_address, pending.num_lba);
    }

    outer.complete_irp(r.status);
}

// Reads/writes are all-or-nothing: buffer.size must be a multiple of
// SECTOR_SIZE, `offset` is an LBA number (not a byte offset).
fn do_io(device: &DeviceObject, request: &mut Irp, is_write: bool) -> Status {
    let size = request.buffer.size;
    if size == 0 || size % SECTOR_SIZE != 0 {
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }
    let num_lba = (size / SECTOR_SIZE) as u64;

    let wrapper = unsafe { &mut *(device.ctx as *mut DiskDeviceCtx) };
    let (raw_ctx_ptr, base_lba) = match wrapper {
        DiskDeviceCtx::Raw(ctx) => (ctx as *mut RawDiskCtx, request.offset as u64),
        DiskDeviceCtx::Part(p) => {
            if request.offset as u64 + num_lba > p.entry.num_lba() {
                request.complete_irp(Status::Failed);
                return Status::Failed;
            }
            (p.raw_ctx, p.entry.start_lba + request.offset as u64)
        }
    };

    let raw_ctx = unsafe { &mut *raw_ctx_ptr };
    acquire_spinlock(&mut raw_ctx.lock);
    if raw_ctx.busy || base_lba + num_lba > raw_ctx.lba_count {
        release_spinlock(&mut raw_ctx.lock);
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }
    let child_dev = raw_ctx.child_dev;
    release_spinlock(&mut raw_ctx.lock);

    if !is_write {
        let mut hit = true;
        for i in 0..num_lba {
            if cache_lookup(raw_ctx, base_lba + i).is_none() {
                hit = false;
                break;
            }
        }
        if hit {
            for i in 0..num_lba {
                let data = cache_lookup(raw_ctx, base_lba + i).unwrap();
                let dst = (request.buffer.base_address + i as usize * SECTOR_SIZE) as *mut u8;
                unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dst, SECTOR_SIZE); }
            }
            request.bytes_completed = size;
            request.complete_irp(Status::Success);
            return Status::Success;
        }
    } else {
        for i in 0..num_lba {
            let mut block = [0u8; SECTOR_SIZE];
            let src = (request.buffer.base_address + i as usize * SECTOR_SIZE) as *const u8;
            unsafe { core::ptr::copy_nonoverlapping(src, block.as_mut_ptr(), SECTOR_SIZE); }
            cache_put(raw_ctx, base_lba + i, &block, true);
        }
        request.bytes_completed = size;
        request.complete_irp(Status::Success);
        return Status::Success;
    }

    let pending_ptr = Box::into_raw(Box::new(DiskPendingCompletion {
        outer_irp: request as *mut Irp,
        raw_ctx_ptr,
        base_lba,
        num_lba
    })) as *mut c_void;
    let dispatch = io_send_request(
        child_dev, IrpMajor::Read as usize, IrpMinor::None as usize,
        request.buffer.base_address, request.buffer.size, base_lba as usize,
        core::ptr::null(), Some(on_child_io_complete), pending_ptr
    );
    if dispatch.status == Status::Failed {
        // Since dispatch failed, reclaim the completion context
        unsafe { drop(Box::from_raw(pending_ptr as *mut DiskPendingCompletion)); }
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }
    Status::Pending
}

#[kmod::dispatch_handler]
fn dispatch_read(device: &DeviceObject, request: &mut Irp) -> Status {
    do_io(device, request, false)
}

#[kmod::dispatch_handler]
fn dispatch_write(device: &DeviceObject, request: &mut Irp) -> Status {
    do_io(device, request, true)
}
