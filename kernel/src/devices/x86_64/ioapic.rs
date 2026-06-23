use crate::BOOT_INFO;
use crate::Spinlock;
use kernel_intf::{debug, info};
use common::{MemoryRegion, PAGE_SIZE};
use crate::{RemapEntry, RemapType::*, REMAP_LIST};
use crate::mem::PageDescriptor;
use crate::hal::write_port_u8;
use core::mem::size_of;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::ptr::{read_volatile, write_volatile};

#[cfg(feature="acpi")] 
use {common::madt::*, crate::acpica, acpi_intf::*};

const MAX_IOAPIC: usize = 4;
const MAX_INT_OVERRIDE: usize = 20;

const IOAPIC_VER_OFFSET: u32 = 0x1;
const IOAPIC_REDIR_START_OFFSET: u32 = 0x10;

static NUM_IOAPIC: AtomicUsize = AtomicUsize::new(0);
static NUM_IOAPIC_REL: AtomicUsize = AtomicUsize::new(0);
static NUM_INT_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct Ioapic {
    id: usize,
    base_address: usize,
    gsi: usize
}

#[derive(Debug, Clone, Copy)]
struct IntOverride {
    irq: usize,
    gsi: usize,
    is_active_high: bool,
    is_edge_triggered: bool
}

static IOAPIC_LIST: Spinlock<[Ioapic; MAX_IOAPIC]> = Spinlock::new([Ioapic { id: 0, base_address: 0, gsi: 0}; MAX_IOAPIC]);
static OVERRIDE_LIST: Spinlock<[IntOverride; MAX_INT_OVERRIDE]> = Spinlock::new([IntOverride { irq: 0, gsi: 0, is_edge_triggered: true, is_active_high: true}; MAX_INT_OVERRIDE]);

// Register legacy type IRQ with IOAPIC
pub fn set_redirection_entry(
    enable: bool, 
    irq: usize, 
    cpu_lapic_id: usize, 
    vector: usize, 
    active_high: bool, 
    is_edge_triggered: bool
) {
    assert!(cpu_lapic_id <= 0xf);
    
    // First apply any interrupt source overrides
    let irq_override = OVERRIDE_LIST.lock().iter().find(|item| {
        item.irq == irq
    }).map_or(IntOverride {irq: irq, gsi: irq, is_active_high: active_high, is_edge_triggered}, |int| {
        int.clone()
    });

    let msg_type = 0u32 << 8;
    let upper_dword = (cpu_lapic_id as u32) << 24;

    let masked_lower_dword = (1u32 << 16) |
    (((if irq_override.is_edge_triggered {0} else {1}) as u32) << 15) | 
    (((if irq_override.is_active_high {0} else {1}) as u32) << 13) | (msg_type) | ((vector as u32) & 0xff);

    let lower_dword = (((if !enable {1} else {0}) as u32) << 16) |
    (((if irq_override.is_edge_triggered {0} else {1}) as u32) << 15) | 
    (((if irq_override.is_active_high {0} else {1}) as u32) << 13) | (msg_type) | ((vector as u32) & 0xff);

    if enable {
        info!("Adding IOAPIC redirection entry for src_irq:{}, dest_irq:{}", irq_override.irq, irq_override.gsi);
    }
    else {
        info!("Disabling IOAPIC redirection entry for src_irq:{}, dest_irq:{}", irq_override.irq, irq_override.gsi);
    }

    // Now we need to find out which IOAPIC this irq belongs to
    let stat = IOAPIC_LIST.lock().iter().find(|item| {
        let iosel = item.base_address as *mut u32;
        let iowin = (item.base_address + 0x10) as *const u32;  

        // Select IOAPICVER register
        unsafe {
            write_volatile(iosel, IOAPIC_VER_OFFSET);
        }
        let max_redir_entries = unsafe {
            (read_volatile(iowin) >> 16) & 0xff
        } as usize + 1;

        irq_override.gsi >= item.gsi && irq_override.gsi < item.gsi + max_redir_entries
    }).and_then(|item| {
        let iosel = item.base_address as *mut u32;
        let iowin = (item.base_address + 0x10) as *mut u32;  

        // Select IOAPIC Redirection entry
        unsafe {
            let redir = IOAPIC_REDIR_START_OFFSET + 2 * (irq_override.gsi - item.gsi) as u32;

            // First mask the entry to avoid delivery while reprogramming.
            write_volatile(iosel, redir);
            write_volatile(iowin, masked_lower_dword);

            // Update destination.
            write_volatile(iosel, redir + 1);
            write_volatile(iowin, upper_dword);

            // Update the full lower dword.
            write_volatile(iosel, redir);
            write_volatile(iowin, lower_dword);
        }

        Some(true)
    });
    
    if stat.is_none() {
        panic!("No IOAPIC found for src_irq:{} with redirection entry:{}!", irq_override.irq, irq_override.gsi);
    }
}


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
            MADT_TYPE_IOAPIC => {
                assert!(NUM_IOAPIC.load(Ordering::Acquire) < MAX_IOAPIC, "Number of IOAPIC in system exceeded memory limitation!");

                let entry = unsafe {
                    &*(ptr as *const IoapicEntry)
                };
                IOAPIC_LIST.lock()[NUM_IOAPIC.fetch_add(1, Ordering::Acquire)] = Ioapic {
                    id: entry.id as usize,
                    base_address: entry.addr as usize,
                    gsi: entry.gsi as usize
                };

                // Here, we're making a crucial assumption that each IOAPIC window registers are located in a separate page
                REMAP_LIST.lock().add_node(RemapEntry { 
                    value: MemoryRegion { 
                        base_address: entry.addr as usize,
                        size: PAGE_SIZE 
                    }, 
                    map_type: OffsetMapped(|virt_addr| {
                        info!("Relocated IOAPIC-{} to address:{:#X}", NUM_IOAPIC_REL.load(Ordering::Acquire), virt_addr);
                        IOAPIC_LIST.lock()[NUM_IOAPIC_REL.fetch_add(1, Ordering::Acquire)].base_address = virt_addr;
                    }),
                    flags: PageDescriptor::MMIO 
                }).unwrap();
            },
            INT_SRC_OVERRIDE => {
                assert!(NUM_INT_OVERRIDE.load(Ordering::Acquire) < MAX_INT_OVERRIDE, "Number of Interrupt source overrides in system exceeded memory limitation!");

                let entry = unsafe {
                    &*(ptr as *const IntEntry)
                };

                let override_desc = IntOverride { 
                    irq: entry.src as usize, gsi: entry.gsi as usize, is_active_high: (entry.flags & 0x3) == 0x1,
                    is_edge_triggered: ((entry.flags >> 2) & 0x3) != 0x3
                };
   
                debug!("{:?}", override_desc);

                OVERRIDE_LIST.lock()[NUM_INT_OVERRIDE.fetch_add(1, Ordering::Acquire)] = override_desc;
            },
            _ => {
            }
        }

        ptr += hdr.length as usize;
    }
}


#[cfg(feature="acpi")]
pub fn early_init() {
    
    let madt_tab = acpica::fetch_acpi_table::<AcpiTableMadt>(
        BOOT_INFO.get().unwrap().rsdp as *const u8).expect("No MADT ACPI table found!");

    // Disable PIC (8259 controller)
    unsafe {
        write_port_u8(0x21, 0xFF); // Mask all IRQs on master PIC
        write_port_u8(0xA1, 0xFF); // Mask all IRQs on slave PIC
    }

    parse_madt(madt_tab);

    info!("Found {} IOAPIC in system", NUM_IOAPIC.load(Ordering::Relaxed));
    info!("Found {} source interrupt overrides", NUM_INT_OVERRIDE.load(Ordering::Relaxed));
}