use crate::hal::get_bsp_lapic_id;
use crate::mem::{PageDescriptor, map_memory, reserve_virtual_memory};
use crate::infra;
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use crate::BOOT_INFO;
use crate::mem::PHY_MEM_CB;
use kernel_intf::list::{List, DynList};
use crate::sync::Spinlock;
use crate::cpu::{self, MAX_CPUS};
use kernel_intf::{debug, info};
use alloc::alloc::Layout;
use common::PAGE_SIZE;
use super::page_mapper;
use super::lapic;
use super::timer;
use super::cpu::get_core;
use super::tables;
use super::cpu_regs;
use super::sleep;
use super::disable_interrupts;
use super::syscall;

#[cfg(feature = "acpi")]
use {crate::acpica, common::madt::*, acpi_intf::*};

#[allow(dead_code)]
#[derive(Debug)]
struct Lapic {
    id: usize,
    uid: usize,
    is_x2apic: bool
}

#[allow(dead_code)]
#[derive(Debug)]
struct Nmi {
    uid: usize,
    pin: u8,
    is_active_high: bool,
    is_edge_triggered: bool
}

static LAPIC_LIST: Spinlock<DynList<Lapic>> = Spinlock::new(List::new());
static NMI_LIST: Spinlock<DynList<Nmi>> = Spinlock::new(List::new());

static AP_INIT_COMPLETE: AtomicBool = AtomicBool::new(false);
static AP_CORES_INIT: AtomicUsize = AtomicUsize::new(1);
static AP_CORES_ID: AtomicUsize = AtomicUsize::new(1);
static AP_TRAMPOLINE: &[u8] = include_bytes!(env!("TRAMPOLINE_BIN"));

include!("asm/trampoline_offsets.rs");

#[cfg(feature="acpi")]
fn parse_madt(madt: &AcpiTableMadt) {
    let madt_start = madt as *const _ as usize;
    let madt_len = madt.header.length as usize;

    let entries_start = madt_start + size_of::<AcpiTableMadt>();
    let entries_end = madt_start + madt_len;

    let mut ptr = entries_start;

    while ptr < entries_end {
        let hdr = unsafe {
            &*(ptr as *const MadtEntryHeader) 
        };

        // Sanity check 
        if (hdr.length as usize) < size_of::<MadtEntryHeader>() {
            break;
        }

        match hdr.entry_type {
            XLAPIC => {
                let entry = unsafe {
                    &*(ptr as *const MadtLapic)
                };

                // LAPIC is enabled
                if entry.flags & 0x1 != 0 {
                    let lapic = Lapic {
                        id: entry.apic_id as usize,
                        uid: entry.uid as usize,
                        is_x2apic: false
                    };

                    debug!("Adding LAPIC entry {:?}", lapic);

                    // We're just going to assume here for now that firmware 
                    // won't provide the same lapic as both xlapic and x2lapic structure
                    // This is a fair assumption (ACPI spec mentions it)
                    LAPIC_LIST.lock().add_node(lapic).expect("Couldn't store lapic info in list!");
                }
            },
            X2LAPIC => {
                let entry = unsafe {
                    &*(ptr as *const MadtX2Lapic)
                };

                // LAPIC is enabled
                if entry.flags & 0x1 != 0 {
                    let lapic = Lapic {
                        id: entry.apic_id as usize,
                        uid: entry.uid as usize,
                        is_x2apic: true
                    };
                    
                    debug!("Adding X2LAPIC entry {:?}", lapic);

                    LAPIC_LIST.lock().add_node(lapic).expect("Couldn't store lapic info in list!");
                }
            }
            XAPIC_NMI => {
                let entry = unsafe {
                    &*(ptr as *const MadtLapicNmi)
                };

                let nmi = Nmi {
                    uid: entry.uid as usize,
                    pin: entry.pin,
                    is_active_high: (entry.flags & 0x3) == 0x1,
                    is_edge_triggered: ((entry.flags >> 2) & 0x3) != 0x3
                };
                    
                debug!("Adding LAPIC NMI entry {:?}", nmi);

                NMI_LIST.lock().add_node(nmi).expect("Couldn't add NMI pin data to NMI list!");
            },
            X2APIC_NMI => {
                let entry = unsafe {
                    &*(ptr as *const MadtX2LapicNmi)
                };

                let nmi = Nmi {
                    uid: entry.uid as usize,
                    pin: entry.pin,
                    is_active_high: (entry.flags & 0x3) == 0x2,
                    is_edge_triggered: ((entry.flags >> 2) & 0x3) != 0x3
                };

                debug!("Adding x2LAPIC NMI entry {:?}", nmi);
                NMI_LIST.lock().add_node(nmi).expect("Couldn't add NMI pin data to NMI list!");
            }
            _ => {
            }
        }

        ptr += hdr.length as usize;
    }
}

// The stack will be fixed per ap, so we won't do it here
unsafe fn patch_trampoline(load_addr: *mut u8, pml4: u32, ap_init: u64) {
    debug!("Patching trampoline with pml4={:#X} and ap_init_address={:#X}", pml4, ap_init);
    unsafe {
        let gdt = load_addr.add(GDT);
        let gdt_desc = load_addr.add(GDT_DESC);

        (gdt_desc.add(2) as *mut u32).write_unaligned(gdt as u32);
        let pml4_phys = load_addr.add(PML4_PHYS);
        let ap_entry = load_addr.add(AP_ENTRY);
        (pml4_phys as *mut u32).write(pml4);
        (ap_entry as *mut u64).write(ap_init);

        // Apply manual relocation to instructions

        // _patch1 -> 0f 01 modr/m addrbyte1 addrbyte2
        (load_addr.add(_PATCH1 + 3) as *mut u16).write_unaligned(gdt_desc as u16);
        
        // _patch2 -> 66 ea addrbyte1 addrbyte2 addrbyte3 addrbyte4
        (load_addr.add(_PATCH2 + 2) as *mut u32).write_unaligned((load_addr.addr() + PMODE_ENTRY) as u32);
        
        // _patch3 -> a1 addrbyte1-addrbyte4
        (load_addr.add(_PATCH3 + 1) as *mut u32).write_unaligned((load_addr.addr() + PML4_PHYS) as u32);
        
        // _patch4 -> ea addrbyte1-addrbyte4
        (load_addr.add(_PATCH4 + 1) as *mut u32).write_unaligned((load_addr.addr() + LMODE_ENTRY) as u32);
    } 
}

#[cfg(feature = "acpi")]
pub fn init() {

    let madt_tab = acpica::fetch_acpi_table::<AcpiTableMadt>(
        BOOT_INFO.get().unwrap().rsdp as *const u8).expect("No MADT ACPI table found!");

    parse_madt(madt_tab);
    activate_local_core_nmi_trap();

    let total_cores = LAPIC_LIST.lock().get_nodes();
    let total_cores_capped = total_cores.min(MAX_CPUS);
    cpu::set_total_cores(total_cores_capped);
    
    info!("Found {} enabled logical cores in system", total_cores);

    if total_cores == 1 {
        info!("Found only 1 core in system. Skipping rest of smp init...");
        return;
    }

    if total_cores > MAX_CPUS {
        info!("Found more than {} cores in system ({}). Aris will only use {} of them...", MAX_CPUS, total_cores, MAX_CPUS);
    }

    let tramp_start = AP_TRAMPOLINE.as_ptr().addr();
    let tramp_size = AP_TRAMPOLINE.len(); 

    info!("Trampoline start={:#X}, size={}", tramp_start, tramp_size);

    let ap_start_code = {
        let mut frame_allocator = PHY_MEM_CB.get().unwrap().lock();
        // Theoretically, any address below 1MB should be fine. However, in practice have noted that
        // addresses above 0x80000 seem to be having some issues
        frame_allocator.configure_upper_limit(0x80000);
        frame_allocator.configure_lower_limit(0);
        
        let addr = frame_allocator.allocate(Layout::from_size_align(tramp_size, PAGE_SIZE).unwrap());

        // Switch back to 4GB limit (needed by MP init)
        frame_allocator.configure_upper_limit((1 << 32) - 1);

        addr
    }.expect("Unable to find suitable memory region < 1MB for ap init code!!");

    info!("Trampoline mapped to region: {:#X}", ap_start_code.addr());

    reserve_virtual_memory(ap_start_code.addr(), Layout::from_size_align(tramp_size, PAGE_SIZE).unwrap())
    .expect("Failed to reserve identity mapped address for trampoline in kernel address space");

    map_memory(ap_start_code as usize, ap_start_code as usize, tramp_size, PageDescriptor::VIRTUAL)
    .expect("Failed to identity map ap trampoline region to kernel address space!");

    // Copy the trampoline to < 1MB region
    unsafe {
        core::ptr::copy_nonoverlapping(
            tramp_start as *const u8,
            ap_start_code,
            tramp_size,
        );
        
        // Here, we manually fix all the addresses in the trampoline
        // PML4 address + bit 3 is set to enable PWT mode for base table
        patch_trampoline(ap_start_code, page_mapper::get_kernel_pml4() as u32 | (1 << 3), ap_init as *const () as u64);
    }
    
    let bsp_id = get_bsp_lapic_id();
    for (idx, core) in LAPIC_LIST.lock().iter().take(total_cores_capped).enumerate() {
        if core.id == bsp_id {
            continue;
        } 

        // This registers CPU at kernel level
        cpu::register_cpu();

        let stack_base = cpu::get_worker_stack(idx);

        debug!("Setting stack base {:#X} for core {}", stack_base, idx);
        
        // Each AP gets their own stack. However, due to the way our trampoline is structured
        // we only let one ap run the trampoline at a time
        unsafe {
            (ap_start_code.add(AP_STACK_TOP) as *mut u64)
                .write_volatile(stack_base as u64);
        }
        
        AP_INIT_COMPLETE.store(false, Ordering::SeqCst);
        let sipi_vector = (ap_start_code.addr() >> 12) as u8;
        debug!("Sending INIT-SIPI-SIPI sequence to core:{} with apic_id:{} at vector: {}", idx, core.id, sipi_vector);

        lapic::send_init_ipi(core.id as u32);
        timer::delay_ns(10_000_000);
        lapic::send_init_deassert(core.id as u32);
        timer::delay_ns(200_000);

        lapic::send_sipi(core.id as u32, sipi_vector);
        timer::delay_ns(200_000);
        lapic::lapic_wait_icr_idle();

        lapic::send_sipi(core.id as u32, sipi_vector);
        timer::delay_ns(200_000);
        lapic::lapic_wait_icr_idle();

        // Wait for core to complete
        while AP_INIT_COMPLETE.load(Ordering::SeqCst) == false {
            core::hint::spin_loop();
        }
    }

    // From this point on, pages can be freely allocated from any range in the physical address space
    PHY_MEM_CB.get().unwrap().lock().disable_limits();

    // Wait for all cores to initialize before proceeding
    while AP_CORES_INIT.load(Ordering::Acquire) < total_cores_capped {
        core::hint::spin_loop();
    }
    
    infra::enable_mp_init(); 
    super::enable_invalidation();
}

fn activate_local_core_nmi_trap() {
    // First find the uid associated with this cpu
    let apic_id = lapic::get_lapic_id();
    for core in LAPIC_LIST.lock().iter() {
        if core.id == apic_id {
            // Now find the nmi entry associated with this value
            for nmi in NMI_LIST.lock().iter() {
                // UID of 0xff means that this nmi entry is associated with every logical core
                if nmi.uid == core.uid || nmi.uid == 0xff {
                    assert!(nmi.pin == 0 || nmi.pin == 1);
                    lapic::configure_nmi(nmi.pin == 0, nmi.is_edge_triggered);
                    break;
                }
            }            
            
            break;
        }
    }
}


#[unsafe(no_mangle)]
extern "C" fn ap_init() -> ! {
    disable_interrupts();
    
    // Signal BSP that this core is up
    let core = AP_CORES_ID.fetch_add(1, Ordering::SeqCst);
    AP_INIT_COMPLETE.store(true, Ordering::SeqCst);
    lapic::init();
    super::init_per_cpu_data(core);
    
    info!("Starting AP init for core {}", get_core());
    debug!("gs_base={:#X}, kernel_gs_base={:#X} on core {}", super::get_per_cpu_kernel_base(), super::get_per_cpu_base(),
 get_core());
    crate::mem::ap_init();
    activate_local_core_nmi_trap();
    cpu_regs::init();
    tables::build_gdt();
    tables::register_tables();
    syscall::init();
    timer::init();

    debug!("APIC base: {:#X}", lapic::get_apic_base());
    info!("AP core {} going to sleep", get_core());
    AP_CORES_INIT.fetch_add(1, Ordering::Release);
    

    // This will internally enable interrupts
    sleep();
}