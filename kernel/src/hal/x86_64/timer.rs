use crate::cpu::{MAX_CPUS, PerCpu}; 
use crate::devices::HPET;
use crate::sched::QUANTUM;
use super::asm;
use super::get_core;
use kernel_intf::info;
use super::lapic;
use core::sync::atomic::{AtomicUsize, Ordering};

pub static BASE_COUNT: PerCpu<AtomicUsize> = PerCpu::new_with([const {AtomicUsize::new(0)}; MAX_CPUS]);
static TSC_FREQ_HZ: PerCpu<AtomicUsize> = PerCpu::new_with([const {AtomicUsize::new(0)}; MAX_CPUS]);
static EXPECTED_VISITOR: AtomicUsize = AtomicUsize::new(0);

// Smallest granularity timer
pub fn delay_ns(value: usize) {
    // This stops kernel from pre-empting, so only use for small infrequent delays
    // We use the platform HPET for this purpose
    let hpet = HPET.lock();
    
    // Convert the wait time required to femtoseconds
    let total_time= (value * 1_000_000) as u64;
    
    let mut current_time  = 0u64;
    let start_ticks = hpet.read_counter();

    while current_time < total_time {
        let cur_ticks = hpet.read_counter();

        current_time = cur_ticks.wrapping_sub(start_ticks) * hpet.clk_period as u64;
        core::hint::spin_loop();
    }
}


pub fn init() {
    let core = get_core();

    // Only 1 core should execute this calibration code at a time
    // This is because we're using the hpet shared timer to track the time
    // across all cores. This would cause core contention which will 
    // result in wrong timings 
    while EXPECTED_VISITOR.load(Ordering::Relaxed) != core {
        core::hint::spin_loop();
    }

    // Measure the CPU clock frequency
    let old = asm::rdtsc();

    //Let's wait for 1ms
    delay_ns(1_000_000);
    
    let new = asm::rdtsc();

    let num_ticks_passed = new.wrapping_sub(old);
    let base_freq = num_ticks_passed * 1000;
    TSC_FREQ_HZ.local().store(base_freq as usize, Ordering::Relaxed);

    info!("CPU Base Clock frequency measured as {}Hz", base_freq);
    
    // Now measure APIC timer
    lapic::init_timer();

    let old = lapic::get_timer_value();

    //Let's wait for 1ms
    delay_ns(1_000_000);

    let new = lapic::get_timer_value();
    // This is a countdown timer
    let num_ticks_passed = old.wrapping_sub(new) as u64;
    let apic_freq = num_ticks_passed * 1000;
    
    info!("CPU APIC Clock frequency measured as {}Hz", apic_freq);

    // Currently we use a divide factor of 128
    let init_count = (apic_freq as usize / 128) / (1000 / QUANTUM);
    assert!(init_count <= 0xffffffff);
    info!("Init count calculated as {:#X}", init_count);

    BASE_COUNT.local().store(init_count, Ordering::Relaxed);

    lapic::setup_timer();
    EXPECTED_VISITOR.fetch_add(1, Ordering::Relaxed);
}

pub fn get_time_ms() -> u64 {
    let freq = TSC_FREQ_HZ.local().load(Ordering::Relaxed) as u64;
    if freq == 0 {
        return 0;
    }
    asm::rdtsc() * 1000 / freq
}
