#![allow(dead_code)]

use crate::hal::{disable_interrupts, enable_interrupts};
use crate::mem::{PageDescriptor, allocate_memory, map_memory};

use super::asm::{rdmsr, wrmsr};
use super::handlers::{SPURIOUS_VECTOR, ERROR_VECTOR, TIMER_VECTOR};
use common::PAGE_SIZE;
use core::alloc::Layout;

const APIC_BASE_OFFSET: u32 = 0x1b;
const APIC_ID_OFFSET: u32 = 0x802;
const APIC_EOI_OFFSET: u32 = 0x80b;
const TASK_REG_OFFSET: u32 = 0x808;
const TIMER_LVT: u32 = 0x832;
const THERMAL_LVT: u32 = 0x833;
const PERF_CNTR_LVT: u32 = 0x834;
const LINT0_LVT: u32 = 0x835;
const LINT1_LVT: u32 = 0x836;
const ERROR_LVT: u32 = 0x837;
const ERROR_STS_OFFSET: u32 = 0x828;
const INITIAL_CNT_OFFSET: u32 = 0x838;
const CURRENT_CNT_OFFSET: u32 = 0x839;
const DIVIDE_CNT_OFFSET: u32 = 0x83e;
const SPURIOUS_ENTRY_OFFSET: u32 = 0x80f;
const APIC_ICR_OFFSET:  u32 = 0x830;

const DELIVERY_INIT: u32 = 0b101 << 8;
const DELIVERY_SIPI: u32 = 0b110 << 8;

const TRIGGER_LEVEL: u32 = 1 << 15;
const LEVEL_ASSERT: u32 = 1 << 14;


static mut X2APIC_ENABLED: bool = false;
static mut APIC_BASE: usize = 0;

fn lapic_mmio_offset(msr: u32) -> usize {
    ((msr - 0x800) << 4) as usize
}

fn lapic_read(offset: u32) -> u64 {
    unsafe {
        if X2APIC_ENABLED {
            rdmsr(offset)
        } else {
            let mmio_base = get_apic_mmio_base();
            core::ptr::read_volatile((mmio_base + lapic_mmio_offset(offset) as usize) as *const u32) as u64
        }
    }
}

fn lapic_write(offset: u32, value: u64) {
    unsafe {
        if X2APIC_ENABLED {
            wrmsr(offset, value);
        } else {
            let mmio_base = get_apic_mmio_base();
            core::ptr::write_volatile((mmio_base + lapic_mmio_offset(offset) as usize) as *mut u32, value as u32);
        }
    }
}

fn lapic_icr_write(icr_high: u32, icr_low: u32) {
    unsafe {
        if X2APIC_ENABLED {
            // In x2APIC mode, ICR is a single 64-bit MSR
            let value = ((icr_high as u64) << 32) | (icr_low as u64);
            wrmsr(APIC_ICR_OFFSET, value);
        } else {
            let mmio_base = get_apic_mmio_base();
            let stat = disable_interrupts();
            core::ptr::write_volatile((mmio_base + lapic_mmio_offset(APIC_ICR_OFFSET) + 0x10) as *mut u32, icr_high);
            core::ptr::write_volatile((mmio_base + lapic_mmio_offset(APIC_ICR_OFFSET)) as *mut u32, icr_low);
            enable_interrupts(stat);
        }
    }
}

fn lapic_icr_read() -> (u32, u32) {
    unsafe {
        if X2APIC_ENABLED {
            let value = rdmsr(APIC_ICR_OFFSET);
            ((value >> 32) as u32, value as u32)
        } else {
            let mmio_base = get_apic_mmio_base();
            let stat = disable_interrupts();
            let high = core::ptr::read_volatile((mmio_base + lapic_mmio_offset(APIC_ICR_OFFSET) + 0x10) as *const u32);
            let mut low = core::ptr::read_volatile((mmio_base + lapic_mmio_offset(APIC_ICR_OFFSET)) as *const u32);

            // Wait for delivery status pending bit to clear 
            while low & (1 << 12) != 0 {
                core::hint::spin_loop();
                low = core::ptr::read_volatile((mmio_base + lapic_mmio_offset(APIC_ICR_OFFSET)) as *const u32);
            }

            enable_interrupts(stat);
            (high, low)
        }
    }
}

fn get_apic_mmio_base() -> usize {
    unsafe {
        APIC_BASE
    }
}

pub fn enable_x2apic() {
    unsafe {
        X2APIC_ENABLED = true;
    }
}

#[cfg(debug_assertions)]
pub fn get_apic_base() -> usize {
    let apic_base = unsafe {
        rdmsr(APIC_BASE_OFFSET)
    };

    (apic_base & 0xfffff000) as usize
}

pub fn init() {
    let apic_base = unsafe {
        rdmsr(APIC_BASE_OFFSET)
    };

    let apic_base_addr = apic_base & 0xfffff000;
    let is_bsp = ((apic_base >> 8) & 0x1) != 0;

    if unsafe {X2APIC_ENABLED} {
        // Enable APIC + x2APIC mode
        unsafe {
            wrmsr(APIC_BASE_OFFSET, apic_base | (0x3 << 10));
        }
    } else {
        // Legacy xAPIC mode
        unsafe {
            wrmsr(APIC_BASE_OFFSET, apic_base | (0x1 << 11));
        }

        // Map the APIC register space
        // Here, we're making the assumption that every AP will have the same APIC_BASE_ADDRESS as BSP
        if is_bsp && unsafe {!X2APIC_ENABLED} {
            let base = allocate_memory(Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap(), 
            PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC)
            .expect("Virtual memory allocation failed for APIC register space");

            map_memory(apic_base_addr as usize, base as usize, PAGE_SIZE, PageDescriptor::MMIO)
            .expect("map_memory failed for apic register space");
            
            unsafe {
                APIC_BASE = base as usize; 
            }
        }
    }

    // Allow all interrupts
    lapic_write(TASK_REG_OFFSET, 0);

    // Mask THERMAL, PERF, LINT0/1 LVT entries
    for &addr in &[THERMAL_LVT, PERF_CNTR_LVT, LINT0_LVT, LINT1_LVT] {
        let lvt = lapic_read(addr);
        lapic_write(addr, (lvt | (1 << 16)) & 0xffffffff);
    }

    // Setup the error table vector entry
    lapic_write(ERROR_LVT, (ERROR_VECTOR & 0xff) as u64);

    // Setup spurious vector entry
    lapic_write(SPURIOUS_ENTRY_OFFSET, (1 << 8) | (SPURIOUS_VECTOR & 0xff) as u64);
}

pub fn get_lapic_id() -> usize {
    let id = lapic_read(APIC_ID_OFFSET);
    if unsafe {X2APIC_ENABLED} {
        id as usize
    }
    else {
        ((id >> 24) & 0xff) as usize
    }
}

pub fn eoi() {
    lapic_write(APIC_EOI_OFFSET, 0);
}

pub fn get_error() -> u64 {
    // This write is required to get latest error status
    lapic_write(ERROR_STS_OFFSET, 0);
    
    lapic_read(ERROR_STS_OFFSET)
}

pub fn clear_error() {
    lapic_write(ERROR_STS_OFFSET, 0);
}

// This initial setup is required for measuring the timer frequency
pub fn init_timer() {
    // Setup timer in periodic mode. Masked initially
    lapic_write(TIMER_LVT, (1 << 17) | (1 << 16) | TIMER_VECTOR as u64);

    // Divide by 128 
    lapic_write(DIVIDE_CNT_OFFSET, 0b1010);
    lapic_write(INITIAL_CNT_OFFSET, 0xffffffff);
}

pub fn get_timer_value() -> u32 {
    lapic_read(CURRENT_CNT_OFFSET) as u32
}

// This is the setup we will use at scheduler level
pub fn setup_timer() {
    // Enable timer in one-shot mode. Keep interrupt masked
    lapic_write(TIMER_LVT, (1 << 16) | TIMER_VECTOR as u64);

    // Divide by 128 (Max divide factor)
    lapic_write(DIVIDE_CNT_OFFSET, 0b1010);
}

pub fn enable_timer(init_count: u32) {
    setup_timer_value(0);
    lapic_write(TIMER_LVT, TIMER_VECTOR as u64);
    setup_timer_value(init_count);
}

pub fn disable_timer() {
    lapic_write(TIMER_LVT, (1 << 16) | TIMER_VECTOR as u64);
    setup_timer_value(0);
}

pub fn setup_timer_value(init_count: u32) {
    lapic_write(INITIAL_CNT_OFFSET, init_count as u64);
}

pub fn configure_nmi(is_pin_0: bool, is_edge_triggered: bool) {
    let addr = if is_pin_0 {LINT0_LVT} else {LINT1_LVT};
    let tgm_mask = if is_edge_triggered {0} else {1u64 << 15};

    lapic_write(addr, tgm_mask | (0b100u64 << 8));    
}

pub fn lapic_wait_icr_idle() {
    if unsafe {!X2APIC_ENABLED} {
        while lapic_read(APIC_ICR_OFFSET) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
    }
}

fn create_apic_dw(apic_id: u32) -> u32 {
    if unsafe {X2APIC_ENABLED} {
        apic_id
    }
    else {
        apic_id << 24
    }
}

pub fn send_ipi(apic_id: u32, vector: u8) {
   lapic_icr_write(
create_apic_dw(apic_id),
    vector as u32); 
}

pub fn send_init_ipi(apic_id: u32) {
    lapic_icr_write(
        create_apic_dw(apic_id),
        DELIVERY_INIT |
        TRIGGER_LEVEL |
        LEVEL_ASSERT
    );
    
    lapic_wait_icr_idle();
}

pub fn send_init_deassert(apic_id: u32) {
    lapic_icr_write(
    create_apic_dw(apic_id),
        DELIVERY_INIT |
        TRIGGER_LEVEL 
    );
    
    lapic_wait_icr_idle();
}

pub fn send_sipi(apic_id: u32, vector: u8) {
    lapic_icr_write(
    create_apic_dw(apic_id),
        DELIVERY_SIPI |
        vector as u32 
    );
}