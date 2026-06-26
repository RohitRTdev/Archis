// ACPICA OS Services Layer (OSL).
//
// Glue between the ACPICA C interpreter and the kernel. Every AcpiOs* symbol
// ACPICA needs is implemented in this file.

use alloc::alloc::{alloc, dealloc, Layout};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void, CStr};
use core::mem::{align_of, size_of};
use core::ptr::{self, read_volatile, write_volatile, NonNull};
use core::sync::atomic::Ordering;

use common::{align_down, align_up, PAGE_SIZE};
use kernel_intf::{info, debug};
use kernel_intf::list::{DynList, List, ListNodeGuard};
use kernel_intf::mem::{Allocator, PoolAllocator, PoolAllocatorGlobal};
use kernel_intf::{io_install_interrupt_handler, io_remove_interrupt_handler};
use kernel_intf::InterruptHandle;

use acpi_intf::*;

use crate::acpica_log;
use crate::devices::HPET;
use crate::hal::{self, Spinlock as HalSpinlock, allocate_vector};
use crate::mem::{
    allocate_memory, deallocate_memory, get_physical_address, map_memory, unmap_memory,
    PageDescriptor,
};
use crate::sched;
use crate::sync::{KEvent, KSem, Once, Spinlock};
use crate::BOOT_INFO;

use super::table::fetch_acpi_table_raw;

#[derive(Clone, Copy)]
struct WorkItem {
    function: ACPI_OSD_EXEC_CALLBACK,
    context: *mut c_void,
}

unsafe impl Send for WorkItem {}

struct WorkQueue {
    queue: Spinlock<DynList<WorkItem>>,
    signal: KEvent,
}

static WORK_QUEUE: Once<WorkQueue> = Once::new();

struct SciHandler {
    handler: AcpiHandlerSCI,
    context: *mut c_void,
    kernel_handle: InterruptHandle,
}

unsafe impl Send for SciHandler {}

static SCI_HANDLERS: Spinlock<BTreeMap<u32, SciHandler>> = Spinlock::new(BTreeMap::new());

struct MmioMapping {
    virt_base: usize,
    mapped_size: usize,
    flags: u8,
}

static MMIO_MAPS: Spinlock<BTreeMap<usize, MmioMapping>> = Spinlock::new(BTreeMap::new());

// SCI handler signature (ACPICA): returns u32 (ACPI_INTERRUPT_HANDLED /
// ACPI_INTERRUPT_NOT_HANDLED). Distinct from the OSD_HANDLER alias in
// types.rs which is used by GPE handlers and returns nothing — keeping them
// separate matches ACPICA's expectation that interrupt handlers report
// whether they claimed the IRQ.
pub type AcpiHandlerSCI = extern "C" fn(*mut c_void) -> u32;

// Bring up OSL-side resources.
pub fn init() {
    WORK_QUEUE.call_once(|| WorkQueue {
        queue: Spinlock::new(List::new()),
        signal: KEvent::new(true), // auto-reset
    });

    // One worker thread is enough; ACPICA work is sequential.
    sched::create_system_thread(work_queue_worker, ptr::null_mut())
        .expect("ACPICA OSL: failed to spawn work queue worker");

    info!("ACPICA OSL initialised");
}

fn dequeue_work(q: &mut DynList<WorkItem>) -> Option<ListNodeGuard<WorkItem, PoolAllocator>> {
    let head = q.first().map(NonNull::from)?;
    let guard = unsafe { q.remove_node(head) };
    Some(guard)
}

extern "C" fn work_queue_worker() -> ! {
    let wq = WORK_QUEUE.get().expect("ACPICA work queue not initialised");
    loop {
        wq.signal.wait(false);
        // Drain everything currently visible. With auto-reset we only get
        // one wake per signal, but ACPICA may submit several items before
        // we get scheduled — drain to empty before sleeping again.
        loop {
            let item = dequeue_work(&mut wq.queue.lock());
            match item {
                Some(item) => {
                    acpica_log!("worker: running {:p}({:p})", item.function as *const (), item.context);
                    (item.function)(item.context);
                }
                None => break,
            }
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsInitialize() -> ACPI_STATUS {
    // The kernel-side init() above handles the heavy lifting once; ACPICA may
    // call AcpiOsInitialize multiple times in some sub-paths so this stays a
    // cheap no-op.
    acpica_log!("AcpiOsInitialize");
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsTerminate() -> ACPI_STATUS {
    acpica_log!("AcpiOsTerminate");
    AcpiOsWaitEventsComplete();
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsGetRootPointer() -> ACPI_PHYSICAL_ADDRESS {
    // Bootloader-supplied RSDP. ACPICA's own scan fallback handles the rest
    // if this returns 0, but our bootloader always stashes it.
    match BOOT_INFO.get() {
        Some(bi) => bi.rsdp as ACPI_PHYSICAL_ADDRESS,
        None => 0,
    }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsPredefinedOverride(
    predefined_obj: *const ACPI_PREDEFINED_NAMES,
    new_value: *mut ACPI_STRING
) -> ACPI_STATUS {
    if predefined_obj.is_null() || new_value.is_null() {
        return AE_BAD_PARAMETER;
    }
    // ACPICA hands us out-pointers that aren't guaranteed to be properly
    // aligned for their pointee type — its internal table structs are often
    // packed. Use write_unaligned everywhere we store through one.
    unsafe { ptr::write_unaligned(new_value, ptr::null()); }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsTableOverride(
    existing_table: *mut AcpiTableHeader,
    new_table: *mut *const AcpiTableHeader
) -> ACPI_STATUS {
    if existing_table.is_null() || new_table.is_null() {
        return AE_BAD_PARAMETER;
    }
    unsafe { ptr::write_unaligned(new_table, ptr::null()); }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsPhysicalTableOverride(
    existing_table: *const AcpiTableHeader,
    new_address: *mut ACPI_PHYSICAL_ADDRESS,
    new_table_length: *mut u32
) -> ACPI_STATUS {
    if existing_table.is_null() || new_address.is_null() || new_table_length.is_null() {
        return AE_BAD_PARAMETER;
    }
    unsafe {
        ptr::write_unaligned(new_address, 0);
        ptr::write_unaligned(new_table_length, 0);
    }
    AE_OK
}

#[repr(C)]
struct AcpiCache {
    object_size: usize,
    max_depth: u16,
    freelist: Spinlock<Vec<NonNull<u8>>>,
}

unsafe impl Send for AcpiCache {}
unsafe impl Sync for AcpiCache {}

fn cache_layout(object_size: usize) -> Layout {
    // Most ACPICA objects are small structs that want pointer alignment.
    Layout::from_size_align(object_size.max(1), align_of::<usize>())
        .expect("ACPICA cache: invalid object size")
}

fn drain_cache(cache: &AcpiCache) {
    let mut fl = cache.freelist.lock();
    let layout = cache_layout(cache.object_size);
    while let Some(ptr) = fl.pop() {
        unsafe { <PoolAllocator as Allocator<u8>>::dealloc(ptr, layout); }
    }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsCreateCache(
    cache_name: *const c_char,
    object_size: u16,
    max_depth: u16,
    return_cache: *mut *mut c_void
) -> ACPI_STATUS {
    if return_cache.is_null() || object_size == 0 {
        return AE_BAD_PARAMETER;
    }

    let name = if cache_name.is_null() {
        "<anon>"
    } else {
        // ACPICA always passes a printable ASCII name. Best-effort decode.
        unsafe { CStr::from_ptr(cache_name).to_str().unwrap_or("<bad-utf8>") }
    };

    // Descriptor lives in the pool — it's small, fixed-size, and we churn
    // through Create/Delete pairs at init/teardown.
    let cache = Box::new_in(AcpiCache {
        object_size: object_size as usize,
        max_depth,
        freelist: Spinlock::new(Vec::new()),
    }, PoolAllocatorGlobal);

    let raw = Box::into_raw_with_allocator(cache).0;
    unsafe { *return_cache = raw as *mut c_void; }
    acpica_log!("CreateCache name={} size={} max_depth={} -> {:p}", name, object_size, max_depth, raw);
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsDeleteCache(cache: *mut c_void) -> ACPI_STATUS {
    if cache.is_null() {
        return AE_BAD_PARAMETER;
    }
    // ACPICA hands us back what we returned from CreateCache.
    let cache = unsafe { Box::from_raw_in(cache as *mut AcpiCache, PoolAllocatorGlobal) };
    drain_cache(&cache);
    acpica_log!("DeleteCache {:p}", &*cache);
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsPurgeCache(cache: *mut c_void) -> ACPI_STATUS {
    if cache.is_null() {
        return AE_BAD_PARAMETER;
    }
    // descriptor remains valid; we only drain the freelist.
    let cache = unsafe { &*(cache as *const AcpiCache) };
    drain_cache(cache);
    acpica_log!("PurgeCache {:p}", cache);
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsAcquireObject(cache: *mut c_void) -> *mut c_void {
    if cache.is_null() {
        return ptr::null_mut();
    }
    let cache = unsafe { &*(cache as *const AcpiCache) };
    let layout = cache_layout(cache.object_size);

    // Fast path: pop the freelist (cache hit).
    let popped = cache.freelist.lock().pop();
    let ptr: *mut u8 = match popped {
        Some(p) => p.as_ptr(),
        None => match <PoolAllocator as Allocator<u8>>::alloc(layout) {
            Ok(nn) => nn.as_ptr(),
            Err(_) => return ptr::null_mut(),
        }
    };

    // ACPICA expects freshly-acquired objects to be zeroed.
    unsafe { ptr::write_bytes(ptr, 0, cache.object_size); }
    ptr as *mut c_void
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsReleaseObject(cache: *mut c_void, object: *mut c_void) -> ACPI_STATUS {
    if cache.is_null() || object.is_null() {
        return AE_BAD_PARAMETER;
    }
    let cache = unsafe { &*(cache as *const AcpiCache) };
    let layout = cache_layout(cache.object_size);
    let nn = unsafe { NonNull::new_unchecked(object as *mut u8) };

    let mut fl = cache.freelist.lock();
    if fl.len() < cache.max_depth as usize {
        fl.push(nn);
    } else {
        drop(fl);
        unsafe { <PoolAllocator as Allocator<u8>>::dealloc(nn, layout); }
    }
    AE_OK
}

// We tag every heap allocation with its layout so AcpiOsFree (which only
// receives a pointer) can hand the right Layout back to dealloc. The tag is
// stored as a usize prefix immediately before the pointer we return to
// ACPICA. Cost: 8 bytes per allocation + one extra usize-alignment.

const HEAP_ALIGN: usize = {
    // Manual max() — Ord::max isn't const-stable yet.
    let a = align_of::<usize>();
    if a > 16 { a } else { 16 }
};

fn heap_layout(size: usize) -> Layout {
    Layout::from_size_align(size + size_of::<usize>(), HEAP_ALIGN)
        .expect("ACPICA heap: bad layout")
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsAllocate(size: ACPI_SIZE) -> *mut c_void {
    if size == 0 {
        return ptr::null_mut();
    }
    let layout = heap_layout(size);
    let raw = unsafe { alloc(layout) };
    if raw.is_null() {
        info!("ACPICA: AcpiOsAllocate({}) failed", size);
        return ptr::null_mut();
    }
    unsafe {
        // Store size prefix so Free can reconstruct the Layout.
        (raw as *mut usize).write(size);
        raw.add(size_of::<usize>()) as *mut c_void
    }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsFree(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let raw = (ptr as *mut u8).sub(size_of::<usize>());
        let size = (raw as *const usize).read();
        dealloc(raw, heap_layout(size));
    }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsMapMemory(
    phys_addr: ACPI_PHYSICAL_ADDRESS,
    length: ACPI_SIZE
) -> *mut c_void {
    if length == 0 {
        return ptr::null_mut();
    }

    // Align to page boundary; remember the offset so we can return
    // virt_base + offset to the caller.
    let phys_aligned = align_down(phys_addr as usize, PAGE_SIZE);
    let offset = (phys_addr as usize) - phys_aligned;
    let mapped_size = align_up(offset + length, PAGE_SIZE);

    let flags = PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC | PageDescriptor::MMIO;
    let layout = match Layout::from_size_align(mapped_size, PAGE_SIZE) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut()
    };

    let virt = match allocate_memory(layout, flags) {
        Ok(v) => v as usize,
        Err(_) => {
            info!("ACPICA: MapMemory virt reserve failed (size={})", mapped_size);
            return ptr::null_mut();
        }
    };

    if let Err(_) = map_memory(phys_aligned, virt, mapped_size, PageDescriptor::MMIO) {
        let _ = deallocate_memory(virt as *mut u8, layout, flags);
        info!("ACPICA: MapMemory map_memory failed phys={:#X}", phys_aligned);
        return ptr::null_mut();
    }

    MMIO_MAPS.lock().insert(virt, MmioMapping {
        virt_base: virt,
        mapped_size,
        flags,
    });

    acpica_log!("MapMemory phys={:#X} len={} -> virt={:#X} (+offset {})",
        phys_addr, length, virt, offset);
    (virt + offset) as *mut c_void
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsUnmapMemory(virt_addr: *mut c_void, _length: ACPI_SIZE) {
    if virt_addr.is_null() {
        return;
    }

    // Recover the virtual page base. We stored under the page-aligned virt
    // base, but ACPICA passes us the offset-into-page pointer we returned.
    let virt_base = align_down(virt_addr as usize, PAGE_SIZE);

    let entry = MMIO_MAPS.lock().remove(&virt_base);
    if let Some(m) = entry {
        if let Err(e) = unmap_memory(m.virt_base, m.mapped_size, PageDescriptor::MMIO) {
            info!("ACPICA: UnmapMemory unmap_memory failed: {:?}", e);
        }
        let layout = Layout::from_size_align(m.mapped_size, PAGE_SIZE).unwrap();
        if let Err(e) = deallocate_memory(m.virt_base as *mut u8, layout, m.flags) {
            info!("ACPICA: UnmapMemory deallocate failed: {:?}", e);
        }
        acpica_log!("UnmapMemory virt={:#X} size={}", m.virt_base, m.mapped_size);
    } else {
        acpica_log!("UnmapMemory: no mapping for {:#X}", virt_base);
    }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsGetPhysicalAddress(
    virt_addr: *const c_void,
    phys_addr: *mut ACPI_PHYSICAL_ADDRESS
) -> ACPI_STATUS {
    if virt_addr.is_null() || phys_addr.is_null() {
        return AE_BAD_PARAMETER;
    }
    match get_physical_address(virt_addr as usize, 0) {
        Some(p) => {
            unsafe { ptr::write_unaligned(phys_addr, p as ACPI_PHYSICAL_ADDRESS); }
            AE_OK
        }
        None => AE_ERROR,
    }
}

// ACPICA's Readable / Writable predicates. We don't have a fault-tolerant
// probe mechanism, so we accept any non-null, non-zero range as valid. The
// kernel will fault if ACPICA actually touches bad memory; this matches what
// most OSL implementations do.
#[unsafe(no_mangle)]
extern "C" fn AcpiOsReadable(memory: *const c_void, length: ACPI_SIZE) -> u8 {
    if memory.is_null() || length == 0 { 0 } else { 1 }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsWritable(memory: *const c_void, length: ACPI_SIZE) -> u8 {
    if memory.is_null() || length == 0 { 0 } else { 1 }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsGetThreadId() -> ACPI_THREAD_ID {
    // ACPICA requires this to be non-zero. Task IDs start at 0, so bias by 1.
    (sched::get_current_task_id().expect("AcpiOsGetThreadId() called during idle task!") as ACPI_THREAD_ID) + 1
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsExecute(
    _execute_type: u32,
    function: ACPI_OSD_EXEC_CALLBACK,
    context: *mut c_void
) -> ACPI_STATUS {
    let wq = match WORK_QUEUE.get() {
        Some(q) => q,
        None => {
            info!("ACPICA: AcpiOsExecute called before init");
            return AE_ERROR;
        }
    };

    if let Err(_) = wq.queue.lock().add_node(WorkItem { function, context }) {
        info!("ACPICA: AcpiOsExecute enqueue failed (pool OOM)");
        return AE_ERROR;
    }
    wq.signal.signal();
    AE_OK
}

extern "C" fn fence_routine(ctx: *mut c_void) {
    let event = unsafe { Box::from_raw_in(ctx as *mut KEvent, PoolAllocatorGlobal) };
    event.signal();
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsWaitEventsComplete() {
    acpica_log!("Waiting for ACPI events to complete");
    let wq = match WORK_QUEUE.get() {
        Some(q) => q,
        None => return,
    };

    let event = KEvent::new(true);
    let event_box = Box::new_in(event.clone(), PoolAllocatorGlobal);
    let event_ptr = Box::into_raw_with_allocator(event_box).0 as *mut c_void;

    if wq.queue.lock()
        .add_node(WorkItem { function: fence_routine, context: event_ptr })
        .is_err()
    {
        info!("ACPICA: WaitEventsComplete fence enqueue failed");
        // Reclaim the leaked event in case of error
        unsafe { drop(Box::from_raw_in(event_ptr as *mut KEvent, PoolAllocatorGlobal)); }
        return;
    }
    wq.signal.signal();
    event.wait(false);
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsSleep(milliseconds: u64) -> ACPI_STATUS {
    if milliseconds == 0 {
        return AE_OK;
    }
    sched::delay_ms(milliseconds as usize, false);
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsStall(microseconds: u64) -> ACPI_STATUS {
    if microseconds == 0 {
        return AE_OK;
    }
    
    hal::delay_ns((microseconds as usize) * 1000);
    AE_OK
}

struct AcpiMutex {
    sem: KSem,
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsCreateMutex(out_handle: *mut *mut c_void) -> ACPI_STATUS {
    if out_handle.is_null() {
        return AE_BAD_PARAMETER;
    }
    let m = Box::new_in(AcpiMutex { sem: KSem::new(1, 1) }, PoolAllocatorGlobal);
    unsafe { *out_handle = Box::into_raw_with_allocator(m).0 as *mut c_void; }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsDeleteMutex(handle: *mut c_void) -> ACPI_STATUS {
    if handle.is_null() {
        return AE_BAD_PARAMETER;
    }
    unsafe { drop(Box::from_raw_in(handle as *mut AcpiMutex, PoolAllocatorGlobal)); }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsAcquireMutex(handle: *mut c_void, timeout: u16) -> ACPI_STATUS {
    if handle.is_null() {
        return AE_BAD_PARAMETER;
    }
    let m = unsafe { &*(handle as *const AcpiMutex) };
    let ok = if timeout == ACPI_WAIT_FOREVER {
        m.sem.wait(false)
    } else {
        m.sem.wait_with_timeout(timeout as usize, false)
    };
    if ok.is_ok() { AE_OK } else { AE_TIME }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsReleaseMutex(handle: *mut c_void) -> ACPI_STATUS {
    if handle.is_null() {
        return AE_BAD_PARAMETER;
    }
    let m = unsafe { &*(handle as *const AcpiMutex) };
    m.sem.signal();
    AE_OK
}

struct AcpiSemaphore {
    sem: KSem,
    max_units: u32,
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsCreateSemaphore(
    max_units: u32,
    initial_units: u32,
    out_handle: *mut *mut c_void,
) -> ACPI_STATUS {
    if out_handle.is_null() || max_units == 0 || initial_units > max_units {
        return AE_BAD_PARAMETER;
    }
    let s = Box::new_in(AcpiSemaphore {
        sem: KSem::new(initial_units as isize, max_units as isize),
        max_units,
    }, PoolAllocatorGlobal);
    unsafe { *out_handle = Box::into_raw_with_allocator(s).0 as *mut c_void; }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsDeleteSemaphore(handle: *mut c_void) -> ACPI_STATUS {
    if handle.is_null() {
        return AE_BAD_PARAMETER;
    }
    unsafe { drop(Box::from_raw_in(handle as *mut AcpiSemaphore, PoolAllocatorGlobal)); }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsWaitSemaphore(handle: *mut c_void, units: u32, timeout: u16) -> ACPI_STATUS {
    if handle.is_null() || units == 0 {
        return AE_BAD_PARAMETER;
    }
    let s = unsafe { &*(handle as *const AcpiSemaphore) };
    // KSem operates on single-unit increments, so multi-unit acquire is a
    // loop. ACPICA almost always asks for 1; rare exceptions tolerate this.
    for _ in 0..units {
        let ok = if timeout == ACPI_WAIT_FOREVER {
            s.sem.wait(false)
        } else {
            s.sem.wait_with_timeout(timeout as usize, false)
        };
        if ok.is_err() {
            return AE_TIME;
        }
    }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsSignalSemaphore(handle: *mut c_void, units: u32) -> ACPI_STATUS {
    if handle.is_null() || units == 0 {
        return AE_BAD_PARAMETER;
    }
    let s = unsafe { &*(handle as *const AcpiSemaphore) };
    if units > s.max_units {
        return AE_BAD_PARAMETER;
    }
    // KSem clamps at max_count internally; safe to call N times.
    for _ in 0..units {
        s.sem.signal();
    }
    AE_OK
}

// We hand ACPICA the raw 8-byte hal::Spinlock. We do not use sync::Spinlock<T>
// here because ACPICA wants to control irq save/restore explicitly through
// ACPI_CPU_FLAGS, and sync::Spinlock<T> bundles that into its own guard.

// AcpiCPUFlags is u64 per ACPICA; we encode the prior interrupt state in
// bit 0.
type AcpiCPUFlags = u64;

#[unsafe(no_mangle)]
extern "C" fn AcpiOsCreateLock(out_handle: *mut *mut c_void) -> ACPI_STATUS {
    if out_handle.is_null() {
        return AE_BAD_PARAMETER;
    }
    let lock = Box::new_in(HalSpinlock::new(), PoolAllocatorGlobal);
    unsafe { *out_handle = Box::into_raw_with_allocator(lock).0 as *mut c_void; }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsDeleteLock(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    unsafe { drop(Box::from_raw_in(handle as *mut HalSpinlock, PoolAllocatorGlobal)); }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsAcquireLock(handle: *mut c_void) -> AcpiCPUFlags {
    if handle.is_null() {
        return 0;
    }
    let lock = unsafe { &*(handle as *const HalSpinlock) };
    let int_was_enabled = hal::disable_interrupts();
    lock.lock();
    int_was_enabled as u64
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsReleaseLock(handle: *mut c_void, flags: AcpiCPUFlags) {
    if handle.is_null() {
        return;
    }
    let lock = unsafe { &*(handle as *const HalSpinlock) };
    lock.unlock();
    hal::enable_interrupts((flags & 1) != 0);
}

// Per-IRQ shim called by the kernel ISR layer. Resolves the SciHandler from
// the bookkeeping map and forwards. Returns true if ACPICA claimed the IRQ.
extern "C" fn sci_irq_wrapper(context: *mut c_void) -> bool {
    // We stash the IRQ number as the context here so we can look up the
    // matching ACPICA handler. The actual ACPICA `context` value is kept
    // inside the SciHandler descriptor.
    let irq = context as usize as u32;
    let map = SCI_HANDLERS.lock();
    if let Some(entry) = map.get(&irq) {
        let result = (entry.handler)(entry.context);
        result == ACPI_INTERRUPT_HANDLED
    } else {
        false
    }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsInstallInterruptHandler(
    interrupt_number: u32,
    handler: AcpiHandlerSCI,
    context: *mut c_void,
) -> ACPI_STATUS {
    let mut map = SCI_HANDLERS.lock();
    if map.contains_key(&interrupt_number) {
        return AE_ERROR;
    }

    // Register the wrapper. We pass the IRQ number as the kernel-side
    // context so the wrapper can look us up; the real ACPICA context is held
    // by us in SciHandler.context.
    let int_stat = hal::disable_interrupts();
    let vector = allocate_vector();
    let kernel_handle = io_install_interrupt_handler(
        vector,
        interrupt_number as isize,
        interrupt_number as usize as *mut c_void,
        sci_irq_wrapper,
        true,  // SCI is active-high
        false // and level-triggered
    );

    map.insert(interrupt_number, SciHandler {
        handler,
        context,
        kernel_handle,
    });

    hal::enable_interrupts(int_stat);

    acpica_log!("InstallInterruptHandler irq={} ctx={:p}", interrupt_number, context);
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsRemoveInterruptHandler(
    interrupt_number: u32,
    _handler: AcpiHandlerSCI,
) -> ACPI_STATUS {
    let entry = SCI_HANDLERS.lock().remove(&interrupt_number);
    match entry {
        Some(entry) => {
            io_remove_interrupt_handler(entry.kernel_handle);
            acpica_log!("RemoveInterruptHandler irq={}", interrupt_number);
            AE_OK
        }
        None => AE_NOT_FOUND
    }
}

// ReadMemory / WriteMemory pass physical addresses. We map a transient page
// through the existing virtual_allocator, do one volatile access of the right
// width, then unmap. ACPICA accesses are infrequent enough that this is fine.
fn map_one(phys: ACPI_PHYSICAL_ADDRESS, len: usize) -> Option<(usize, MmioMapping, usize)> {
    let phys_aligned = align_down(phys as usize, PAGE_SIZE);
    let offset = (phys as usize) - phys_aligned;
    let size = align_up(offset + len, PAGE_SIZE);

    let flags = PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC | PageDescriptor::MMIO;
    let layout = Layout::from_size_align(size, PAGE_SIZE).ok()?;
    let virt = allocate_memory(layout, flags).ok()? as usize;
    if map_memory(phys_aligned, virt, size, PageDescriptor::MMIO).is_err() {
        let _ = deallocate_memory(virt as *mut u8, layout, flags);
        return None;
    }
    Some((virt + offset, MmioMapping { virt_base: virt, mapped_size: size, flags }, offset))
}

fn unmap_one(m: MmioMapping) {
    let _ = unmap_memory(m.virt_base, m.mapped_size, PageDescriptor::MMIO);
    let layout = Layout::from_size_align(m.mapped_size, PAGE_SIZE).unwrap();
    let _ = deallocate_memory(m.virt_base as *mut u8, layout, m.flags);
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsReadMemory(
    phys_addr: ACPI_PHYSICAL_ADDRESS,
    value: *mut u64,
    width: u32,
) -> ACPI_STATUS {
    if value.is_null() {
        return AE_BAD_PARAMETER;
    }
    let bytes = (width / 8) as usize;
    let map = match map_one(phys_addr, bytes) {
        Some(m) => m,
        None => return AE_ERROR,
    };
    let v_addr = map.0;
    let result: u64 = match width {
        8  => unsafe { read_volatile(v_addr as *const u8)  as u64 },
        16 => unsafe { read_volatile(v_addr as *const u16) as u64 },
        32 => unsafe { read_volatile(v_addr as *const u32) as u64 },
        64 => unsafe { read_volatile(v_addr as *const u64) },
        _ => { unmap_one(map.1); return AE_BAD_PARAMETER; }
    };
    core::sync::atomic::fence(Ordering::SeqCst);
    unmap_one(map.1);
    unsafe { ptr::write_unaligned(value, result); }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsWriteMemory(
    phys_addr: ACPI_PHYSICAL_ADDRESS,
    value: u64,
    width: u32,
) -> ACPI_STATUS {
    let bytes = (width / 8) as usize;
    let map = match map_one(phys_addr, bytes) {
        Some(m) => m,
        None => return AE_ERROR,
    };
    let v_addr = map.0;
    match width {
        8  => unsafe { write_volatile(v_addr as *mut u8,  value as u8) },
        16 => unsafe { write_volatile(v_addr as *mut u16, value as u16) },
        32 => unsafe { write_volatile(v_addr as *mut u32, value as u32) },
        64 => unsafe { write_volatile(v_addr as *mut u64, value) },
        _ => { unmap_one(map.1); return AE_BAD_PARAMETER; }
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    unmap_one(map.1);
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsReadPort(port: u16, value: *mut u32, width: u32) -> ACPI_STATUS {
    if value.is_null() {
        return AE_BAD_PARAMETER;
    }
    let read = unsafe {
        match width {
            8  => hal::read_port_u8(port)  as u32,
            16 => hal::read_port_u16(port) as u32,
            32 => hal::read_port_u32(port),
            _  => return AE_BAD_PARAMETER,
        }
    };
    unsafe { ptr::write_unaligned(value, read); }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsWritePort(port: u16, value: u32, width: u32) -> ACPI_STATUS {
    unsafe {
        match width {
            8  => hal::write_port_u8(port,  value as u8),
            16 => hal::write_port_u16(port, value as u16),
            32 => hal::write_port_u32(port, value),
            _  => return AE_BAD_PARAMETER,
        }
    }
    AE_OK
}

const PCI_CONFIG_ADDR: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

fn pci_config_select(bus: u32, device: u32, function: u32, reg: u32) {
    let addr: u32 = (1 << 31)
        | ((bus & 0xff) << 16)
        | ((device & 0x1f) << 11)
        | ((function & 0x7) << 8)
        | (reg & 0xfc);
    unsafe { hal::write_port_u32(PCI_CONFIG_ADDR, addr); }
}

fn pci_read_dword(bus: u32, device: u32, function: u32, reg: u32) -> u32 {
    pci_config_select(bus, device, function, reg);
    unsafe { hal::read_port_u32(PCI_CONFIG_DATA) }
}

fn pci_write_dword(bus: u32, device: u32, function: u32, reg: u32, value: u32) {
    pci_config_select(bus, device, function, reg);
    unsafe { hal::write_port_u32(PCI_CONFIG_DATA, value); }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsReadPciConfiguration(
    handle: *const AcpiPciId,
    reg: u32,
    value: *mut u64,
    width: u32,
) -> ACPI_STATUS {
    if handle.is_null() || value.is_null() {
        return AE_BAD_PARAMETER;
    }
    let pci = unsafe { &*handle };
    // AcpiPciId is #[repr(C, packed)] but the fields are all u16
    // aligned to 2; reading individual fields by-value is fine.
    let bus = pci.bus as u32;
    let dev = pci.device as u32;
    let func = pci.function as u32;
    let word = pci_read_dword(bus, dev, func, reg);
    let shift = ((reg & 0x3) * 8) as u32;

    let v: u64 = match width {
        8  => ((word >> shift) & 0xff) as u64,
        16 => ((word >> shift) & 0xffff) as u64,
        32 => word as u64,
        64 => {
            let hi = pci_read_dword(bus, dev, func, reg + 4);
            ((hi as u64) << 32) | (word as u64)
        }
        _ => return AE_BAD_PARAMETER,
    };
    unsafe { ptr::write_unaligned(value, v); }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsWritePciConfiguration(
    handle: *const AcpiPciId,
    reg: u32,
    value: u64,
    width: u32,
) -> ACPI_STATUS {
    if handle.is_null() {
        return AE_BAD_PARAMETER;
    }
    let pci = unsafe { &*handle };
    let bus = pci.bus as u32;
    let dev = pci.device as u32;
    let func = pci.function as u32;

    match width {
        8 => {
            let shift = ((reg & 0x3) * 8) as u32;
            let mut word = pci_read_dword(bus, dev, func, reg);
            word = (word & !(0xff << shift)) | (((value as u32) & 0xff) << shift);
            pci_write_dword(bus, dev, func, reg, word);
        }
        16 => {
            let shift = ((reg & 0x3) * 8) as u32;
            let mut word = pci_read_dword(bus, dev, func, reg);
            word = (word & !(0xffff << shift)) | (((value as u32) & 0xffff) << shift);
            pci_write_dword(bus, dev, func, reg, word);
        }
        32 => pci_write_dword(bus, dev, func, reg, value as u32),
        64 => {
            pci_write_dword(bus, dev, func, reg, value as u32);
            pci_write_dword(bus, dev, func, reg + 4, (value >> 32) as u32);
        }
        _ => return AE_BAD_PARAMETER,
    }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsPrintStr(s: *const c_char) {
    if s.is_null() { return; }
    let msg = unsafe { CStr::from_ptr(s).to_str().unwrap_or("<bad-utf8>") };
    debug!("ACPICA: {}", msg.trim_end_matches('\n'));
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsRedirectOutput(_target: *mut c_void) {
    // Output redirection isn't useful for us
}

// ACPICA's own table manager handles most table lookups internally. These
// hooks are kept minimal: walk the XSDT from the bootloader-supplied RSDP.
#[unsafe(no_mangle)]
extern "C" fn AcpiOsGetTableByAddress(
    addr: ACPI_PHYSICAL_ADDRESS,
    out_table: *mut *mut AcpiTableHeader,
) -> ACPI_STATUS {
    if out_table.is_null() || addr == 0 {
        return AE_BAD_PARAMETER;
    }
    
    // Currently, all ACPI tables are identity mapped
    unsafe { ptr::write_unaligned(out_table, addr as *mut AcpiTableHeader); }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsGetTableByName(
    signature: *const c_char,
    _instance: u32,
    out_table: *mut *mut AcpiTableHeader,
    out_phys: *mut ACPI_PHYSICAL_ADDRESS,
) -> ACPI_STATUS {
    if signature.is_null() || out_table.is_null() {
        return AE_BAD_PARAMETER;
    }
    let sig = unsafe { CStr::from_ptr(signature) }
        .to_str()
        .unwrap_or("");
    let rsdp = match BOOT_INFO.get() {
        Some(bi) => bi.rsdp as *const u8,
        None => return AE_NOT_FOUND,
    };
    match fetch_acpi_table_raw(rsdp, sig) {
        Some(p) => {
            unsafe {
                ptr::write_unaligned(out_table, p as *mut AcpiTableHeader);
                if !out_phys.is_null() {
                    ptr::write_unaligned(out_phys, p as ACPI_PHYSICAL_ADDRESS);
                }
            }
            AE_OK
        }
        None => AE_NOT_FOUND,
    }
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsGetTableByIndex(
    _index: u32,
    _out_table: *mut *mut AcpiTableHeader,
    _instance: *mut u32,
    _out_phys: *mut ACPI_PHYSICAL_ADDRESS,
) -> ACPI_STATUS {
    // Not commonly used by core ACPICA paths. Return AE_NOT_FOUND so callers
    // fall back to ACPICA's internal table manager.
    AE_NOT_FOUND
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsGetTimer() -> u64 {
    // ACPICA expects 100-nanosecond units, monotonically increasing.
    let hpet = HPET.lock();
    let ticks = hpet.read_counter();
    let period_fs = hpet.clk_period as u64;     // femtoseconds per tick
    drop(hpet);

    // 100 ns = 100_000_000 fs.
    // units_of_100ns = ticks * period_fs / 100_000_000
    // For typical HPET periods (~1e7 fs) and 64-bit tick counters this
    // doesn't overflow within any reasonable system uptime.
    ticks.wrapping_mul(period_fs) / 100_000_000
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsSignal(function: u32, info_ptr: *mut c_void) -> ACPI_STATUS {
    match function {
        ACPI_SIGNAL_FATAL => {
            info!("ACPICA: SIGNAL_FATAL (info={:p})", info_ptr);
        }
        ACPI_SIGNAL_BREAKPOINT => {
            info!("ACPICA: SIGNAL_BREAKPOINT (info={:p})", info_ptr);
        }
        other => {
            info!("ACPICA: unknown signal {} (info={:p})", other, info_ptr);
        }
    }
    AE_OK
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsGetLine(
    _buffer: *mut c_char,
    _buffer_length: u32,
    _bytes_read: *mut u32
) -> ACPI_STATUS {
    // Only the AML debugger uses this — we don't expose it.
    AE_SUPPORT
}

#[unsafe(no_mangle)]
extern "C" fn AcpiOsEnterSleep(sleep_state: u8, reg_a: u32, reg_b: u32) -> ACPI_STATUS {
    // For now, we just give the green flag here
    acpica_log!("AcpiOsEnterSleep state=S{} PM1A={:#X} PM1B={:#X}", sleep_state, reg_a, reg_b);
    AE_OK
}
